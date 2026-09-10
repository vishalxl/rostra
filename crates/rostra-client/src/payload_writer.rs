use std::io;

use rostra_client_db::{PayloadAdmissionPause, PayloadAllocation};

/// Budget each CBOR output growth before allocating it.
pub(crate) struct PayloadWriter<B = PayloadAllocation> {
    /// Encoded bytes drop before their capacity owner.
    pub(crate) bytes: Vec<u8>,
    /// Provisional capacity, later bound to the signed event.
    allocation: B,
    /// Typed capacity refusal, kept separate from serialization/validation
    /// errors.
    pub(crate) pause: Option<PayloadAdmissionPause>,
}

/// Capacity callback kept separate from the serializer for allocation-order
/// tests.
pub(crate) trait GrowCapacity {
    /// Charge the requested capacity before the writer allocates.
    fn try_grow(&mut self, bytes: u64) -> Result<(), PayloadAdmissionPause>;
}

impl GrowCapacity for PayloadAllocation {
    fn try_grow(&mut self, bytes: u64) -> Result<(), PayloadAdmissionPause> {
        PayloadAllocation::try_grow(self, bytes)
    }
}

impl<B: GrowCapacity> PayloadWriter<B> {
    /// Start with an already reserved buffer slot and no allocated bytes.
    pub(crate) fn new(allocation: B) -> Self {
        Self {
            bytes: Vec::new(),
            allocation,
            pause: None,
        }
    }
}

impl<B: GrowCapacity> io::Write for PayloadWriter<B> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let needed = self
            .bytes
            .len()
            .checked_add(buf.len())
            .ok_or_else(|| io::Error::other("Payload length overflow"))?;
        if needed > self.bytes.capacity() {
            let preferred = needed.max(self.bytes.capacity().saturating_mul(2)).max(128);
            let allocation = &mut self.allocation;
            let capacity = if allocation.try_grow(preferred as u64).is_ok() {
                preferred
            } else {
                allocation.try_grow(needed as u64).map_err(|reason| {
                    self.pause = Some(reason);
                    io::Error::other("Payload storage buffer capacity unavailable")
                })?;
                needed
            };
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
