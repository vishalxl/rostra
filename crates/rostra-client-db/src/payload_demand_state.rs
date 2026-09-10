use std::collections::BTreeMap;
use std::sync::Weak;

use rostra_core::id::ToShort as _;
use rostra_core::{EventId, ShortEventId, Timestamp};

use crate::RetentionGeneration;
use crate::payload_demand::DemandOwner;

/// Bounded metadata for one intent; no retained content or acquisition guard.
#[derive(Debug)]
pub(crate) struct DemandEntry {
    /// Weak deduplication handle; the caller owns cancellation.
    pub(crate) owner: Weak<DemandOwner>,
    /// Identity preventing a stale owner's drop from cancelling a replacement.
    pub(crate) id: u64,
    /// Verified logical size, counted as intent only.
    pub(crate) bytes: u64,
    /// Registration walltime, also rejecting a backwards clock before it.
    pub(crate) created: Timestamp,
    /// Fixed expiry, not extended by duplicate requests.
    pub(crate) expires: Timestamp,
}

/// Account-local arbitration and bounded pending metadata.
#[derive(Debug, Default)]
pub(crate) struct DemandState {
    /// One complete policy/holder identity for all current demands.
    pub(crate) generation: Option<RetentionGeneration>,
    /// Weak immutable budget incarnation; comparison/cleanup never visits or
    /// retains the potentially large author-override map.
    pub(crate) config: Weak<()>,
    /// At most the configured in-flight count and bytes of distinct intent.
    pub(crate) entries: BTreeMap<EventId, DemandEntry>,
    /// Monotonic demand identity, not reset when invalidating old owners.
    pub(crate) next_id: u64,
}

impl DemandState {
    /// Invalidate ownership without reusing cancellation identities.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.generation = None;
        self.config = Weak::new();
    }

    /// Forget a completed event after the lifecycle transaction commits.
    pub(crate) fn remove_completed(&mut self, id: ShortEventId) {
        self.entries.retain(|event, _| event.to_short() != id);
    }

    /// Expire bounded metadata using trusted walltime; no clock latch is kept.
    pub(crate) fn expire(&mut self, now: Timestamp) {
        self.entries
            .retain(|_, e| e.created <= now && now < e.expires);
    }

    /// Return distinct intent counters, never promised storage or memory.
    pub(crate) fn usage(&self) -> (usize, u64) {
        (
            self.entries.len(),
            self.entries.values().map(|e| e.bytes).sum(),
        )
    }
}
