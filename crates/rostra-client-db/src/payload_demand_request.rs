use std::num::NonZeroUsize;
use std::sync::Weak;
use std::time::Instant;

use crate::RetentionGeneration;

/// Immutable runtime authority and cooperative allowances for one demand step.
pub(crate) struct DemandRequest<'a> {
    /// Full policy and storing-account identity.
    pub(crate) generation: RetentionGeneration,
    /// Expected runtime config, not merely whichever config is current later.
    pub(crate) config: &'a Weak<()>,
    /// Shared candidate-row allowance across all ranked plans.
    pub(crate) scan_limit: NonZeroUsize,
    /// Maximum retained logical bytes released; Missing rejection releases
    /// zero.
    pub(crate) max_bytes: u64,
    /// Cooperative deadline, checked between indivisible operations.
    pub(crate) deadline: Instant,
}
