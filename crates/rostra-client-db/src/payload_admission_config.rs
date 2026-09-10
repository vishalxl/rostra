use std::collections::BTreeMap;
use std::num::{NonZeroU64, NonZeroUsize};

use rostra_core::id::RostraId;

/// Named input budgets, preventing positional swaps of distinct byte limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadAdmissionLimits {
    /// Logical retained bytes in this identity's database, not physical disk.
    pub database_bytes: NonZeroU64,
    /// Common strict author ceiling; this is not a reserved share.
    pub author_bytes: NonZeroU64,
    /// Explicit full-account author ceilings.
    pub overrides: BTreeMap<RostraId, NonZeroU64>,
    /// Maximum simultaneous logical acquisitions and, independently, buffers.
    pub in_flight_count: NonZeroUsize,
    /// Maximum aggregate bytes of simultaneously owned payload buffers.
    pub in_flight_bytes: NonZeroU64,
}

/// Explicit logical payload ceilings and independent acquisition-buffer limits.
///
/// This foundation has no production configuration setter. Installing a policy
/// requires the complete client admission/worker integration, not just a cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadAdmissionConfig {
    /// Validated immutable input, including the bounded maximum lease count.
    limits: PayloadAdmissionLimits,
}

impl PayloadAdmissionConfig {
    /// Validate explicit budgets; there is deliberately no universal byte
    /// default.
    pub fn new(limits: PayloadAdmissionLimits) -> Option<Self> {
        if limits.in_flight_count.get() > crate::PAYLOAD_MAINTENANCE_MAX {
            return None;
        }
        Some(Self { limits })
    }

    /// Return the per-database logical high-water ceiling.
    pub fn database_bytes(&self) -> u64 {
        self.limits.database_bytes.get()
    }

    /// Return the strict ceiling for a full author identity.
    pub fn author_bytes(&self, author: RostraId) -> u64 {
        self.limits
            .overrides
            .get(&author)
            .unwrap_or(&self.limits.author_bytes)
            .get()
    }

    /// Return the bounded number of logical acquisitions and, independently,
    /// buffers.
    pub fn in_flight_count(&self) -> usize {
        self.limits.in_flight_count.get()
    }

    /// Return the independent budget for actual acquisition buffers.
    pub fn in_flight_bytes(&self) -> u64 {
        self.limits.in_flight_bytes.get()
    }

    /// Experimental starting low water: floor(90% of the explicit high water).
    ///
    /// The future worker owns hysteresis; this arithmetic does not start
    /// eviction.
    pub fn experimental_low_water(high_water: u64) -> u64 {
        (high_water / 10) * 9 + ((high_water % 10) * 9) / 10
    }
}
