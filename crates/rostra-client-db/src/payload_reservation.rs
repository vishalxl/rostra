use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rostra_core::EventId;
use rostra_core::id::RostraId;
use tokio::sync::Notify;

use crate::PayloadAdmissionConfig;

/// A temporary admission refusal, never a peer failure or durable prune
/// decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, snafu::Snafu)]
pub enum PayloadAdmissionPause {
    /// Exact logical accounting is still rebuilding.
    AccountingNotReady,
    /// This event already has a logical acquisition owner.
    AlreadyReserved,
    /// The configured count of logical acquisitions or buffers is exhausted.
    InFlightCount,
    /// The configured aggregate acquisition-buffer bytes are exhausted.
    InFlightBytes,
    /// The strict author ceiling cannot fit this acquisition plus reservations.
    AuthorCapacity,
    /// The database ceiling cannot fit this acquisition plus reservations.
    DatabaseCapacity,
    /// The reservation no longer owns a Missing event, or belongs to another
    /// DB.
    ReservationExpired,
}

/// Independent logical and buffer usage; neither reports database file size.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PayloadAdmissionUsage {
    /// Distinct Missing events currently reserved.
    pub acquisitions: usize,
    /// Logical event bytes reserved in addition to retained usage.
    pub logical_reserved_bytes: u64,
    /// Distinct live admission intents, not logical reservations.
    pub pending_demands: usize,
    /// Signed lengths of live intents; no buffers or storage are promised.
    pub pending_demand_bytes: u64,
    /// Payload buffers whose owners have not released them.
    pub buffers: usize,
    /// Capacity charged for those buffers, including racing duplicate
    /// downloads.
    pub buffer_bytes: u64,
}

/// Account-scoped ledger; all logical additions occur under the attached
/// database's writer lock.
#[derive(Debug, Default)]
pub(crate) struct AdmissionLedger {
    /// Conservative process-local invalidation of demand scan frontiers.
    pub(crate) retention_revision: std::sync::atomic::AtomicU64,
    /// Arbitration for demand cancellation and logical lease release. Lock
    /// before `state`; writer transactions serialize logical additions.
    pub(crate) demands: Mutex<crate::payload_demand_state::DemandState>,
    /// No production setter exists until every acquisition caller is
    /// integrated.
    pub(crate) state: Mutex<AdmissionState>,
    /// Lossy capacity/configuration/lifecycle wakeup; callers must recheck
    /// state.
    pub(crate) changed: Notify,
}

/// Synchronous counters shared by RAII leases and serialized DB transactions.
#[derive(Debug, Default)]
pub(crate) struct AdmissionState {
    /// Exclusive database attachment; HTTP owners may outlive an unloaded DB.
    pub(crate) database_owner: std::sync::Weak<()>,
    /// Absent in every production database in this checkpoint.
    pub(crate) config: Option<PayloadAdmissionConfig>,
    /// Each full event has at most one logical acquisition owner.
    pub(crate) events: BTreeMap<EventId, ReservedEvent>,
    /// Monotonic process-local lease identity, never reused on reconfiguration.
    pub(crate) next_id: u64,
    /// Counts independent actual buffers, not just events.
    pub(crate) buffers: usize,
    /// Aggregate charged buffer capacities.
    pub(crate) buffer_bytes: u64,
}

/// A pending event's logical charge, independent of its number of peer
/// attempts.
#[derive(Debug)]
pub(crate) struct ReservedEvent {
    /// Full author owning the logical charge.
    pub(crate) author: RostraId,
    /// Verified envelope length.
    pub(crate) bytes: u64,
    /// Unique owner identity preventing stale drops from removing a new lease.
    pub(crate) id: u64,
}

/// Unique logical acquisition lease; dropping it cancels an unfinished request.
///
/// A buffer holds this lease alive. After a terminal/materialized state change
/// the logical charge is released, but buffer charges remain until their owners
/// drop.
#[derive(Debug)]
pub struct PayloadReservation {
    /// Shared immutable identity; buffer guards may extend its lifetime.
    pub(crate) inner: Arc<ReservationOwner>,
}

/// Shared lease identity, released only when the last buffer/owner is gone.
#[derive(Debug)]
pub(crate) struct ReservationOwner {
    /// Database-specific ledger; a lease cannot authorize another database.
    pub(crate) ledger: Arc<AdmissionLedger>,
    /// Full verified event identity.
    pub(crate) event: EventId,
    /// Expected payload length used for each buffer allocation.
    pub(crate) bytes: u64,
    /// Unique lease identity within this ledger.
    pub(crate) id: u64,
}

impl Drop for ReservationOwner {
    fn drop(&mut self) {
        let arbitration = self.ledger.demands.lock().unwrap();
        let mut state = self.ledger.state.lock().unwrap();
        if state
            .events
            .get(&self.event)
            .is_some_and(|e| e.id == self.id)
        {
            state.events.remove(&self.event);
        }
        drop(state);
        drop(arbitration);
        self.ledger.changed.notify_waiters();
    }
}

/// One payload-sized buffer lease; acquire before each network read/allocation.
///
/// Four simultaneous peer attempts require four of these, not one. Keep the
/// winning lease through ingestion and until its bytes are no longer owned.
#[derive(Debug)]
pub struct PayloadBuffer {
    /// Keeps the logical owner alive during the network read and ingestion.
    pub(crate) owner: Arc<ReservationOwner>,
    /// Actual reserved capacity, possibly larger than the signed length.
    pub(crate) bytes: u64,
}

impl PayloadReservation {
    /// Reserve one actual payload buffer without waiting or allocating bytes.
    pub fn try_acquire_buffer(&self) -> Result<PayloadBuffer, PayloadAdmissionPause> {
        let owner = &self.inner;
        let mut state = owner.ledger.state.lock().unwrap();
        if !state
            .events
            .get(&owner.event)
            .is_some_and(|e| e.id == owner.id)
        {
            return Err(PayloadAdmissionPause::ReservationExpired);
        }
        let Some(config) = &state.config else {
            return Err(PayloadAdmissionPause::ReservationExpired);
        };
        if state.buffers >= config.in_flight_count() {
            return Err(PayloadAdmissionPause::InFlightCount);
        }
        let Some(bytes) = state.buffer_bytes.checked_add(owner.bytes) else {
            return Err(PayloadAdmissionPause::InFlightBytes);
        };
        if bytes > config.in_flight_bytes() {
            return Err(PayloadAdmissionPause::InFlightBytes);
        }
        state.buffers += 1;
        state.buffer_bytes = bytes;
        Ok(PayloadBuffer {
            owner: owner.clone(),
            bytes: owner.bytes,
        })
    }
}

impl Drop for PayloadBuffer {
    fn drop(&mut self) {
        let mut state = self.owner.ledger.state.lock().unwrap();
        state.buffers -= 1;
        state.buffer_bytes -= self.bytes;
        drop(state);
        self.owner.ledger.changed.notify_waiters();
    }
}
