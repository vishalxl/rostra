//! Non-activatable pending admission intent, not a reservation or a worker.
//!
//! The DB writer excludes logical additions/config changes. `demands`
//! arbitrates cancellation and logical lease drops through the destructive
//! reducer; lock it before `state`, and release `state` before entering
//! reducers. Commit hooks run after the demand guard is released. Buffer-only
//! changes need only `state`. Cancellation linearizes at removal under
//! `demands`: a concurrent prune either observes cancellation or finishes its
//! synchronous reducer first.

#[cfg(test)]
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;

use rostra_core::event::{EventExt as _, EventKind, VerifiedEvent};
use rostra_core::id::ToShort as _;
use rostra_core::{EventId, Timestamp};

use crate::payload_demand_request::DemandRequest;
use crate::payload_demand_scan::DemandScan;
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

impl PayloadDemand {
    /// Check this exact nonrenewable owner, expiring metadata under
    /// arbitration.
    pub(crate) fn is_live(&self) -> bool {
        let mut demands = self.0.ledger.demands.lock().unwrap();
        demands.expire(Timestamp::now());
        demands
            .entries
            .get(&self.0.event)
            .is_some_and(|e| e.id == self.0.id)
    }
}

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
    /// A ranked demand fits now; do not evict for aggregate lower demand
    /// while that caller has yet to acquire its reservation.
    Fits(EventId),
    /// Eligible lower-ranked candidates cannot relieve the selected pressure.
    /// A caller must sleep/reconcile, not immediately retry the same minimum.
    NoVictim {
        /// Earliest demand expiry or skipped future row becoming eligible.
        /// Notifications can warrant an earlier retry; walltime is not latched.
        retry_at: Timestamp,
    },
    /// A bounded frontier advanced; yield before continuing. No eviction.
    Continue,
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
    /// A current eligible retained minimum outranked a Missing demand under
    /// retained-only pressure. No logical retained bytes were released.
    Rejected {
        /// Incoming event whose ordinary retry scheduling is now terminal.
        event: EventId,
    },
}

impl Database {
    /// Rederive a safe Missing rank from durable verified header/origins.
    pub(crate) fn demand_event_tx(
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
                    scan: None,
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
    /// low-water hysteresis, a worker or DryRun. A fresh eligible index minimum
    /// can instead authorize durable rejection of a lower-ranked Missing
    /// demand.
    pub(crate) async fn preempt_payload_demand(
        &self,
        request: DemandRequest<'_>,
    ) -> DbResult<DemandStep> {
        self.preempt_payload_demand_bound_with(request, Timestamp::now, || Ok(()))
            .await
    }

    /// Same operation with a synchronous boundary hook for deterministic
    /// cancellation/rollback tests; never exposed outside this crate.
    #[cfg(test)]
    pub(crate) async fn preempt_payload_demand_with(
        &self,
        generation: RetentionGeneration,
        clock: impl FnOnce() -> Timestamp,
        scan_limit: NonZeroUsize,
        max_bytes: u64,
        deadline: Instant,
        before_prune: impl FnOnce() -> DbResult<()>,
    ) -> DbResult<DemandStep> {
        let config = self
            .payload_admission
            .state
            .lock()
            .unwrap()
            .config
            .as_ref()
            .map(|config| config.identity())
            .unwrap_or_default();
        self.preempt_payload_demand_bound_with(
            DemandRequest {
                generation,
                config: &config,
                scan_limit,
                max_bytes,
                deadline,
            },
            clock,
            before_prune,
        )
        .await
    }

    /// Bind expected runtime authority inside the same writer as reduction.
    pub(crate) async fn preempt_payload_demand_bound_with(
        &self,
        request: DemandRequest<'_>,
        clock: impl FnOnce() -> Timestamp,
        before_prune: impl FnOnce() -> DbResult<()>,
    ) -> DbResult<DemandStep> {
        let DemandRequest {
            generation,
            config,
            scan_limit,
            max_bytes,
            deadline,
        } = request;
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
            if state
                .config
                .as_ref()
                .is_some_and(|current| !config.ptr_eq(&current.identity()))
            {
                // An old runner must not prune for, or cancel, a new runtime's
                // otherwise valid demand after a config replacement.
                return Ok(DemandStep::NotReady);
            }
            if demands.generation != Some(generation)
                || state
                    .config
                    .as_ref()
                    .is_none_or(|config| !demands.config.ptr_eq(&config.identity()))
            {
                demands.clear();
                return Ok(DemandStep::NotReady);
            }
            let mut plans = Vec::new();
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
                plans.push((event, key));
            }
            for id in stale {
                demands.entries.remove(&id);
            }
            if plans.is_empty() {
                demands.active = None;
                return Ok(DemandStep::Idle);
            }
            if demands.active.is_some_and(|(event, id)| {
                demands
                    .entries
                    .get(&event)
                    .is_none_or(|entry| entry.id != id)
            }) {
                demands.active = None;
            }
            plans.sort_unstable_by_key(|plan| std::cmp::Reverse(plan.1));
            // A partially serviced plan owns further eviction until its caller
            // reserves, cancels or expires. This is not an aggregate promise.
            if let Some((active, _)) = demands.active {
                plans.retain(|(event, _)| event.event_id == active);
            }
            // Even a previously exhausted plan may now fit after reservation
            // release. Check all applicable plans before spending for any.
            let mut pressures = Vec::with_capacity(plans.len());
            for (incoming, _) in &plans {
                if Instant::now() >= deadline {
                    return Ok(DemandStep::Bounded);
                }
                pressures.push(
                    match Self::payload_capacity_pause_tx(tx, incoming, &state)? {
                        None => return Ok(DemandStep::Fits(incoming.event_id)),
                        Some(PayloadAdmissionPause::AuthorCapacity) => Some(incoming.author()),
                        Some(PayloadAdmissionPause::DatabaseCapacity) => None,
                        _ => return Ok(DemandStep::NotReady),
                    },
                );
            }
            drop(state);
            let revision = self
                .payload_admission
                .retention_revision
                .load(std::sync::atomic::Ordering::Relaxed);
            let policy =
                rostra_core::retention::RetentionPolicy::from_bytes(generation.policy_bytes())
                    .ok_or(DbError::PayloadAccountingInvariant)?;
            let mut remaining = scan_limit.get();
            let mut advanced = false;
            let mut byte_blocked = false;
            let mut retry_at = demands
                .entries
                .values()
                .map(|entry| entry.expires)
                .min()
                .ok_or(DbError::PayloadAccountingInvariant)?;
            let rejection_barred = demands.active.is_some();
            for ((incoming, key), author) in plans.into_iter().zip(pressures) {
                if Instant::now() >= deadline {
                    return Ok(if advanced {
                        DemandStep::Continue
                    } else {
                        DemandStep::Bounded
                    });
                }
                let scan = demands
                    .entries
                    .get_mut(&incoming.event_id)
                    .ok_or(DbError::PayloadAccountingInvariant)?
                    .scan
                    .get_or_insert_with(|| DemandScan::new(revision, author, now));
                scan.prepare(revision, author, now);
                if let Some(retry) = scan.retry_at {
                    retry_at = retry_at.min(retry);
                }
                if scan.exhausted {
                    continue;
                }
                if scan.blocked_bytes.is_some_and(|bytes| bytes > max_bytes) {
                    byte_blocked = true;
                    continue;
                }
                loop {
                    if Instant::now() >= deadline {
                        return Ok(if advanced {
                            DemandStep::Continue
                        } else {
                            DemandStep::Bounded
                        });
                    }
                    if remaining == 0 {
                        return Ok(DemandStep::Continue);
                    }
                    let Some((victim_key, id)) = scan.next_tx(tx)? else {
                        scan.exhausted = true;
                        advanced = true;
                        break;
                    };
                    remaining -= 1;
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
                    let current = self.retention_candidate_current_tx(
                        tx,
                        generation,
                        candidate,
                        RetentionClock::Trusted(now),
                    )?;
                    if !current
                        && let Some((entry, _)) =
                            self.derive_retention_entry_tx(tx, generation, id)?
                        && now.as_u64() < entry.eligible_at
                    {
                        let retry = Timestamp::from(entry.eligible_at);
                        scan.retry_at = Some(scan.retry_at.map_or(retry, |old| old.min(retry)));
                        retry_at = retry_at.min(retry);
                    }
                    if victim_key >= key {
                        // A resumed/skipped frontier is advice, not proof of the
                        // true minimum. Protected bytes alone establish no rank
                        // boundary, nor do temporary reservations.
                        if victim_key > key
                            && current
                            && scan.after.is_none()
                            && !rejection_barred
                            && self.retained_rejection_pressure_tx(tx, &incoming, author)?
                        {
                            before_prune()?;
                            return match self.prune_quota_payload_tx(
                                tx,
                                QuotaPruneRequest {
                                    id: incoming.event_id.to_short(),
                                    target: QuotaPruneTarget::Missing,
                                    reason: if author.is_some() {
                                        QuotaPruneReason::AuthorQuota
                                    } else {
                                        QuotaPruneReason::GlobalQuota
                                    },
                                    policy,
                                    clock: RetentionClock::Trusted(now),
                                },
                            )? {
                                QuotaPruneOutcome::Pruned {
                                    logical_released_bytes: 0,
                                } => Ok(DemandStep::Rejected {
                                    event: incoming.event_id,
                                }),
                                _ => Err(DbError::PayloadAccountingInvariant),
                            };
                        }
                        scan.exhausted = true;
                        advanced = true;
                        break;
                    }
                    if !current {
                        scan.after = Some(victim_key);
                        advanced = true;
                        continue;
                    }
                    if u64::from(event.content_len()) > max_bytes {
                        scan.blocked_bytes = Some(u64::from(event.content_len()));
                        byte_blocked = true;
                        break;
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
                        } => {
                            let demand = incoming.event_id;
                            // Publish before commit releases the writer, not in
                            // a hook which a subsequent writer could overtake.
                            // A commit failure can conservatively retain this
                            // barrier, never authorize an unchecked reduction.
                            let id = demands
                                .entries
                                .get(&demand)
                                .ok_or(DbError::PayloadAccountingInvariant)?
                                .id;
                            demands.active = Some((demand, id));
                            Ok(DemandStep::Pruned {
                                demand,
                                victim: event.event_id,
                                bytes: logical_released_bytes,
                            })
                        }
                        _ => Err(DbError::PayloadAccountingInvariant),
                    };
                }
            }
            Ok(if byte_blocked {
                DemandStep::Bounded
            } else {
                DemandStep::NoVictim { retry_at }
            })
        })
        .await
    }

    /// Check retained-only pressure while the writer and demand arbitration
    /// remain held. This is only one input to the fresh-minimum rank proof.
    fn retained_rejection_pressure_tx(
        &self,
        tx: &WriteTransactionCtx,
        incoming: &VerifiedEvent,
        author: Option<rostra_core::id::RostraId>,
    ) -> DbResult<bool> {
        let state = self.payload_admission.state.lock().unwrap();
        let Some(config) = &state.config else {
            return Ok(false);
        };
        if state.events.contains_key(&incoming.event_id) {
            return Ok(false);
        }
        let (retained, high) = if let Some(author) = author {
            (
                tx.open_table(&crate::ids_data_usage::TABLE)?
                    .get(&author)?
                    .map(|row| row.value().current_content_size)
                    .unwrap_or(0),
                config.author_bytes(author),
            )
        } else {
            (
                Self::payload_usage_tx(tx)?
                    .ok_or(DbError::PayloadAccountingInvariant)?
                    .logical_current_bytes,
                config.database_bytes(),
            )
        };
        Ok(retained
            .checked_add(u64::from(incoming.content_len()))
            .is_some_and(|total| total > high))
    }
}
