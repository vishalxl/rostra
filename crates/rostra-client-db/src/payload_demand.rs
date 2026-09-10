//! Non-activatable pending admission intent, not a reservation or a worker.
//!
//! The DB writer excludes logical additions/config changes. `demands`
//! arbitrates cancellation and logical lease drops through the destructive
//! reducer; lock it before `state`, and release `state` before entering
//! reducers. Commit hooks run after the demand guard is released. Buffer-only
//! changes need only `state`. Cancellation linearizes at removal under
//! `demands`: a concurrent prune either observes cancellation or finishes its
//! synchronous reducer first.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;

use rostra_core::event::{EventExt as _, EventKind, VerifiedEvent};
use rostra_core::id::ToShort as _;
use rostra_core::{EventId, Timestamp};

use crate::payload_demand_state::DemandEntry;
use crate::payload_reservation::AdmissionLedger;
use crate::{
    Database, DbError, DbResult, PayloadAdmissionPause, QuotaPruneOutcome, QuotaPruneReason,
    QuotaPruneRequest, QuotaPruneTarget, RetentionCandidate, RetentionClock, RetentionGeneration,
    WriteTransactionCtx,
};

/// Short-lived interest in one verified Missing payload, owning no payload
/// bytes.
///
/// Clones share one deduplicated intent and its original, nonrenewable
/// deadline. Expiry, completion, reattachment or policy/config replacement
/// invalidate all clones. A later lease has a different ID, so dropping an
/// obsolete owner is safe.
#[derive(Debug, Clone)]
pub(crate) struct PayloadDemand(Arc<DemandOwner>);

/// Unique cancellation owner, weakly referenced by the ledger to avoid a cycle.
#[derive(Debug)]
pub(crate) struct DemandOwner {
    /// Shared account ownership, including cancellation arbitration.
    ledger: Arc<AdmissionLedger>,
    /// Full event identity, never a shortened rank tie.
    event: EventId,
    /// Monotonic identity within the ledger.
    id: u64,
}

impl Drop for DemandOwner {
    fn drop(&mut self) {
        let mut demands = self.ledger.demands.lock().unwrap();
        if demands
            .entries
            .get(&self.event)
            .is_some_and(|e| e.id == self.id)
        {
            demands.entries.remove(&self.event);
        }
        drop(demands);
        self.ledger.changed.notify_waiters();
    }
}

/// Registration is deliberately separate from ordinary admission until runtime
/// integration supplies ownership across paused acquisition paths.
#[derive(Debug)]
pub(crate) enum DemandRegistration {
    /// Config absent, index incomplete/stale, ineligible, no pressure, or
    /// logical reservation already present: this request establishes no
    /// demand.
    Unneeded,
    /// Count/intent-byte bound, impossible per-event ceiling, or timestamp
    /// overflow prevents bounded registration. This is not durable rejection.
    Overloaded,
    /// Caller must keep this metadata-only owner alive while waiting.
    Pending(PayloadDemand),
}

/// One internal, bounded preemption result. None of these schedules a worker.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DemandStep {
    /// No live eligible demand remains.
    Idle,
    /// Active config/generation, accounting, or fixed-time due prefix is not
    /// ready. Promotion's backfill-ready flag alone is insufficient.
    NotReady,
    /// Highest ranked demand fits now; do not evict for aggregate lower demand
    /// while that caller has yet to acquire its reservation.
    Fits(EventId),
    /// Eligible lower-ranked candidates cannot relieve the selected pressure.
    /// A caller must sleep/reconcile, not immediately retry the same minimum.
    NoVictim,
    /// Row, logical-byte or cooperative time bound prevented further work.
    /// One DB operation/reducer is indivisible and may exceed the time bound.
    Bounded,
    /// Exactly one checked Processed transition, with no payload collection.
    Pruned {
        /// Live incoming intent that authorized preemption.
        demand: EventId,
        /// Lower-ranked current candidate dematerialized.
        victim: EventId,
        /// Logical event bytes, not unique or physical reclaimed bytes.
        bytes: u64,
    },
}

impl Database {
    /// Rederive a safe Missing rank from durable verified header/origins.
    fn demand_event_tx(
        &self,
        tx: &WriteTransactionCtx,
        id: EventId,
        generation: RetentionGeneration,
        now: Timestamp,
    ) -> DbResult<Option<(VerifiedEvent, [u8; 48])>> {
        if generation.holder() != self.self_id || !Self::payload_is_missing_tx(tx, id.to_short())? {
            return Ok(None);
        }
        let Some(event) = tx
            .open_table(&crate::events::TABLE)?
            .get(&id.to_short())?
            .map(|r| r.value_try())
            .transpose()?
        else {
            return Ok(None);
        };
        let verified = VerifiedEvent::assume_verified_from_signed(event.signed);
        if verified.event_id != id
            || verified.author() == self.self_id
            || verified.kind() != EventKind::SOCIAL_POST
            || verified.content_len() == 0
        {
            return Ok(None);
        }
        let Some(origin) = tx
            .open_table(&crate::events_retention_origins::TABLE)?
            .get(&id.to_short())?
            .map(|r| r.value_try())
            .transpose()?
        else {
            return Ok(None);
        };
        if origin.materialized_at.is_some() || now < origin.effective_timestamp {
            return Ok(None);
        }
        let policy = rostra_core::retention::RetentionPolicy::from_bytes(generation.policy_bytes())
            .ok_or(DbError::PayloadAccountingInvariant)?;
        let key = policy
            .key(
                id,
                self.self_id,
                verified.content_len(),
                origin.effective_timestamp,
            )
            .to_bytes();
        Ok(Some((verified, key)))
    }

    /// Register at most 30 seconds of demand after a failed logical
    /// reservation.
    ///
    /// Requires an existing verified Missing header; never creates accounts,
    /// retains a payload buffer, prunes, or promises capacity. Distinct count
    /// and bytes independently reuse the explicit acquisition count/byte
    /// limits as intent bounds (not as a charge against buffers or reserved
    /// logical bytes). This crate-private foundation has no production
    /// enabled caller.
    pub(crate) async fn register_payload_demand(
        &self,
        event: EventId,
        generation: RetentionGeneration,
    ) -> DbResult<DemandRegistration> {
        self.register_payload_demand_with(event, generation, Timestamp::now)
            .await
    }

    /// Inject a clock for deterministic tests, sampled only after both writer
    /// and cancellation arbitration are acquired.
    pub(crate) async fn register_payload_demand_with(
        &self,
        event: EventId,
        generation: RetentionGeneration,
        clock: impl FnOnce() -> Timestamp,
    ) -> DbResult<DemandRegistration> {
        self.write_with(|tx| {
            let mut demands = self.payload_admission.demands.lock().unwrap();
            let now = clock();
            if !Self::retention_selection_ready_tx(tx, generation, now)? {
                return Ok(DemandRegistration::Unneeded);
            }
            let Some((verified, _)) = self.demand_event_tx(tx, event, generation, now)? else {
                return Ok(DemandRegistration::Unneeded);
            };
            let state = self.payload_admission.state.lock().unwrap();
            let Some(config) = &state.config else {
                return Ok(DemandRegistration::Unneeded);
            };
            if demands.generation != Some(generation) || !demands.config.ptr_eq(&config.identity())
            {
                demands.clear();
                demands.generation = Some(generation);
                demands.config = config.identity();
            }
            demands.expire(now);
            if state.events.contains_key(&event)
                || !matches!(
                    Self::payload_capacity_pause_tx(tx, &verified, &state)?,
                    Some(
                        PayloadAdmissionPause::AuthorCapacity
                            | PayloadAdmissionPause::DatabaseCapacity
                    )
                )
            {
                return Ok(DemandRegistration::Unneeded);
            }
            if let Some(owner) = demands.entries.get(&event).and_then(|e| e.owner.upgrade()) {
                return Ok(DemandRegistration::Pending(PayloadDemand(owner)));
            }
            let bytes = u64::from(verified.content_len());
            let (count, intent_bytes) = demands.usage();
            if count >= config.in_flight_count()
                || intent_bytes
                    .checked_add(bytes)
                    .is_none_or(|n| n > config.in_flight_bytes())
                || bytes > config.database_bytes()
                || bytes > config.author_bytes(verified.author())
            {
                return Ok(DemandRegistration::Overloaded);
            }
            let Some(expires) = now.as_u64().checked_add(30).map(Timestamp::from) else {
                return Ok(DemandRegistration::Overloaded);
            };
            let id = demands.next_id.checked_add(1).ok_or(DbError::Overflow)?;
            demands.next_id = id;
            let owner = Arc::new(DemandOwner {
                ledger: self.payload_admission.clone(),
                event,
                id,
            });
            demands.entries.insert(
                event,
                DemandEntry {
                    owner: Arc::downgrade(&owner),
                    id,
                    bytes,
                    created: now,
                    expires,
                },
            );
            drop(state);
            drop(demands);
            self.payload_admission.changed.notify_waiters();
            Ok(DemandRegistration::Pending(PayloadDemand(owner)))
        })
        .await
    }

    /// Preempt at most one lower-ranked retained payload for one live demand.
    ///
    /// Rechecks all authority inside the writer transaction. Demand arbitration
    /// prevents cancellation/logical release races until the reducer returns.
    /// Config remains absent in production. This is not general quota pressure,
    /// low-water hysteresis, permanent Missing rejection, a worker or DryRun.
    pub(crate) async fn preempt_payload_demand(
        &self,
        generation: RetentionGeneration,
        scan_limit: NonZeroUsize,
        max_bytes: u64,
        deadline: Instant,
    ) -> DbResult<DemandStep> {
        self.preempt_payload_demand_with(
            generation,
            Timestamp::now,
            scan_limit,
            max_bytes,
            deadline,
            || Ok(()),
        )
        .await
    }

    /// Same operation with a synchronous boundary hook for deterministic
    /// cancellation/rollback tests; never exposed outside this crate.
    pub(crate) async fn preempt_payload_demand_with(
        &self,
        generation: RetentionGeneration,
        clock: impl FnOnce() -> Timestamp,
        scan_limit: NonZeroUsize,
        max_bytes: u64,
        deadline: Instant,
        before_prune: impl FnOnce() -> DbResult<()>,
    ) -> DbResult<DemandStep> {
        if scan_limit.get() > crate::PAYLOAD_MAINTENANCE_MAX {
            return crate::PayloadMaintenanceLimitSnafu.fail();
        }
        self.write_with(|tx| {
            let mut demands = self.payload_admission.demands.lock().unwrap();
            let now = clock();
            demands.expire(now);
            if demands.entries.is_empty() {
                return Ok(DemandStep::Idle);
            }
            if !Self::retention_selection_ready_tx(tx, generation, now)?
                || Self::payload_usage_tx(tx)?.is_none()
            {
                return Ok(DemandStep::NotReady);
            }
            let state = self.payload_admission.state.lock().unwrap();
            if demands.generation != Some(generation)
                || state
                    .config
                    .as_ref()
                    .is_none_or(|config| !demands.config.ptr_eq(&config.identity()))
            {
                demands.clear();
                return Ok(DemandStep::NotReady);
            }
            let mut selected = None;
            // The ledger is explicitly count-bounded; stop cooperatively between
            // rows rather than hiding an unbounded pending-header scan.
            let mut stale = Vec::new();
            for id in demands.entries.keys() {
                if Instant::now() >= deadline {
                    return Ok(DemandStep::Bounded);
                }
                if state.events.contains_key(id) {
                    stale.push(*id);
                    continue;
                }
                let Some((event, key)) = self.demand_event_tx(tx, *id, generation, now)? else {
                    stale.push(*id);
                    continue;
                };
                if selected.as_ref().is_none_or(|(_, old)| key > *old) {
                    selected = Some((event, key));
                }
            }
            for id in stale {
                demands.entries.remove(&id);
            }
            let Some((incoming, key)) = selected else {
                return Ok(DemandStep::Idle);
            };
            let pressure = Self::payload_capacity_pause_tx(tx, &incoming, &state)?;
            let author = match pressure {
                None => return Ok(DemandStep::Fits(incoming.event_id)),
                Some(PayloadAdmissionPause::AuthorCapacity) => Some(incoming.author()),
                Some(PayloadAdmissionPause::DatabaseCapacity) => None,
                _ => return Ok(DemandStep::NotReady),
            };
            drop(state);
            let policy =
                rostra_core::retention::RetentionPolicy::from_bytes(generation.policy_bytes())
                    .ok_or(DbError::PayloadAccountingInvariant)?;
            // The due prefix was checked in this same transaction. Visit the
            // ordered minimum, counting future rows after wallclock rollback.
            let rows = if let Some(author) = author {
                tx.open_table(&crate::content_retention_author::TABLE)?
                    .range((author, [0; 48])..=(author, [255; 48]))?
                    .take(scan_limit.get())
                    .map(|row| {
                        let (k, v) = row?;
                        Ok((k.value_try()?.1, v.value_try()?))
                    })
                    .collect::<DbResult<Vec<_>>>()?
            } else {
                tx.open_table(&crate::content_retention_global::TABLE)?
                    .range::<[u8; 48]>(..)?
                    .take(scan_limit.get())
                    .map(|row| {
                        let (k, v) = row?;
                        Ok((k.value_try()?, v.value_try()?))
                    })
                    .collect::<DbResult<Vec<_>>>()?
            };
            let exhausted = rows.len() < scan_limit.get();
            for (victim_key, id) in rows {
                if Instant::now() >= deadline {
                    return Ok(DemandStep::Bounded);
                }
                if victim_key >= key {
                    return Ok(DemandStep::NoVictim);
                }
                let event = tx
                    .open_table(&crate::events::TABLE)?
                    .get(&id)?
                    .ok_or(DbError::PayloadAccountingInvariant)?
                    .value_try()?;
                let event = VerifiedEvent::assume_verified_from_signed(event.signed);
                let candidate = RetentionCandidate {
                    event: event.event_id,
                    author: event.author(),
                    key: victim_key,
                };
                if !self.retention_candidate_current_tx(
                    tx,
                    generation,
                    candidate,
                    RetentionClock::Trusted(now),
                )? {
                    continue;
                }
                if u64::from(event.content_len()) > max_bytes {
                    return Ok(DemandStep::Bounded);
                }
                before_prune()?;
                let outcome = self.prune_quota_payload_tx(
                    tx,
                    QuotaPruneRequest {
                        id,
                        target: QuotaPruneTarget::Processed,
                        reason: if author.is_some() {
                            QuotaPruneReason::AuthorQuota
                        } else {
                            QuotaPruneReason::GlobalQuota
                        },
                        policy,
                        clock: RetentionClock::Trusted(now),
                    },
                )?;
                return match outcome {
                    QuotaPruneOutcome::Pruned {
                        logical_released_bytes,
                    } => Ok(DemandStep::Pruned {
                        demand: incoming.event_id,
                        victim: event.event_id,
                        bytes: logical_released_bytes,
                    }),
                    _ => Err(DbError::PayloadAccountingInvariant),
                };
            }
            Ok(if exhausted {
                DemandStep::NoVictim
            } else {
                DemandStep::Bounded
            })
        })
        .await
    }
}
