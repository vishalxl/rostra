use rostra_core::retention::RetentionPolicy;

use crate::{DryRunLimits, PayloadAdmissionConfig, PayloadRuntimeLimits};

/// Immutable startup input for one explicitly listed storing account.
///
/// Disabled is the default. Enforce uses experimental static scoring and 90%
/// low-water targets, not a disk-space guarantee. DryRun forecasts separately
/// from Disabled admission. Changing any input requires quiescent shutdown and
/// fresh construction; no running account can be reconfigured.
#[derive(Debug, Clone, Default)]
pub enum PayloadRetentionConfig {
    /// Ordinary ingestion, without quota admission or retention maintenance.
    #[default]
    Disabled,
    /// Bounded read-only observation; never installs enforcing admission.
    DryRun {
        /// Explicit versioned experimental scoring and grace parameters.
        policy: RetentionPolicy,
        /// Forecast ceilings only, never actual reservations or rejections.
        admission: PayloadAdmissionConfig,
        /// Whole-source snapshot bounds; an incomplete model has no projection.
        limits: DryRunLimits,
    },
    /// Strict admission plus bounded author-first retention and quota-only GC.
    Enforce {
        /// Explicit versioned experimental scoring and grace parameters.
        policy: RetentionPolicy,
        /// Logical ceilings and independent acquisition-buffer capacity.
        admission: PayloadAdmissionConfig,
        /// Explicit mutation and cooperative-time allowances.
        limits: PayloadRuntimeLimits,
    },
}
