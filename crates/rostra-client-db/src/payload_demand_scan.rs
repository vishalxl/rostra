use std::ops::Bound::{Excluded, Included, Unbounded};

use rostra_core::Timestamp;
use rostra_core::id::RostraId;

use crate::{DbResult, WriteTransactionCtx};

/// Constant-size advisory frontier for one bounded demand, not a victim list.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DemandScan {
    /// Index mutation incarnation; even aborted mutations invalidate advice.
    pub(crate) revision: u64,
    /// Current author pressure, or the global index.
    pub(crate) author: Option<RostraId>,
    /// Last observation, used only to invalidate on backwards walltime.
    pub(crate) observed: Timestamp,
    /// Earliest skipped future candidate becoming eligible.
    pub(crate) retry_at: Option<Timestamp>,
    /// Exclusive last rejected key; eligible victims are never skipped.
    pub(crate) after: Option<[u8; 48]>,
    /// No lower-ranked rows remain in this scope until invalidation.
    pub(crate) exhausted: bool,
    /// Minimum next-victim allowance; do not revisit with the same tiny budget.
    pub(crate) blocked_bytes: Option<u64>,
}

impl DemandScan {
    /// Seek one row so callers can check their deadline between actual visits.
    pub(crate) fn next_tx(
        &self,
        tx: &WriteTransactionCtx,
    ) -> DbResult<Option<([u8; 48], rostra_core::ShortEventId)>> {
        if let Some(author) = self.author {
            tx.open_table(&crate::content_retention_author::TABLE)?
                .range((
                    self.after
                        .map_or(Included((author, [0; 48])), |key| Excluded((author, key))),
                    Included((author, [255; 48])),
                ))?
                .next()
                .transpose()?
                .map(|(key, id)| Ok((key.value_try()?.1, id.value_try()?)))
                .transpose()
        } else {
            tx.open_table(&crate::content_retention_global::TABLE)?
                .range((self.after.map_or(Unbounded, Excluded), Unbounded))?
                .next()
                .transpose()?
                .map(|(key, id)| Ok((key.value_try()?, id.value_try()?)))
                .transpose()
        }
    }

    /// Reset when a previously skipped prefix might contain the true minimum.
    pub(crate) fn prepare(&mut self, revision: u64, author: Option<RostraId>, now: Timestamp) {
        if self.revision != revision
            || self.author != author
            || now < self.observed
            || self.retry_at.is_some_and(|retry| retry <= now)
        {
            *self = Self::new(revision, author, now);
        }
        self.observed = now;
    }

    /// Begin from the current minimum.
    pub(crate) fn new(revision: u64, author: Option<RostraId>, now: Timestamp) -> Self {
        Self {
            revision,
            author,
            observed: now,
            retry_at: None,
            after: None,
            exhausted: false,
            blocked_bytes: None,
        }
    }
}
