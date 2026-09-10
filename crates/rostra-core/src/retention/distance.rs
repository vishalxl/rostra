use crate::EventId;
use crate::id::RostraId;

/// Exact big-endian 256-bit XOR distance; smaller means a preferred holder.
///
/// Version 1 hashes the literal ASCII domain followed immediately by the full
/// 32 raw identifier bytes using unkeyed BLAKE3 (32-byte output). No length
/// prefix, textual encoding, author ID or transport key enters this metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RetentionDistance(
    /// XOR bytes, most significant first.
    [u8; 32],
);

impl RetentionDistance {
    /// Compute distance to a storing/holder account, not to an iroh endpoint.
    pub fn new(event: EventId, holder: RostraId) -> Self {
        let event = coordinate(b"rostra/retention/event/v1", event.as_slice());
        let holder = coordinate(b"rostra/retention/holder/v1", holder.as_slice());
        Self(std::array::from_fn(|i| event[i] ^ holder[i]))
    }

    /// Return exact metric bytes for stable holder ordering and test vectors.
    pub fn to_bytes(self) -> [u8; 32] {
        self.0
    }

    /// Quantize normalized distance down to Q0.64 for scoring only.
    pub(super) fn fraction(self) -> u64 {
        u64::from_be_bytes(self.0[..8].try_into().expect("Eight bytes"))
    }
}

fn coordinate(domain: &[u8], id: &[u8]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(domain);
    hash.update(id);
    *hash.finalize().as_bytes()
}
