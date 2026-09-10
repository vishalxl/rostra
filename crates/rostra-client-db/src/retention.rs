//! Durable local retention source metadata, independent of disposable indexes.

use bincode::{Decode, Encode};
use rostra_core::Timestamp;

/// Immutable local age and grace origins for an accepted event.
///
/// An absent row means that first receipt predates tracking. Such an event is
/// not eligible for quota pruning until a conservative migration policy handles
/// that uncertainty. Replay must never manufacture receipt or grace origins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub(crate) struct RetentionOrigins {
    /// Author time clamped to the first accepted local header receipt.
    pub(crate) effective_timestamp: Timestamp,
    /// First successful payload materialization, not subsequent
    /// replay/delivery.
    pub(crate) materialized_at: Option<Timestamp>,
}

/// Durable reason for a local quota decision, distinct from signed deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub enum QuotaPruneReason {
    /// The event author's logical retained bytes exceeded its quota.
    AuthorQuota,
    /// The database's logical retained bytes exceeded its global quota.
    GlobalQuota,
}

/// Authoritative local decision that must survive projection reconstruction.
///
/// The checked quota transition dematerializes projections and updates
/// bookkeeping before committing this row. Signed deletion remains stronger
/// than this decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub(crate) struct QuotaPruneDecision {
    /// Pressure source that caused the original decision.
    pub(crate) reason: QuotaPruneReason,
    /// Local decision time, retained even if a later signed deletion wins.
    pub(crate) pruned_at: Timestamp,
}
