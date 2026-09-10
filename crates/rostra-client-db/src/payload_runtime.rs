//! Caller-owned retention maintenance, joined by the client's retained task
//! group.

use std::num::NonZeroUsize;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rostra_core::Timestamp;
use rostra_core::event::VerifiedEvent;

use crate::payload_demand::{DemandRegistration, DemandStep, PayloadDemand};
use crate::payload_demand_request::DemandRequest;
use crate::payload_pressure::{PressureCursor, PressureRequest, PressureStep, PressureWorker};
use crate::{
    Database, DbError, DbResult, PayloadReservationOutcome, RetentionClock, RetentionGeneration,
};

/// Explicit turn bounds, independent of acquisition and logical byte budgets.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeLimits {
    /// Maximum one-row maintenance/selection operations per turn.
    pub operations: NonZeroUsize,
    /// Maximum logical eviction bytes per turn; an oversized minimum blocks.
    pub bytes: u64,
    /// Maximum unique content-store bytes removed per turn, not physical pages.
    pub gc_bytes: u64,
    /// Cooperative time allowance; one DB transaction is indivisible.
    pub time: Duration,
}

/// Immutable policy binding and exclusive runner arbitration.
#[derive(Debug)]
pub(crate) struct PayloadRuntime {
    /// Exact storing-account/policy identity, never a transport key.
    generation: RetentionGeneration,
    /// Immutable admission incarnation, without cloning its override map.
    config: Weak<()>,
    /// Explicit bounded-turn settings.
    limits: RuntimeLimits,
    /// A borrowed RAII owner prevents overlapping runners, including on panic.
    running: AtomicBool,
    /// Incarnation binding survives cancellation of a borrowed runner.
    pressure: PressureWorker,
    /// Independent forecast configuration; admission remains Disabled.
    dry_run: Option<crate::payload_dry_run::DryRun>,
}

/// Scheduler continuation only; no state here authorizes pruning.
#[derive(Debug, Default)]
pub(crate) struct RuntimeCursor {
    /// Round-robin next subsystem, preserved even with a one-operation turn.
    phase: RuntimePhase,
    /// Independent readiness for accounting, nomination, index and due prefix.
    ready: Readiness,
    /// Fixed promotion target until that prefix is drained.
    drain_at: Option<Timestamp>,
    /// Bounded general-pressure discovery and selection.
    pressure: PressureCursor,
    /// Pressure waits until the next recovery cycle after blockage/exhaustion.
    pressure_waiting: bool,
    /// Exclusive quota nomination frontier; oversized hashes remain queued.
    gc_after: Option<rostra_core::ContentHash>,
    /// A complete quota queue pass waits before retrying skipped nominations.
    gc_done: bool,
    /// Progress survives a cycle split across one-operation turns.
    cycle_progress: bool,
}

/// Explicit round-robin order, independent of the per-turn operation allowance.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum RuntimePhase {
    #[default]
    Accounting,
    Nominations,
    Index,
    Grace,
    Demand,
    Pressure,
    Collection,
}

impl RuntimePhase {
    fn next(self) -> Self {
        match self {
            Self::Accounting => Self::Nominations,
            Self::Nominations => Self::Index,
            Self::Index => Self::Grace,
            Self::Grace => Self::Demand,
            Self::Demand => Self::Pressure,
            Self::Pressure => Self::Collection,
            Self::Collection => Self::Accounting,
        }
    }
}

/// Independent readiness dimensions; none alone grants selection authority.
#[derive(Debug, Default)]
struct Readiness {
    /// Logical and hash counters are complete.
    accounting: bool,
    /// Historical quota nominations have been recovered.
    nominations: bool,
    /// The full policy/holder index is built.
    index: bool,
    /// The fixed-time grace prefix has drained.
    grace: bool,
}

impl Readiness {
    fn all(&self) -> bool {
        self.accounting && self.nominations && self.index && self.grace
    }
}

/// Advisory scheduling result, not durable pressure or candidate authority.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RuntimeTurn {
    /// A bounded frontier moved or maintenance remains; yield before
    /// continuing.
    Continue,
    /// Nothing can progress with this allowance; await signals or recovery.
    Wait,
}

/// Borrowed unique runner ownership, released on cancellation or unwinding.
struct Running<'a>(&'a AtomicBool);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl PayloadRuntime {
    /// Bind explicit validated allowances to an already configured account.
    pub(crate) fn new(
        generation: RetentionGeneration,
        config: Weak<()>,
        limits: RuntimeLimits,
    ) -> Option<Self> {
        if limits.operations.get() > crate::PAYLOAD_MAINTENANCE_MAX
            || limits.bytes == 0
            || limits.gc_bytes == 0
            || limits.time.is_zero()
            || Instant::now().checked_add(limits.time).is_none()
        {
            return None;
        }
        Some(Self {
            generation,
            config,
            limits,
            running: AtomicBool::new(false),
            pressure: PressureWorker::default(),
            dry_run: None,
        })
    }

    /// Construct a separate forecast; its caps never enter admission.
    pub(crate) fn new_dry_run(
        db: &Database,
        generation: RetentionGeneration,
        config: crate::PayloadAdmissionConfig,
        limits: crate::payload_dry_run::DryRunLimits,
    ) -> DbResult<Self> {
        crate::payload_dry_run::DryRun::check_disabled(db)?;
        if generation.holder() != db.self_id {
            return Err(DbError::PayloadAccountingInvariant);
        }
        let identity = config.identity();
        let dry_run = crate::payload_dry_run::DryRun::new(config, limits)
            .ok_or(DbError::PayloadAccountingInvariant)?;
        Ok(Self {
            generation,
            config: identity,
            limits: RuntimeLimits {
                operations: limits.events,
                bytes: limits.logical_bytes,
                gc_bytes: 0,
                time: limits.time,
            },
            running: AtomicBool::new(false),
            pressure: PressureWorker::default(),
            dry_run: Some(dry_run),
        })
    }

    /// Last bounded as-of forecast; not fresh work or current pruning
    /// authority.
    pub(crate) fn dry_run_report(&self) -> Option<crate::payload_dry_run::DryRunReport> {
        self.dry_run.as_ref().and_then(|dry_run| dry_run.report())
    }

    /// Reject stale construction rather than silently installing a new policy.
    fn check_config(&self, db: &Database) -> DbResult<()> {
        if self.dry_run.is_some() {
            return crate::payload_dry_run::DryRun::check_disabled(db);
        }
        if db
            .payload_admission
            .state
            .lock()
            .unwrap()
            .config
            .as_ref()
            .is_none_or(|config| !self.config.ptr_eq(&config.identity()))
        {
            return Err(DbError::PayloadAccountingInvariant);
        }
        Ok(())
    }

    /// Wait with only a verified header and at most one expiring demand owner.
    ///
    /// Preparation owns no input payload, provisional allocation or buffer.
    /// Shared-store attempts drop their scratch before pausing. A monotonic
    /// 30-second outer deadline also bounds waits across wall-clock changes.
    /// Retrying cannot renew the original demand. Callers receiving Deferred
    /// still use their existing retry scheduling, not a peer failure.
    pub(crate) async fn prepare(
        &self,
        db: &Database,
        event: &VerifiedEvent,
    ) -> DbResult<PayloadReservationOutcome> {
        if self.dry_run.is_some() {
            self.check_config(db)?;
            return db.prepare_payload_acquisition_once(event).await;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut demand: Option<PayloadDemand> = None;
        loop {
            self.check_config(db)?;
            let notified = db.payload_admission_changed();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let outcome = db.prepare_payload_acquisition_once(event).await?;
            let PayloadReservationOutcome::Deferred(_) = outcome else {
                return Ok(outcome);
            };
            if tokio::time::Instant::now() >= deadline
                || demand.as_ref().is_some_and(|owner| !owner.is_live())
            {
                return Ok(outcome);
            }
            if demand.is_none() {
                match db
                    .register_payload_demand(event.event_id, self.generation)
                    .await?
                {
                    DemandRegistration::Pending(owner) => demand = Some(owner),
                    DemandRegistration::Overloaded => return Ok(outcome),
                    DemandRegistration::Unneeded => {}
                }
            }
            // Bound retries even under self-generated and sustained external
            // notification churn. No payload allocation is retained here.
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + Duration::from_millis(100)),
            )
            .await;
            tokio::select! {
                () = &mut notified => {},
                () = tokio::time::sleep_until(
                    deadline.min(tokio::time::Instant::now() + Duration::from_secs(1))
                ) => {},
            }
        }
    }

    /// Run under caller-owned task lifetime, with no detached destructive work.
    ///
    /// Register before reconciliation; signals coalesce and a one-second
    /// recovery wait also notices grace deadlines and lost notifications.
    /// Maintenance never depends on the presence of a live acquisition.
    pub(crate) async fn run(&self, db: &Database) -> DbResult<()> {
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(DbError::PayloadAccountingInvariant);
        }
        let _running = Running(&self.running);
        if self.dry_run.is_none() {
            db.write_with(|tx| {
                self.check_config(db)?;
                db.configure_retention_index_tx(tx, self.generation)?;
                Ok(())
            })
            .await?;
        }
        let mut cursor = RuntimeCursor::default();
        loop {
            let notified = db.payload_admission_changed();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.turn(db, &mut cursor).await? {
                RuntimeTurn::Continue => tokio::task::yield_now().await,
                RuntimeTurn::Wait => {
                    // Minimum reconciliation spacing prevents notification storms
                    // (including our own after-commit wakes) from spinning.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    tokio::select! {
                        () = &mut notified => {},
                        () = tokio::time::sleep(Duration::from_secs(1)) => {},
                    }
                }
            }
        }
    }

    /// Interleave independent one-row maintenance with existing atomic demand
    /// preemption. All pruning authority remains inside the fresh writer step.
    pub(crate) async fn turn(
        &self,
        db: &Database,
        cursor: &mut RuntimeCursor,
    ) -> DbResult<RuntimeTurn> {
        if let Some(dry_run) = &self.dry_run {
            dry_run.turn(db, self.generation).await?;
            return Ok(RuntimeTurn::Wait);
        }
        self.check_config(db)?;
        let deadline = Instant::now()
            .checked_add(self.limits.time)
            .ok_or(DbError::Overflow)?;
        let one = NonZeroUsize::new(1).expect("one is nonzero");
        let mut bytes = self.limits.bytes;
        let mut gc_bytes = self.limits.gc_bytes;
        let mut continued = false;
        let mut worked = false;
        for _ in 0..self.limits.operations.get() {
            if Instant::now() >= deadline {
                break;
            }
            worked = true;
            let phase = cursor.phase;
            cursor.phase = phase.next();
            match phase {
                RuntimePhase::Accounting => {
                    cursor.ready.accounting = db.rebuild_payload_accounting(one).await?.ready
                }
                RuntimePhase::Nominations => {
                    cursor.ready.nominations =
                        db.rebuild_quota_payload_nominations(one).await?.ready
                }
                RuntimePhase::Index => {
                    let progress = db
                        .rebuild_retention_index(one)
                        .await?
                        .ok_or(DbError::PayloadAccountingInvariant)?;
                    if progress.generation != self.generation {
                        return Err(DbError::PayloadAccountingInvariant);
                    }
                    cursor.ready.index = progress.ready;
                }
                RuntimePhase::Grace => {
                    let now = *cursor.drain_at.get_or_insert_with(Timestamp::now);
                    let progress = db
                        .promote_retention_grace(RetentionClock::Trusted(now), one)
                        .await?
                        .ok_or(DbError::PayloadAccountingInvariant)?;
                    if progress.generation != self.generation {
                        return Err(DbError::PayloadAccountingInvariant);
                    }
                    cursor.ready.grace = progress.ready && progress.visited == 0;
                    if cursor.ready.grace {
                        cursor.drain_at = None;
                    }
                }
                RuntimePhase::Demand if cursor.ready.all() => {
                    match db
                        .preempt_payload_demand(DemandRequest {
                            generation: self.generation,
                            config: &self.config,
                            scan_limit: one,
                            max_bytes: bytes,
                            deadline,
                        })
                        .await?
                    {
                        DemandStep::Pruned {
                            bytes: released, ..
                        } => {
                            bytes = bytes.checked_sub(released).ok_or(DbError::Overflow)?;
                            continued = true;
                        }
                        DemandStep::Continue | DemandStep::Rejected { .. } => continued = true,
                        DemandStep::NotReady => {
                            // A newer grace deadline can become due after a fixed
                            // prefix drain. Never reuse that older time as authority.
                            cursor.ready = Readiness::default();
                        }
                        DemandStep::Idle
                        | DemandStep::Fits(_)
                        | DemandStep::NoVictim { .. }
                        | DemandStep::Bounded => {}
                    }
                }
                RuntimePhase::Pressure if cursor.ready.all() && !cursor.pressure_waiting => {
                    match self
                        .pressure
                        .step(
                            db,
                            PressureRequest {
                                generation: self.generation,
                                config: &self.config,
                                max_bytes: bytes,
                                deadline,
                            },
                            &mut cursor.pressure,
                        )
                        .await?
                    {
                        PressureStep::Continue => continued = true,
                        PressureStep::Pruned {
                            logical_released_bytes: released,
                        } => {
                            bytes = bytes.checked_sub(released).ok_or(DbError::Overflow)?;
                            continued = true;
                        }
                        PressureStep::Wait => cursor.pressure_waiting = true,
                        PressureStep::NotReady => cursor.ready = Readiness::default(),
                    }
                }
                RuntimePhase::Collection
                    if cursor.ready.accounting && cursor.ready.nominations && !cursor.gc_done =>
                {
                    let (after, progress) = db
                        .collect_quota_payload_step(cursor.gc_after, gc_bytes)
                        .await?;
                    gc_bytes = gc_bytes
                        .checked_sub(progress.removed_bytes)
                        .ok_or(DbError::Overflow)?;
                    cursor.gc_after = after;
                    cursor.gc_done = after.is_none();
                    continued |= !cursor.gc_done;
                }
                _ => {}
            }
            cursor.cycle_progress |= continued;
            if cursor.phase == RuntimePhase::Accounting {
                continued |= cursor.cycle_progress;
                cursor.cycle_progress = false;
            }
            if cursor.phase == RuntimePhase::Accounting
                && !continued
                && cursor.ready.all()
                && cursor.pressure_waiting
                && cursor.gc_done
            {
                break;
            }
        }
        let outcome = if worked
            && (continued
                || cursor.phase != RuntimePhase::Accounting
                || !cursor.ready.all()
                || !cursor.pressure_waiting
                || !cursor.gc_done)
        {
            RuntimeTurn::Continue
        } else {
            RuntimeTurn::Wait
        };
        if outcome == RuntimeTurn::Wait {
            cursor.pressure_waiting = false;
            cursor.gc_done = false;
        }
        Ok(outcome)
    }
}

impl Database {
    /// Whether immutable startup configuration requires a retained worker.
    pub fn has_payload_retention_runtime(&self) -> bool {
        self.payload_runtime.is_some()
    }

    /// Run configured maintenance under the caller's retained, joined task.
    ///
    /// This spawns no work. Cancellation stops between indivisible database
    /// operations; callers must join before reopening or reconstructing a
    /// client.
    pub async fn run_payload_retention(&self) -> DbResult<()> {
        if let Some(runtime) = &self.payload_runtime {
            runtime.run(self).await?;
        }
        Ok(())
    }

    /// Return the last bounded read-only snapshot, not cumulative pruning work.
    pub fn payload_retention_forecast(&self) -> Option<crate::DryRunReport> {
        self.payload_runtime.as_ref()?.dry_run_report()
    }
}
