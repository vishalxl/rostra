//! Internal vertical slice, deliberately not a production activation surface.
//!
//! The caller owns and joins `run`; this module never spawns a task. It must be
//! integrated with Client's retained task completion before production
//! activation.

use std::num::NonZeroUsize;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rostra_core::Timestamp;
use rostra_core::event::VerifiedEvent;

use crate::payload_demand::{DemandRegistration, DemandStep, PayloadDemand};
use crate::{
    Database, DbError, DbResult, PayloadReservationOutcome, RetentionClock, RetentionGeneration,
};

/// Explicit turn bounds, independent of acquisition and logical byte budgets.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RuntimeLimits {
    /// Maximum one-row maintenance/selection operations per turn.
    pub(crate) operations: NonZeroUsize,
    /// Maximum logical eviction bytes per turn; an oversized minimum blocks.
    pub(crate) bytes: u64,
    /// Cooperative time allowance; one DB transaction is indivisible.
    pub(crate) time: Duration,
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
}

/// Scheduler continuation only; no state here authorizes pruning.
#[derive(Debug, Default)]
pub(crate) struct RuntimeCursor {
    /// Round-robin next subsystem, preserved even with a one-operation turn.
    phase: usize,
    /// Independent readiness for accounting, nomination, index and due prefix.
    ready: [bool; 4],
    /// Fixed promotion target until that prefix is drained.
    drain_at: Option<Timestamp>,
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
    #[cfg(test)]
    pub(crate) fn new(
        generation: RetentionGeneration,
        config: Weak<()>,
        limits: RuntimeLimits,
    ) -> Option<Self> {
        if limits.operations.get() > crate::PAYLOAD_MAINTENANCE_MAX
            || limits.bytes == 0
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
        })
    }

    /// Reject stale construction rather than silently installing a new policy.
    fn check_config(&self, db: &Database) -> DbResult<()> {
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
        self.check_config(db)?;
        let deadline = Instant::now()
            .checked_add(self.limits.time)
            .ok_or(DbError::Overflow)?;
        let one = NonZeroUsize::new(1).expect("one is nonzero");
        let mut bytes = self.limits.bytes;
        let mut continued = false;
        let mut worked = false;
        for _ in 0..self.limits.operations.get() {
            if Instant::now() >= deadline {
                break;
            }
            worked = true;
            let phase = cursor.phase;
            cursor.phase = (phase + 1) % 5;
            match phase {
                0 => cursor.ready[0] = db.rebuild_payload_accounting(one).await?.ready,
                1 => cursor.ready[1] = db.rebuild_quota_payload_nominations(one).await?.ready,
                2 => {
                    let progress = db
                        .rebuild_retention_index(one)
                        .await?
                        .ok_or(DbError::PayloadAccountingInvariant)?;
                    if progress.generation != self.generation {
                        return Err(DbError::PayloadAccountingInvariant);
                    }
                    cursor.ready[2] = progress.ready;
                }
                3 => {
                    let now = *cursor.drain_at.get_or_insert_with(Timestamp::now);
                    let progress = db
                        .promote_retention_grace(RetentionClock::Trusted(now), one)
                        .await?
                        .ok_or(DbError::PayloadAccountingInvariant)?;
                    if progress.generation != self.generation {
                        return Err(DbError::PayloadAccountingInvariant);
                    }
                    cursor.ready[3] = progress.ready && progress.visited == 0;
                    if cursor.ready[3] {
                        cursor.drain_at = None;
                    }
                }
                _ if cursor.ready.iter().all(|ready| *ready) => {
                    match db
                        .preempt_payload_demand(self.generation, one, bytes, deadline)
                        .await?
                    {
                        DemandStep::Pruned {
                            bytes: released, ..
                        } => {
                            bytes = bytes.checked_sub(released).ok_or(DbError::Overflow)?;
                            continued = true;
                        }
                        DemandStep::Continue => continued = true,
                        DemandStep::NotReady => {
                            // A newer grace deadline can become due after a fixed
                            // prefix drain. Never reuse that older time as authority.
                            cursor.ready = [false; 4];
                        }
                        DemandStep::Idle
                        | DemandStep::Fits(_)
                        | DemandStep::NoVictim { .. }
                        | DemandStep::Bounded => {}
                    }
                }
                _ => {}
            }
            if cursor.phase == 0 && !continued && cursor.ready.iter().all(|ready| *ready) {
                break;
            }
        }
        Ok(
            if worked && (continued || cursor.phase != 0 || cursor.ready.iter().any(|ready| !ready))
            {
                RuntimeTurn::Continue
            } else {
                RuntimeTurn::Wait
            },
        )
    }
}
