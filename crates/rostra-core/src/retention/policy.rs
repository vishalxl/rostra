use super::RetentionDistance;
use crate::id::RostraId;
use crate::{EventId, Timestamp};

/// Version-1 score parameters, with no implied production defaults.
///
/// Parameters are private so all instances satisfy the arithmetic bounds.
/// Persist the entire encoding along with the holder identity before indexing
/// keys; changing either requires a rebuild, not mixing keys from different
/// policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Positive size floor in bytes.
    size_floor: u32,
    /// Positive age scale in seconds.
    tau_seconds: u32,
    /// Size exponent in unsigned Q16.
    alpha_q16: u32,
    /// Distance exponent in unsigned Q16.
    beta_q16: u32,
    /// Positive integer maximum bonus.
    max_bonus: u32,
    /// First-materialization protection in seconds, bounded by u32.
    grace_seconds: u32,
}

/// Static ascending eviction key: signed Q32 seconds then full event ID.
///
/// Low keys are evicted first. Integer arithmetic defines version 1: natural
/// logs use 32 binary fractional bits from repeated squaring of a Q63 mantissa
/// (each square truncates), then multiply by floor(ln(2)*2^32) and truncate.
/// Size is ln(max(len,floor)) - ln(floor). Distance is rounded down to Q0.64,
/// clamped to at least one quantum, then log bonus is clamped to [0,ln(Bmax)].
/// Each nonnegative weighted term truncates toward zero after multiplication.
/// Subtraction is exact; no clock-dependent rescoring or float math occurs.
///
/// u32 lengths/scales/Q16 exponents and u64 timestamps make every intermediate
/// fit i128: log terms are below 2^38, pre-division weights below 2^102,
/// weighted terms below 2^86, and timestamp terms below 2^96. This key is
/// meaningful only within one policy/holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RetentionKey {
    /// Virtual timestamp in signed Q32 seconds.
    ticks: i128,
    /// Deterministic final tie breaker.
    event: EventId,
}

impl RetentionPolicy {
    /// Validate explicit parameters; invalid values produce no policy.
    pub fn new(
        size_floor: u32,
        tau_seconds: u32,
        alpha_q16: u32,
        beta_q16: u32,
        max_bonus: u32,
        grace_seconds: u32,
    ) -> Option<Self> {
        (size_floor > 0 && tau_seconds > 0 && max_bonus > 0).then_some(Self {
            size_floor,
            tau_seconds,
            alpha_q16,
            beta_q16,
            max_bonus,
            grace_seconds,
        })
    }

    /// Return the proposal's experimental settings, not deployment defaults.
    pub fn experimental() -> Self {
        Self::new(1024, 30 * 86400, 32768, 65536, 64, 86400).expect("Valid experimental parameters")
    }

    /// Encode version (u32 = 1), then constructor parameters as big-endian
    /// u32s.
    pub fn to_bytes(self) -> [u8; 28] {
        let values = [
            1,
            self.size_floor,
            self.tau_seconds,
            self.alpha_q16,
            self.beta_q16,
            self.max_bonus,
            self.grace_seconds,
        ];
        let mut bytes = [0; 28];
        for (chunk, value) in bytes.chunks_exact_mut(4).zip(values) {
            chunk.copy_from_slice(&value.to_be_bytes());
        }
        bytes
    }

    /// Decode only valid version-1 parameter encodings.
    pub fn from_bytes(bytes: [u8; 28]) -> Option<Self> {
        let values: [u32; 7] = std::array::from_fn(|i| {
            u32::from_be_bytes(bytes[i * 4..i * 4 + 4].try_into().expect("Four bytes"))
        });
        if values[0] != 1 {
            return None;
        }
        Self::new(
            values[1], values[2], values[3], values[4], values[5], values[6],
        )
    }

    /// Clamp author time once at first accepted header receipt; persist result.
    pub fn effective_timestamp(author: Timestamp, first_header: Timestamp) -> Timestamp {
        author.min(first_header)
    }

    /// Check only grace, not overall eligibility; absent/future origins fail
    /// shut.
    ///
    /// Persist the first materialization origin without refreshing it on
    /// duplicate delivery/re-download. Missing migration metadata must stay
    /// protected until an explicit migration supplies an origin. Callers
    /// must also check clock reliability: this function cannot detect
    /// forward clock jumps. Comparing elapsed seconds avoids deadline overflow:
    /// a deadline beyond Timestamp::MAX never elapses in representable time.
    pub fn grace_elapsed(self, first_materialized: Option<Timestamp>, now: Timestamp) -> bool {
        first_materialized.is_some_and(|first| {
            now >= first && now.secs_since(first) >= u64::from(self.grace_seconds)
        })
    }

    /// Return the explicit first-materialization protection interval in
    /// seconds.
    pub fn grace_seconds(self) -> u32 {
        self.grace_seconds
    }

    /// Return the exact distance-derived age credit in signed Q32 seconds.
    ///
    /// This is the capped logarithmic bonus after the time scale and exponent,
    /// not a probability of availability or a replication guarantee.
    pub fn distance_credit_ticks(self, event: EventId, holder: RostraId) -> i128 {
        self.key(event, holder, self.size_floor, Timestamp::from(0))
            .ticks()
    }

    /// Compute a static key from verified header metadata and persisted age
    /// time.
    pub fn key(
        self,
        event: EventId,
        holder: RostraId,
        content_len: u32,
        effective_timestamp: Timestamp,
    ) -> RetentionKey {
        self.key_at_distance(
            event,
            RetentionDistance::new(event, holder).fraction(),
            content_len,
            effective_timestamp,
        )
    }

    pub(super) fn key_at_distance(
        self,
        event: EventId,
        distance_q64: u64,
        content_len: u32,
        effective_timestamp: Timestamp,
    ) -> RetentionKey {
        let size = ln(u64::from(content_len.max(self.size_floor))) - ln(u64::from(self.size_floor));
        let bonus = (64 * LN_2 - ln(distance_q64.max(1))).min(ln(u64::from(self.max_bonus)));
        let weight = |log: u64, exponent: u32| {
            i128::from(log) * i128::from(self.tau_seconds) * i128::from(exponent) / 65536
        };
        RetentionKey {
            ticks: (i128::from(effective_timestamp.as_u64()) << 32) - weight(size, self.alpha_q16)
                + weight(bonus, self.beta_q16),
            event,
        }
    }
}

impl RetentionKey {
    /// Return signed Q32 seconds for simulation/diagnostics.
    pub fn ticks(self) -> i128 {
        self.ticks
    }

    /// Encode in ascending byte order: sign-flipped i128 BE then full event
    /// bytes.
    pub fn to_bytes(self) -> [u8; 48] {
        let mut bytes = [0; 48];
        bytes[..16].copy_from_slice(&((self.ticks as u128) ^ (1 << 127)).to_be_bytes());
        bytes[16..].copy_from_slice(self.event.as_slice());
        bytes
    }
}

const LN_2: u64 = 2_977_044_471;

fn ln(value: u64) -> u64 {
    debug_assert_ne!(value, 0);
    let exponent = 63 - value.leading_zeros();
    let mut mantissa = u128::from(value) << (63 - exponent);
    let mut fraction = 0_u64;
    for bit in (0..32).rev() {
        mantissa = (mantissa * mantissa) >> 63;
        if mantissa >= (1 << 64) {
            mantissa >>= 1;
            fraction |= 1 << bit;
        }
    }
    let log2 = (u64::from(exponent) << 32) | fraction;
    ((u128::from(log2) * u128::from(LN_2)) >> 32) as u64
}
