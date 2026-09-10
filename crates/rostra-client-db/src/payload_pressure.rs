//! Bounded general pressure. Durable rows are disposable advice scoped to one
//! runtime incarnation; only current writer-transaction checks authorize
//! pruning.

use std::ops::Bound::{Excluded, Unbounded};
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bincode::{Decode, Encode};
use rostra_core::Timestamp;
use rostra_core::event::{EventExt as _, VerifiedEvent};
use rostra_core::id::{RostraId, ToShort as _};

use crate::payload_demand_scan::DemandScan;
use crate::payload_demand_state::DemandState;
use crate::payload_reservation::AdmissionState;
use crate::{
    Database, DbError, DbResult, PayloadAdmissionConfig, QuotaPruneOutcome, QuotaPruneReason,
    QuotaPruneRequest, QuotaPruneTarget, RetentionCandidate, RetentionClock, RetentionGeneration,
    WriteTransactionCtx,
};

/// Disposable singleton; a new runtime never inherits old hysteresis.
#[derive(Debug, Default, Clone, Copy, Encode, Decode)]
pub(crate) struct PressureRecord {
    /// Checked monotonic database-local runtime incarnation.
    epoch: u64,
    /// Triggered global target, surviving bounded turns and runner drops.
    global: bool,
}

/// At most one row per accounted author; no author-sized in-memory collection.
#[derive(Debug, Clone, Copy, Encode, Decode)]
pub(crate) struct AuthorPressure {
    /// Only the current runtime can use this row.
    epoch: u64,
    /// Triggered author target, even when eligible candidates are exhausted.
    active: bool,
    /// One exclusive candidate frontier, not a victim list.
    scan: Option<DemandScan>,
}

/// Constant-size scheduler continuation; never destructive authority.
#[derive(Debug, Default)]
pub(crate) struct PressureCursor {
    /// Last fully considered author.
    after: Option<RostraId>,
    /// Author whose candidate frontier is still being visited.
    selected: Option<RostraId>,
    /// A complete author pass must precede global selection.
    authors_done: bool,
    /// Logical mutation revision at the beginning of the author pass.
    revision: Option<u64>,
    /// Candidate changes can make a previously blocked author actionable.
    index_revision: Option<u64>,
    /// Earliest skipped author eligibility, including already-promoted rows.
    retry_at: Option<Timestamp>,
    /// Backwards walltime invalidates advice, never clock trust.
    observed: Option<Timestamp>,
    /// Global candidate continuation.
    global_scan: Option<DemandScan>,
}

/// Runtime identity persists across cancellation of its caller-owned runner.
#[derive(Debug, Default)]
pub(crate) struct PressureWorker {
    /// Zero until a counted initialization transaction commits.
    epoch: AtomicU64,
}

/// Immutable policy binding and per-operation cooperative allowances.
#[derive(Clone, Copy)]
pub(crate) struct PressureRequest<'a> {
    /// Exact storing-account/policy generation.
    pub(crate) generation: RetentionGeneration,
    /// Immutable admission-config incarnation.
    pub(crate) config: &'a Weak<()>,
    /// Remaining shared logical-byte allowance.
    pub(crate) max_bytes: u64,
    /// Shared cooperative turn deadline.
    pub(crate) deadline: Instant,
}

/// A single bounded general-pressure operation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PressureStep {
    /// A frontier advanced; yield before continuing.
    Continue,
    /// Exhausted, byte-blocked, demand-barred or mutation-invalidated; wait.
    Wait,
    /// Maintenance must reconcile fresh generation/grace readiness.
    NotReady,
    /// A checked transition committed, without a physical-reclaim promise.
    Pruned {
        /// Exact logical event bytes released.
        logical_released_bytes: u64,
    },
}

/// Result of one bounded author discovery operation.
enum Scope {
    /// The author pass just finished; global selection gets its own operation.
    Advanced,
    /// Continue this author's candidate frontier.
    Author(RostraId),
    /// A revision-stable author pass permits global selection.
    Global,
}

/// Transaction-local environment; clocks and locks are never reacquired here.
struct PressureTransaction<'a> {
    /// Database whose writer and demand arbitration are held.
    db: &'a Database,
    /// Current indivisible writer transaction.
    tx: &'a WriteTransactionCtx,
    /// Fresh trusted walltime sampled after both locks.
    now: Timestamp,
    /// Immutable runtime binding and remaining turn allowances.
    request: PressureRequest<'a>,
}

/// Candidate advice remains separate from the checked reducer outcome.
enum Selection {
    /// A rejected frontier row was visited.
    Advanced,
    /// No operation can progress with current candidates/allowances.
    Blocked,
    /// All checks permit submitting this request to the checked reducer.
    Prune(QuotaPruneRequest),
}

impl PressureWorker {
    /// Initialize one counted incarnation transaction, publishing only after
    /// commit. A fresh runtime also discards any supplied old cursor.
    async fn initialize(&self, db: &Database, cursor: &mut PressureCursor) -> DbResult<()> {
        let epoch = db
            .write_with(|tx| {
                let mut table = tx.open_table(&crate::content_pressure_state::TABLE)?;
                let old = table
                    .get(&())?
                    .map(|r| r.value_try())
                    .transpose()?
                    .unwrap_or_default();
                let epoch = old.epoch.checked_add(1).ok_or(DbError::Overflow)?;
                table.insert(
                    &(),
                    &PressureRecord {
                        epoch,
                        global: false,
                    },
                )?;
                Ok(epoch)
            })
            .await?;
        self.epoch.store(epoch, Ordering::Relaxed);
        *cursor = PressureCursor::default();
        Ok(())
    }

    /// Recheck all pressure and candidate authority in the reducer's
    /// transaction. At most one author and one candidate are visited;
    /// bounded intent/lease passes check time between entries.
    pub(crate) async fn step(
        &self,
        db: &Database,
        request: PressureRequest<'_>,
        cursor: &mut PressureCursor,
    ) -> DbResult<PressureStep> {
        self.step_with(db, request, cursor, Timestamp::now, || Ok(()))
            .await
    }

    /// Supply a fresh clock and pre-reducer hook for deterministic race tests.
    pub(crate) async fn step_with(
        &self,
        db: &Database,
        request: PressureRequest<'_>,
        cursor: &mut PressureCursor,
        clock: impl FnOnce() -> Timestamp,
        before_prune: impl FnOnce() -> DbResult<()>,
    ) -> DbResult<PressureStep> {
        if self.epoch.load(Ordering::Relaxed) == 0 {
            self.initialize(db, cursor).await?;
            return Ok(PressureStep::Continue);
        }
        db.write_with(|tx| {
            let mut demands = db.payload_admission.demands.lock().unwrap();
            let now = clock();
            demands.expire(now);
            let context = PressureTransaction {
                db,
                tx,
                now,
                request,
            };
            if !Database::retention_selection_ready_tx(tx, request.generation, now)? {
                return Ok(PressureStep::NotReady);
            }
            let state = db.payload_admission.state.lock().unwrap();
            context.check_config(&state)?;
            let record = context.load_record(self.epoch.load(Ordering::Relaxed))?;
            if context.demands_block(&demands, &state)? || !cursor.reconcile(db, now)? {
                return Ok(PressureStep::Wait);
            }
            let author = match cursor.next_scope(tx)? {
                Scope::Advanced => return Ok(PressureStep::Continue),
                Scope::Author(author) => Some(author),
                Scope::Global => None,
            };
            let Some(active) = context.scope_active(author, &state, record, cursor)? else {
                return Ok(PressureStep::Wait);
            };
            let mut scope = active;
            let selection = if scope.active {
                context.select(author, &mut scope)?
            } else {
                Selection::Blocked
            };
            let result = context.save_scope(author, scope, record, cursor, &selection)?;
            drop(state);
            if let Selection::Prune(request) = selection {
                before_prune()?;
                return match db.prune_quota_payload_tx(tx, request)? {
                    QuotaPruneOutcome::Pruned {
                        logical_released_bytes,
                    } => Ok(PressureStep::Pruned {
                        logical_released_bytes,
                    }),
                    _ => Err(DbError::PayloadAccountingInvariant),
                };
            }
            Ok(result)
        })
        .await
    }
}

impl PressureCursor {
    /// Reject invalidated sweep advice before selecting any further scope.
    fn reconcile(&mut self, db: &Database, now: Timestamp) -> DbResult<bool> {
        let revision = db
            .payload_admission
            .pressure_revision
            .load(Ordering::Relaxed);
        let index_revision = db
            .payload_admission
            .retention_revision
            .load(Ordering::Relaxed);
        if revision == u64::MAX {
            return Err(DbError::Overflow);
        }
        if self.revision.is_some_and(|old| old != revision)
            || self.index_revision.is_some_and(|old| old != index_revision)
            || self.retry_at.is_some_and(|retry| retry <= now)
            || self.observed.is_some_and(|old| now < old)
        {
            *self = Self::default();
            return Ok(false);
        }
        self.revision = Some(revision);
        self.index_revision = Some(index_revision);
        self.observed = Some(now);
        Ok(true)
    }

    /// Seek one accounted author, retaining an unfinished candidate scope.
    fn next_scope(&mut self, tx: &WriteTransactionCtx) -> DbResult<Scope> {
        if self.authors_done {
            return Ok(Scope::Global);
        }
        if let Some(author) = self.selected {
            return Ok(Scope::Author(author));
        }
        let next = tx
            .open_table(&crate::content_accounting_authors::TABLE)?
            .range((self.after.map_or(Unbounded, Excluded), Unbounded))?
            .next()
            .transpose()?
            .map(|(key, _)| key.value_try())
            .transpose()?;
        if let Some(author) = next {
            self.selected = Some(author);
            Ok(Scope::Author(author))
        } else {
            self.authors_done = true;
            Ok(Scope::Advanced)
        }
    }
}

impl PressureTransaction<'_> {
    fn check_config(&self, state: &AdmissionState) -> DbResult<()> {
        if state
            .config
            .as_ref()
            .is_none_or(|config| !self.request.config.ptr_eq(&config.identity()))
        {
            return Err(DbError::PayloadAccountingInvariant);
        }
        Database::require_payload_accounting_ready_tx(self.tx)
    }

    fn load_record(&self, epoch: u64) -> DbResult<PressureRecord> {
        let record = self
            .tx
            .open_table(&crate::content_pressure_state::TABLE)?
            .get(&())?
            .ok_or(DbError::PayloadAccountingInvariant)?
            .value_try()?;
        if record.epoch != epoch {
            return Err(DbError::PayloadAccountingInvariant);
        }
        Ok(record)
    }

    /// Fits and exact partial-plan event/lease ownership reserve no additional
    /// bytes, but bar spending room already freed for that acquisition.
    fn demands_block(&self, demands: &DemandState, state: &AdmissionState) -> DbResult<bool> {
        for (id, entry) in &demands.entries {
            if Instant::now() >= self.request.deadline {
                return Ok(true);
            }
            if demands.generation != Some(self.request.generation)
                || !demands.config.ptr_eq(self.request.config)
                || state.events.contains_key(id)
            {
                continue;
            }
            let Some((event, _)) =
                self.db
                    .demand_event_tx(self.tx, *id, self.request.generation, self.now)?
            else {
                continue;
            };
            if demands.active == Some((*id, entry.id))
                || Database::payload_capacity_pause_tx(self.tx, &event, state)?.is_none()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Rederive retained-plus-reserved pressure without trusting a saved latch.
    fn scope_active(
        &self,
        author: Option<RostraId>,
        state: &AdmissionState,
        record: PressureRecord,
        cursor: &PressureCursor,
    ) -> DbResult<Option<AuthorPressure>> {
        let mut reserved = 0u64;
        for lease in state.events.values() {
            if Instant::now() >= self.request.deadline {
                return Ok(None);
            }
            if author.is_none_or(|author| author == lease.author) {
                reserved = reserved.checked_add(lease.bytes).ok_or(DbError::Overflow)?;
            }
        }
        let retained = if let Some(author) = author {
            self.tx
                .open_table(&crate::content_accounting_authors::TABLE)?
                .get(&author)?
                .map(|r| r.value_try())
                .transpose()?
                .unwrap_or(0)
        } else {
            Database::payload_usage_tx(self.tx)?
                .ok_or(DbError::PayloadAccountingInvariant)?
                .logical_current_bytes
        };
        let total = retained.checked_add(reserved).ok_or(DbError::Overflow)?;
        let config = state
            .config
            .as_ref()
            .ok_or(DbError::PayloadAccountingInvariant)?;
        let high = author.map_or_else(
            || config.database_bytes(),
            |author| config.author_bytes(author),
        );
        let mut scope = if let Some(author) = author {
            self.tx
                .open_table(&crate::content_pressure_authors::TABLE)?
                .get(&author)?
                .map(|r| r.value_try())
                .transpose()?
                .filter(|r| r.epoch == record.epoch)
                .unwrap_or(AuthorPressure {
                    epoch: record.epoch,
                    active: false,
                    scan: None,
                })
        } else {
            AuthorPressure {
                epoch: record.epoch,
                active: record.global,
                scan: cursor.global_scan,
            }
        };
        scope.active = total > high
            || (scope.active && total > PayloadAdmissionConfig::experimental_low_water(high));
        Ok(Some(scope))
    }

    /// Visit one candidate, never bypassing an eligible oversized minimum.
    fn select(&self, author: Option<RostraId>, scope: &mut AuthorPressure) -> DbResult<Selection> {
        let revision = self
            .db
            .payload_admission
            .retention_revision
            .load(Ordering::Relaxed);
        let scan = scope
            .scan
            .get_or_insert_with(|| DemandScan::new(revision, author, self.now));
        scan.prepare(revision, author, self.now);
        if scan.exhausted
            || scan
                .blocked_bytes
                .is_some_and(|bytes| bytes > self.request.max_bytes)
            || Instant::now() >= self.request.deadline
        {
            return Ok(Selection::Blocked);
        }
        let Some((key, id)) = scan.next_tx(self.tx)? else {
            scan.exhausted = true;
            return Ok(Selection::Blocked);
        };
        let event = self
            .tx
            .open_table(&crate::events::TABLE)?
            .get(&id)?
            .ok_or(DbError::PayloadAccountingInvariant)?
            .value_try()?;
        let event = VerifiedEvent::assume_verified_from_signed(event.signed);
        let candidate = RetentionCandidate {
            event: event.event_id,
            author: event.author(),
            key,
        };
        if !self.db.retention_candidate_current_tx(
            self.tx,
            self.request.generation,
            candidate,
            RetentionClock::Trusted(self.now),
        )? {
            if let Some((entry, _)) =
                self.db
                    .derive_retention_entry_tx(self.tx, self.request.generation, id)?
                && self.now.as_u64() < entry.eligible_at
            {
                let retry = Timestamp::from(entry.eligible_at);
                scan.retry_at = Some(scan.retry_at.map_or(retry, |old| old.min(retry)));
            }
            scan.after = Some(key);
            return Ok(Selection::Advanced);
        }
        if u64::from(event.content_len()) > self.request.max_bytes {
            scan.blocked_bytes = Some(u64::from(event.content_len()));
            return Ok(Selection::Blocked);
        }
        Ok(Selection::Prune(QuotaPruneRequest {
            id: event.event_id.to_short(),
            target: QuotaPruneTarget::Processed,
            reason: if author.is_some() {
                QuotaPruneReason::AuthorQuota
            } else {
                QuotaPruneReason::GlobalQuota
            },
            policy: rostra_core::retention::RetentionPolicy::from_bytes(
                self.request.generation.policy_bytes(),
            )
            .ok_or(DbError::PayloadAccountingInvariant)?,
            clock: RetentionClock::Trusted(self.now),
        }))
    }

    /// Persist only advisory hysteresis/frontiers; inactive/stale author rows
    /// are lazily removed by the bounded accounted-author pass.
    fn save_scope(
        &self,
        author: Option<RostraId>,
        scope: AuthorPressure,
        mut record: PressureRecord,
        cursor: &mut PressureCursor,
        selection: &Selection,
    ) -> DbResult<PressureStep> {
        if let Some(author) = author {
            if scope.active {
                if let Some(retry) = scope.scan.and_then(|scan| scan.retry_at) {
                    cursor.retry_at = Some(cursor.retry_at.map_or(retry, |old| old.min(retry)));
                }
                self.tx
                    .open_table(&crate::content_pressure_authors::TABLE)?
                    .insert(&author, &scope)?;
            } else {
                self.tx
                    .open_table(&crate::content_pressure_authors::TABLE)?
                    .remove(&author)?;
            }
            if matches!(selection, Selection::Blocked) {
                cursor.after = Some(author);
                cursor.selected = None;
            }
            return Ok(PressureStep::Continue);
        }
        record.global = scope.active;
        self.tx
            .open_table(&crate::content_pressure_state::TABLE)?
            .insert(&(), &record)?;
        cursor.global_scan = scope.scan;
        if !scope.active {
            *cursor = PressureCursor::default();
        } else if matches!(selection, Selection::Blocked) {
            cursor.after = None;
            cursor.selected = None;
            cursor.authors_done = false;
            cursor.revision = None;
        }
        Ok(if matches!(selection, Selection::Advanced) {
            PressureStep::Continue
        } else {
            PressureStep::Wait
        })
    }
}
