use std::sync::Arc;

use crate::payload_reservation::AdmissionLedger;
use crate::{Database, PayloadAdmissionPause, PayloadBuffer, PayloadReservation};

/// Capacity for bytes allocated before a verified envelope is available.
///
/// This shares the payload buffer ledger, not logical event reservations.
/// Reserve before allocation and retain until every covered buffer is dropped.
/// It does not authorize materialization or measure allocator/codec overhead.
#[derive(Debug)]
pub struct PayloadAllocation {
    /// Account-scoped shared buffer ledger, also available before database
    /// load.
    ledger: Option<Arc<AdmissionLedger>>,
    /// Aggregate capacities covered by this owner.
    bytes: u64,
}

impl Database {
    /// Reserve provisional buffer capacity, or return `None` when disabled.
    ///
    /// HTTP callers must cover the simultaneous body and decoded payload, not
    /// trust Content-Length or a not-yet-verified envelope's declared length.
    pub fn reserve_payload_allocation(
        &self,
        bytes: u64,
    ) -> Result<Option<PayloadAllocation>, PayloadAdmissionPause> {
        PayloadAllocation::reserve(&self.payload_admission, bytes)
    }
}

impl PayloadAllocation {
    pub(crate) fn reserve(
        ledger: &Arc<AdmissionLedger>,
        bytes: u64,
    ) -> Result<Option<Self>, PayloadAdmissionPause> {
        let mut state = ledger.state.lock().unwrap();
        let Some(config) = &state.config else {
            return Ok(None);
        };
        if state.buffers >= config.in_flight_count() {
            return Err(PayloadAdmissionPause::InFlightCount);
        }
        let total = state
            .buffer_bytes
            .checked_add(bytes)
            .filter(|total| *total <= config.in_flight_bytes())
            .ok_or(PayloadAdmissionPause::InFlightBytes)?;
        state.buffers += 1;
        state.buffer_bytes = total;
        Ok(Some(PayloadAllocation {
            ledger: Some(ledger.clone()),
            bytes,
        }))
    }
}

impl PayloadAllocation {
    /// Grow a provisional charge before extending its covered byte allocation.
    /// After successful binding this returns `ReservationExpired`.
    pub fn try_grow(&mut self, bytes: u64) -> Result<(), PayloadAdmissionPause> {
        let ledger = self
            .ledger
            .as_ref()
            .ok_or(PayloadAdmissionPause::ReservationExpired)?;
        if bytes <= self.bytes {
            return Ok(());
        }
        let mut state = ledger.state.lock().unwrap();
        let Some(config) = &state.config else {
            return Err(PayloadAdmissionPause::ReservationExpired);
        };
        let total = state
            .buffer_bytes
            .checked_add(bytes - self.bytes)
            .filter(|total| *total <= config.in_flight_bytes())
            .ok_or(PayloadAdmissionPause::InFlightBytes)?;
        state.buffer_bytes = total;
        self.bytes = bytes;
        Ok(())
    }

    /// Bind an already covered payload to its verified logical reservation.
    ///
    /// All other provisional buffers must be released separately. Capacity is
    /// transferred without a second buffer slot or an uncharged interval.
    /// The provisional owner is disarmed on success; further binding or growth
    /// returns `ReservationExpired` rather than reusing the charge.
    pub fn bind_to_reservation(
        &mut self,
        reservation: &PayloadReservation,
    ) -> Result<PayloadBuffer, PayloadAdmissionPause> {
        let ledger = self
            .ledger
            .as_ref()
            .ok_or(PayloadAdmissionPause::ReservationExpired)?;
        let owner = &reservation.inner;
        if !Arc::ptr_eq(ledger, &owner.ledger) || self.bytes < owner.bytes {
            return Err(PayloadAdmissionPause::ReservationExpired);
        }
        let state = ledger.state.lock().unwrap();
        if !state
            .events
            .get(&owner.event)
            .is_some_and(|e| e.id == owner.id)
        {
            return Err(PayloadAdmissionPause::ReservationExpired);
        }
        // Retain the full covered allocation capacity, even if it exceeds length.
        let buffer = PayloadBuffer {
            owner: owner.clone(),
            bytes: self.bytes,
        };
        drop(state);
        self.ledger = None;
        Ok(buffer)
    }
}

impl Drop for PayloadAllocation {
    fn drop(&mut self) {
        let Some(ledger) = &self.ledger else {
            return;
        };
        let mut state = ledger.state.lock().unwrap();
        state.buffers -= 1;
        state.buffer_bytes -= self.bytes;
        drop(state);
        ledger.changed.notify_waiters();
    }
}
