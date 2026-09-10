//! Bounded whole-source snapshots, not an incremental Enforce simulator.
//!
//! No transaction here can write. Incomplete snapshots discard their
//! projection; repeated complete snapshots replace totals rather than report
//! fresh work.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rostra_core::event::{EventExt as _, EventKind, VerifiedEvent};
use rostra_core::id::{RostraId, ToShort as _};
use rostra_core::retention::RetentionPolicy;
use rostra_core::{EventId, Timestamp};

use crate::payload_accounting::AccountingStage;
use crate::{
    Database, DbError, DbResult, PayloadAdmissionConfig, PayloadUsage, QuotaPruneReason,
    RetentionGeneration,
};

/// Explicit whole-snapshot bounds; no continuation can enlarge this model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DryRunLimits {
    /// All visited headers, including terminal and Missing events.
    pub(crate) events: NonZeroUsize,
    /// Distinct retained authors kept in bounded model memory.
    pub(crate) authors: NonZeroUsize,
    /// Sum of retained signed lengths admitted to the model, not allocated
    /// bytes.
    pub(crate) logical_bytes: u64,
    /// Cooperative allowance including reading, ranking and projection.
    pub(crate) time: Duration,
}

/// Why this replacement does or does not contain a complete projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DryRunStatus {
    /// All source headers fit and accounted retained totals agree.
    Complete,
    /// Accounting is unready; DryRun does not rebuild it.
    AccountingNotReady,
    /// At least one header did not fit the event bound.
    EventLimit,
    /// Another retained author did not fit the author bound.
    AuthorLimit,
    /// Retained logical bytes exceeded the whole-model allowance.
    LogicalByteLimit,
    /// Cooperative deadline elapsed; no prefix projection is published.
    TimeLimit,
}

/// A retained-only, fresh-start pressure projection, never actual releases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DryRunProjection {
    /// Effective forecast ceilings for every retained author in this snapshot;
    /// bounded by the author limit, not by the configured override map.
    pub(crate) author_high_waters: BTreeMap<RostraId, u64>,
    /// Logical victims in model order; never newly pruned events.
    pub(crate) victims: Vec<(EventId, QuotaPruneReason)>,
    /// Projected logical bytes removed, charging shared hashes per event.
    pub(crate) logical_victim_bytes: u64,
    /// Projected retained bytes after author-first/global 90% targets.
    pub(crate) logical_remaining_bytes: u64,
    /// Observed retained bytes excluded by kind, identity, origins or grace.
    pub(crate) protected_logical_bytes: u64,
    /// Triggered author targets still unmet by eligible retained content.
    pub(crate) unmet_authors: usize,
    /// Whether a triggered global low-water target remains unmet.
    pub(crate) unmet_global: bool,
}

/// One immutable as-of observation, not a current-state or reclamation promise.
///
/// It can become stale immediately after its read transaction. Each new attempt
/// replaces the entire report, including replacing a complete result with an
/// incomplete one. There are no cumulative or "new victim" counters.
/// Reservations, demand, future arrivals and pre-existing Enforce hysteresis
/// are NOT simulated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DryRunReport {
    /// Fixed trusted walltime sampled after opening the read transaction.
    pub(crate) as_of: Timestamp,
    /// Immutable forecast policy and storing identity, independent of indexes.
    pub(crate) generation: RetentionGeneration,
    /// Allowances that can make a large database permanently incomplete.
    pub(crate) limits: DryRunLimits,
    /// Forecast global high water; the target is floor(90% of this value).
    pub(crate) database_high_water: u64,
    /// Completeness or the first exhausted allowance.
    pub(crate) status: DryRunStatus,
    /// Successfully visited source rows, at most the event allowance.
    pub(crate) visited: usize,
    /// Exact persisted accounting at this snapshot, absent while unready.
    pub(crate) observed_usage: Option<PayloadUsage>,
    /// Guarded ledger counters sampled before the read transaction. Disabled
    /// means zero ledger ownership, NOT zero actual buffers or aggregate RAM.
    /// Reservations and demand are not part of the projected workload.
    pub(crate) observed_guarded_admission: crate::PayloadAdmissionUsage,
    /// Only complete snapshots contain projections.
    pub(crate) projection: Option<DryRunProjection>,
}

/// Immutable observation-only configuration and one bounded replacement report.
#[derive(Debug)]
pub(crate) struct DryRun {
    /// Forecast ceilings; never installed in the enforcing admission ledger.
    config: PayloadAdmissionConfig,
    /// Explicit count, logical-byte and cooperative-time bounds.
    limits: DryRunLimits,
    /// One report and pacing timestamp, not a growing history or victim set.
    state: Mutex<DryRunState>,
    /// A long indivisible operation cannot overlap another direct turn.
    attempting: AtomicBool,
}

/// Cancellation/panic-safe ownership; no model-state lock crosses an await.
struct Attempt<'a>(&'a AtomicBool);

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Report publication and minimum attempt spacing share one short-lived lock.
#[derive(Debug, Default)]
struct DryRunState {
    /// Start of the most recent attempt, including incomplete attempts.
    attempted: Option<Instant>,
    /// Last replacement; always as-of, never asserted current.
    report: Option<DryRunReport>,
}

/// Constant-sized per-author state, with count bounded by DryRunLimits.
#[derive(Debug)]
struct Author {
    /// Remaining projected logical bytes.
    bytes: u64,
    /// Immutable forecast ceiling including explicit overrides.
    high: u64,
    /// Fresh-snapshot trigger, not a retained Enforce latch.
    triggered: bool,
}

/// Header-only eligible victim; no content-store values are loaded.
#[derive(Debug)]
struct Candidate {
    /// Full deterministic policy key.
    key: [u8; 48],
    /// Full verified event identifier.
    event: EventId,
    /// Charged retained author.
    author: RostraId,
    /// Signed logical payload length.
    bytes: u64,
    /// Prevents selecting an author victim again in the global pass.
    removed: bool,
}

impl DryRun {
    /// Private fixture construction; no partial production activation surface.
    #[cfg(test)]
    pub(crate) fn new(config: PayloadAdmissionConfig, limits: DryRunLimits) -> Option<Self> {
        if limits.events.get() > crate::PAYLOAD_MAINTENANCE_MAX
            || limits.authors.get() > limits.events.get()
            || limits.logical_bytes == 0
            || limits.time.is_zero()
            || Instant::now().checked_add(limits.time).is_none()
        {
            return None;
        }
        Some(Self {
            config,
            limits,
            state: Mutex::default(),
            attempting: AtomicBool::new(false),
        })
    }

    /// Reject accidental coexistence with enforcing acquisition configuration.
    pub(crate) fn check_disabled(db: &Database) -> DbResult<()> {
        Self::with_disabled(db, || ())
    }

    /// Keep the ownership check valid through publication, without taking a
    /// database writer or changing admission metadata.
    fn with_disabled<T>(db: &Database, f: impl FnOnce() -> T) -> DbResult<T> {
        let demands = db.payload_admission.demands.lock().unwrap();
        let state = db.payload_admission.state.lock().unwrap();
        if state.config.is_some()
            || !state.events.is_empty()
            || state.buffers != 0
            || state.buffer_bytes != 0
            || demands.usage() != (0, 0)
        {
            return Err(DbError::PayloadAccountingInvariant);
        }
        Ok(f())
    }

    /// Return a bounded copy explicitly representing the last as-of snapshot.
    pub(crate) fn report(&self) -> Option<DryRunReport> {
        self.state.lock().unwrap().report.clone()
    }

    /// Replace at most once a second, even under notification or direct-call
    /// churn.
    pub(crate) async fn turn(
        &self,
        db: &Database,
        generation: RetentionGeneration,
    ) -> DbResult<()> {
        self.turn_with_clock(db, generation, Timestamp::now).await
    }

    /// Production clock is sampled inside the read snapshot; deterministic
    /// tests can pause there to exercise concurrent turns and configuration.
    pub(crate) async fn turn_with_clock(
        &self,
        db: &Database,
        generation: RetentionGeneration,
        clock: impl FnOnce() -> Timestamp,
    ) -> DbResult<()> {
        if self
            .attempting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        let _attempt = Attempt(&self.attempting);
        if let Err(error) = Self::check_disabled(db) {
            self.state.lock().unwrap().report = None;
            return Err(error);
        }
        {
            let mut state = self.state.lock().unwrap();
            let now = Instant::now();
            if state
                .attempted
                .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
            {
                return Ok(());
            }
            state.attempted = Some(now);
            // Errors or cancellation must not leave an older projection looking
            // like the result of this new attempt.
            state.report = None;
        }
        let report = self.snapshot(db, generation, clock).await?;
        Self::with_disabled(db, || {
            self.state.lock().unwrap().report = Some(report);
        })?;
        Ok(())
    }

    /// One read-only MVCC snapshot; caller-supplied clock exists for
    /// deterministic disposable tests and is sampled only after opening the
    /// transaction.
    pub(crate) async fn snapshot(
        &self,
        db: &Database,
        generation: RetentionGeneration,
        clock: impl FnOnce() -> Timestamp,
    ) -> DbResult<DryRunReport> {
        if generation.holder() != db.self_id {
            return Err(DbError::PayloadAccountingInvariant);
        }
        Self::check_disabled(db)?;
        let policy = RetentionPolicy::from_bytes(generation.policy_bytes())
            .ok_or(DbError::PayloadAccountingInvariant)?;
        let deadline = Instant::now()
            .checked_add(self.limits.time)
            .ok_or(DbError::Overflow)?;
        db.read_with(|tx| {
            let now = clock();
            let mut report = DryRunReport {
                as_of: now,
                generation,
                limits: self.limits,
                database_high_water: self.config.database_bytes(),
                status: DryRunStatus::AccountingNotReady,
                visited: 0,
                observed_usage: tx
                    .open_table(&crate::content_accounting_state::TABLE)?
                    .get(&())?
                    .map(|row| row.value_try())
                    .transpose()?
                    .filter(|record| matches!(record.stage, AccountingStage::Ready))
                    .map(|record| record.usage),
                projection: None,
                observed_guarded_admission: Default::default(),
            };
            let Some(usage) = report.observed_usage else {
                return Ok(report);
            };
            let mut authors = BTreeMap::<RostraId, Author>::new();
            let mut candidates = Vec::new();
            let mut total = 0u64;
            let events = tx.open_table(&crate::events::TABLE)?;
            let states = tx.open_table(&crate::events_content_state::TABLE)?;
            let origins = tx.open_table(&crate::events_retention_origins::TABLE)?;
            for row in events.range(..)? {
                if Instant::now() >= deadline {
                    report.status = DryRunStatus::TimeLimit;
                    return Ok(report);
                }
                if report.visited == self.limits.events.get() {
                    report.status = DryRunStatus::EventLimit;
                    return Ok(report);
                }
                let (id, event) = row?;
                let id = id.value_try()?;
                let event = event.value_try()?;
                report.visited += 1;
                if states.get(&id)?.is_some() {
                    continue;
                }
                let bytes = u64::from(event.content_len());
                total = total.checked_add(bytes).ok_or(DbError::Overflow)?;
                if total > self.limits.logical_bytes {
                    report.status = DryRunStatus::LogicalByteLimit;
                    return Ok(report);
                }
                if !authors.contains_key(&event.author())
                    && authors.len() == self.limits.authors.get()
                {
                    report.status = DryRunStatus::AuthorLimit;
                    return Ok(report);
                }
                let author = authors.entry(event.author()).or_insert_with(|| Author {
                    bytes: 0,
                    high: self.config.author_bytes(event.author()),
                    triggered: false,
                });
                author.bytes = author.bytes.checked_add(bytes).ok_or(DbError::Overflow)?;
                if event.author() == generation.holder()
                    || event.kind() != EventKind::SOCIAL_POST
                    || bytes == 0
                {
                    continue;
                }
                let Some(origin) = origins.get(&id)?.map(|row| row.value_try()).transpose()? else {
                    continue;
                };
                if now < origin.effective_timestamp
                    || !policy.grace_elapsed(origin.materialized_at, now)
                {
                    continue;
                }
                let event = VerifiedEvent::assume_verified_from_signed(event.signed);
                if event.event_id.to_short() != id {
                    return Err(DbError::PayloadAccountingInvariant);
                }
                candidates.push(Candidate {
                    key: policy
                        .key(
                            event.event_id,
                            generation.holder(),
                            event.content_len(),
                            origin.effective_timestamp,
                        )
                        .to_bytes(),
                    event: event.event_id,
                    author: event.author(),
                    bytes,
                    removed: false,
                });
            }
            if total != usage.logical_current_bytes {
                return Err(DbError::PayloadAccountingInvariant);
            }
            for author in authors.values_mut() {
                author.triggered = author.bytes > author.high;
            }
            // Bounded by the explicit event cap; sorting is indivisible. Reject
            // the projection if that operation consumed the time allowance.
            candidates.sort_unstable_by_key(|candidate| candidate.key);
            let mut projection = DryRunProjection {
                author_high_waters: authors
                    .iter()
                    .map(|(id, author)| (*id, author.high))
                    .collect(),
                victims: Vec::new(),
                logical_victim_bytes: 0,
                logical_remaining_bytes: total,
                protected_logical_bytes: total
                    - candidates
                        .iter()
                        .map(|candidate| candidate.bytes)
                        .sum::<u64>(),
                unmet_authors: 0,
                unmet_global: false,
            };
            for reason in [QuotaPruneReason::AuthorQuota, QuotaPruneReason::GlobalQuota] {
                let global_triggered =
                    projection.logical_remaining_bytes > self.config.database_bytes();
                for candidate in &mut candidates {
                    if Instant::now() >= deadline {
                        report.status = DryRunStatus::TimeLimit;
                        return Ok(report);
                    }
                    let author = authors.get_mut(&candidate.author).expect("counted author");
                    let needed = match reason {
                        QuotaPruneReason::AuthorQuota => {
                            author.triggered
                                && author.bytes
                                    > PayloadAdmissionConfig::experimental_low_water(author.high)
                        }
                        QuotaPruneReason::GlobalQuota => {
                            global_triggered
                                && projection.logical_remaining_bytes
                                    > PayloadAdmissionConfig::experimental_low_water(
                                        self.config.database_bytes(),
                                    )
                        }
                    };
                    if candidate.removed || !needed {
                        continue;
                    }
                    candidate.removed = true;
                    author.bytes -= candidate.bytes;
                    projection.logical_remaining_bytes -= candidate.bytes;
                    projection.logical_victim_bytes += candidate.bytes;
                    projection.victims.push((candidate.event, reason));
                }
                if reason == QuotaPruneReason::GlobalQuota {
                    projection.unmet_global = global_triggered
                        && projection.logical_remaining_bytes
                            > PayloadAdmissionConfig::experimental_low_water(
                                self.config.database_bytes(),
                            );
                }
            }
            projection.unmet_authors = authors
                .values()
                .filter(|author| {
                    author.triggered
                        && author.bytes
                            > PayloadAdmissionConfig::experimental_low_water(author.high)
                })
                .count();
            if Instant::now() >= deadline {
                report.status = DryRunStatus::TimeLimit;
                return Ok(report);
            }
            report.status = DryRunStatus::Complete;
            report.projection = Some(projection);
            Ok(report)
        })
        .await
    }
}
