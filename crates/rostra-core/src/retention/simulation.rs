//! Deterministic, in-memory logical-byte pressure experiment, not a DB worker.

use std::collections::{BTreeMap, BTreeSet};

use super::{RetentionKey, RetentionPolicy};
use crate::id::RostraId;
use crate::{EventId, Timestamp};

/// Explicit high/low logical-byte watermarks; neither is a disk-space promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Trigger strictly above this usage.
    high: u64,
    /// Once triggered, evict whole payloads until at or below this usage.
    low: u64,
}

impl Budget {
    /// Validate nonzero high-water and strictly lower low-water marks.
    pub fn new(high: u64, low: u64) -> Option<Self> {
        (high > 0 && low < high).then_some(Self { high, low })
    }
}

/// Synthetic retained event; protection is supplied by the experiment.
///
/// This deliberately does not classify actual event kinds or content states.
#[derive(Debug, Clone, Copy)]
pub struct Candidate {
    /// Full unique event ID.
    pub event: EventId,
    /// Author charged for these logical bytes.
    pub author: RostraId,
    /// Verified header length in an eventual DB caller.
    pub content_len: u32,
    /// Persisted, clamped age origin.
    pub effective_timestamp: Timestamp,
    /// Persisted first materialization time; absent means protected.
    pub first_materialized: Option<Timestamp>,
    /// Explicit synthetic protection, in addition to grace.
    pub protected: bool,
}

/// Dry-run result, including unmet targets caused by protected bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Events selected in deterministic eviction order.
    pub evicted: Vec<EventId>,
    /// Remaining logical bytes (shared hashes are still charged per event).
    pub retained_bytes: u128,
    /// Triggered author targets still exceeded after both passes.
    pub unmet_authors: BTreeSet<RostraId>,
    /// Whether a triggered global low-water target remains unmet.
    pub unmet_global: bool,
}

/// Simulate strict per-author pressure followed by global pressure.
///
/// Both budgets are explicit; no deployment quota or enabled mode is implied.
/// This function rejects duplicate full event IDs. Input order cannot affect
/// output. Protected/zero-length payloads never become eviction candidates, but
/// all retained bytes count. u128 sums avoid overflow for any allocatable
/// slice. No age-only expiry, fetch admission, physical GC or last-copy safety
/// is modeled.
pub fn simulate(
    policy: RetentionPolicy,
    holder: RostraId,
    now: Timestamp,
    candidates: &[Candidate],
    author_budget: Budget,
    global_budget: Budget,
) -> Option<Outcome> {
    let mut ids = BTreeSet::new();
    let mut usage = BTreeMap::<RostraId, u128>::new();
    let mut ranked = Vec::<(RetentionKey, &Candidate)>::new();
    let mut total = 0;
    for candidate in candidates {
        if !ids.insert(candidate.event) {
            return None;
        }
        let len = u128::from(candidate.content_len);
        *usage.entry(candidate.author).or_default() += len;
        total += len;
        if !candidate.protected
            && candidate.content_len != 0
            && policy.grace_elapsed(candidate.first_materialized, now)
        {
            ranked.push((
                policy.key(
                    candidate.event,
                    holder,
                    candidate.content_len,
                    candidate.effective_timestamp,
                ),
                candidate,
            ));
        }
    }
    ranked.sort_by_key(|(key, _)| *key);
    let triggered_authors: BTreeSet<_> = usage
        .iter()
        .filter_map(|(author, bytes)| (*bytes > u128::from(author_budget.high)).then_some(*author))
        .collect();
    let mut evicted = Vec::new();
    let mut removed = BTreeSet::new();
    // A global sorted scan is also sorted within every author, without depending
    // on author iteration order.
    for (_, candidate) in &ranked {
        let bytes = usage.get_mut(&candidate.author).expect("Author counted");
        if triggered_authors.contains(&candidate.author) && *bytes > u128::from(author_budget.low) {
            *bytes -= u128::from(candidate.content_len);
            total -= u128::from(candidate.content_len);
            evicted.push(candidate.event);
            removed.insert(candidate.event);
        }
    }
    let global_triggered = total > u128::from(global_budget.high);
    if global_triggered {
        for (_, candidate) in &ranked {
            if total <= u128::from(global_budget.low) {
                break;
            }
            if removed.insert(candidate.event) {
                total -= u128::from(candidate.content_len);
                *usage.get_mut(&candidate.author).expect("Author counted") -=
                    u128::from(candidate.content_len);
                evicted.push(candidate.event);
            }
        }
    }
    Some(Outcome {
        evicted,
        retained_bytes: total,
        unmet_authors: triggered_authors
            .into_iter()
            .filter(|author| usage[author] > u128::from(author_budget.low))
            .collect(),
        unmet_global: global_triggered && total > u128::from(global_budget.low),
    })
}
