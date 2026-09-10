//! Disposable static-key indexes. Selection is advice, never pressure
//! authority.

use std::num::NonZeroUsize;
use std::ops::Bound::{Excluded, Included, Unbounded};

use bincode::{Decode, Encode};
use rostra_core::event::{EventExt as _, EventKind, VerifiedEvent};
use rostra_core::id::{RostraId, ToShort as _};
use rostra_core::retention::RetentionPolicy;
use rostra_core::{EventId, ShortEventId};
use snafu::OptionExt as _;

use crate::{
    Database, DbResult, RetentionClock, WriteTransactionCtx, content_retention_author as authors,
    content_retention_global as global, content_retention_grace as grace,
    content_retention_reverse as reverse, content_retention_state as state,
};

/// Complete scoring identity; transport keys cannot enter this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub struct RetentionGeneration {
    /// Complete versioned arithmetic and grace policy.
    policy: [u8; 28],
    /// Account storing the payloads, not the remote author or transport
    /// endpoint.
    holder: RostraId,
}

impl RetentionGeneration {
    /// Construct an identity from explicit policy parameters and storing
    /// account.
    pub fn new(policy: RetentionPolicy, holder: RostraId) -> Self {
        Self {
            policy: policy.to_bytes(),
            holder,
        }
    }

    /// Return the full versioned encoding used for this generation.
    pub fn policy_bytes(self) -> [u8; 28] {
        self.policy
    }

    /// Return the storing account identity used for distance.
    pub fn holder(self) -> RostraId {
        self.holder
    }

    fn policy(self) -> DbResult<RetentionPolicy> {
        RetentionPolicy::from_bytes(self.policy).context(crate::PayloadAccountingInvariantSnafu)
    }
}

/// Durable independent index progress, unrelated to accounting or GC readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionIndexProgress {
    /// Full identity currently being built.
    pub generation: RetentionGeneration,
    /// Whether the complete event set has been indexed under this identity.
    pub ready: bool,
    /// Source, cleanup, or grace rows visited in this bounded call.
    pub visited: usize,
}

/// Bounded reverse cleanup precedes rebuilding; old and new policies never mix.
#[derive(Debug, Clone, Copy, Encode, Decode)]
pub(crate) enum IndexStage {
    /// Remove each owned forward row before building the replacement
    /// generation.
    Clearing,
    /// Last scanned event; concurrent reducers upsert without changing the
    /// cursor.
    Events(Option<ShortEventId>),
    /// Backfill complete; grace maintenance remains an independent bounded
    /// step.
    Ready,
}

/// Disposable identity and cursor, atomically committed with every batch.
#[derive(Debug, Clone, Copy, Encode, Decode)]
pub(crate) struct IndexRecord {
    /// Full requested policy and holder.
    generation: RetentionGeneration,
    /// Cleanup/backfill progress.
    stage: IndexStage,
}

/// Reverse row owns exactly one grace row or both ordered candidate rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub(crate) struct IndexEntry {
    /// Full author for the per-author index.
    author: RostraId,
    /// Canonical ordered key including the full verified event ID.
    key: [u8; 48],
    /// Earliest representable eligibility time, including the header origin.
    eligible_at: u64,
    /// Promotion only changes membership, never the static key.
    promoted: bool,
}

/// Advisory selection, not permission to release a payload.
///
/// Phase 3 must recheck the active generation, lifecycle, trusted clock and
/// pressure in the same write transaction as pruning. This value deliberately
/// has no conversion to `QuotaPruneRequest`; the existing explicit quota API
/// does not establish generation or pressure authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionCandidate {
    /// Full event ID recovered from the retained verified envelope.
    pub event: EventId,
    /// Full author owning the logical payload charge.
    pub author: RostraId,
    /// Exact ascending core-policy bytes, including the full event-ID tie.
    pub key: [u8; 48],
}

/// One bounded ordered index page under an explicitly requested generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionCandidates {
    /// Policy and holder to which every returned key belongs.
    pub generation: RetentionGeneration,
    /// False on absent, different, or incomplete generation, or untrusted
    /// clock.
    pub ready: bool,
    /// Index rows visited, including rows rejected after clock rollback.
    pub visited: usize,
    /// Last visited key for continuation, even when no row remained eligible.
    pub scanned_through: Option<[u8; 48]>,
    /// Eligible candidates in ascending key order; never more than the limit.
    pub candidates: Vec<RetentionCandidate>,
}

impl IndexRecord {
    fn progress(self, visited: usize) -> RetentionIndexProgress {
        RetentionIndexProgress {
            generation: self.generation,
            ready: matches!(self.stage, IndexStage::Ready),
            visited,
        }
    }
}

impl Database {
    /// Require complete backfill and an exhausted due prefix at this exact
    /// walltime before an internal pressure operation can choose a minimum.
    pub(crate) fn retention_selection_ready_tx(
        tx: &WriteTransactionCtx,
        generation: RetentionGeneration,
        now: rostra_core::Timestamp,
    ) -> DbResult<bool> {
        let Some(record) = Self::retention_record_tx(tx)? else {
            return Ok(false);
        };
        if record.generation != generation || !matches!(record.stage, IndexStage::Ready) {
            return Ok(false);
        }
        Ok(tx
            .open_table(&grace::TABLE)?
            .first()?
            .map(|(key, _)| key.value_try())
            .transpose()?
            .is_none_or(|(deadline, _)| deadline > now.as_u64()))
    }

    fn retention_limit(limit: NonZeroUsize) -> DbResult<()> {
        if limit.get() > crate::PAYLOAD_MAINTENANCE_MAX {
            return crate::PayloadMaintenanceLimitSnafu.fail();
        }
        Ok(())
    }

    fn retention_record_tx(tx: &WriteTransactionCtx) -> DbResult<Option<IndexRecord>> {
        tx.open_table(&state::TABLE)?
            .get(&())?
            .map(|row| row.value_try())
            .transpose()
            .map_err(Into::into)
    }

    /// Request a policy generation and immediately invalidate a different one.
    ///
    /// No rows are scanned here. Repeated requests for the same identity resume
    /// existing progress; changes first perform bounded reverse-owned cleanup.
    /// The holder is always this database's account. Reopening for another
    /// account is rejected by the database identity boundary.
    pub async fn configure_retention_index(
        &self,
        policy: RetentionPolicy,
    ) -> DbResult<RetentionIndexProgress> {
        self.write_with(|tx| {
            let generation = RetentionGeneration::new(policy, self.self_id);
            if Self::retention_record_tx(tx)?.is_none_or(|record| record.generation != generation) {
                let ledger = self.payload_admission.clone();
                tx.on_commit(move || {
                    ledger.demands.lock().unwrap().clear();
                    ledger.changed.notify_waiters();
                });
            }
            let record = Self::retention_record_tx(tx)?
                .filter(|record| record.generation == generation)
                .unwrap_or(IndexRecord {
                    generation,
                    stage: IndexStage::Clearing,
                });
            tx.open_table(&state::TABLE)?.insert(&(), &record)?;
            Ok(record.progress(0))
        })
        .await
    }

    /// Read generation/backfill readiness independently of accounting and GC.
    pub async fn retention_index_progress(&self) -> DbResult<Option<RetentionIndexProgress>> {
        self.read_with(|tx| {
            Ok(tx
                .open_table(&state::TABLE)?
                .get(&())?
                .map(|row| row.value_try())
                .transpose()?
                .map(|record| record.progress(0)))
        })
        .await
    }

    /// Recheck an advisory selection against the current generation and clock.
    ///
    /// This result is itself only a snapshot, not pressure authorization. A
    /// destructive worker must use the transactional counterpart together with
    /// its pressure/reservation checks and the checked quota reducer.
    pub async fn retention_candidate_is_current(
        &self,
        generation: RetentionGeneration,
        candidate: RetentionCandidate,
        clock: RetentionClock,
    ) -> DbResult<bool> {
        self.write_with(|tx| self.retention_candidate_current_tx(tx, generation, candidate, clock))
            .await
    }

    pub(crate) fn retention_candidate_current_tx(
        &self,
        tx: &WriteTransactionCtx,
        generation: RetentionGeneration,
        candidate: RetentionCandidate,
        clock: RetentionClock,
    ) -> DbResult<bool> {
        let Some(record) = Self::retention_record_tx(tx)? else {
            return Ok(false);
        };
        if record.generation != generation || !matches!(record.stage, IndexStage::Ready) {
            return Ok(false);
        }
        let RetentionClock::Trusted(now) = clock else {
            return Ok(false);
        };
        let id = candidate.event.to_short();
        let Some((mut entry, event)) = self.derive_retention_entry_tx(tx, generation, id)? else {
            return Ok(false);
        };
        if event != candidate.event
            || entry.author != candidate.author
            || entry.key != candidate.key
            || now.as_u64() < entry.eligible_at
        {
            return Ok(false);
        }
        entry.promoted = true;
        let indexed = tx
            .open_table(&reverse::TABLE)?
            .get(&id)?
            .map(|row| row.value_try())
            .transpose()?;
        if indexed != Some(entry) {
            return Ok(false);
        }
        let global_id = tx
            .open_table(&global::TABLE)?
            .get(&entry.key)?
            .map(|row| row.value_try())
            .transpose()?;
        let author_id = tx
            .open_table(&authors::TABLE)?
            .get(&(entry.author, entry.key))?
            .map(|row| row.value_try())
            .transpose()?;
        Ok(global_id == Some(id) && author_id == Some(id))
    }

    fn remove_retention_entry_tx(tx: &WriteTransactionCtx, id: ShortEventId) -> DbResult<()> {
        let entry = tx
            .open_table(&reverse::TABLE)?
            .remove(&id)?
            .map(|row| row.value_try())
            .transpose()?;
        if let Some(entry) = entry {
            if entry.promoted {
                let global_id = tx
                    .open_table(&global::TABLE)?
                    .remove(&entry.key)?
                    .map(|row| row.value_try())
                    .transpose()?;
                let author_id = tx
                    .open_table(&authors::TABLE)?
                    .remove(&(entry.author, entry.key))?
                    .map(|row| row.value_try())
                    .transpose()?;
                if global_id != Some(id) || author_id != Some(id) {
                    return crate::PayloadAccountingInvariantSnafu.fail();
                }
            } else {
                if tx
                    .open_table(&grace::TABLE)?
                    .remove(&(entry.eligible_at, id))?
                    .is_none()
                {
                    return crate::PayloadAccountingInvariantSnafu.fail();
                }
            }
        }
        Ok(())
    }

    fn derive_retention_entry_tx(
        &self,
        tx: &WriteTransactionCtx,
        generation: RetentionGeneration,
        id: ShortEventId,
    ) -> DbResult<Option<(IndexEntry, EventId)>> {
        if generation.holder != self.self_id {
            return crate::PayloadAccountingInvariantSnafu.fail();
        }
        let policy = generation.policy()?;
        let Some(event) = tx
            .open_table(&crate::events::TABLE)?
            .get(&id)?
            .map(|row| row.value_try())
            .transpose()?
        else {
            return Ok(None);
        };
        if event.author() == self.self_id
            || event.kind() != EventKind::SOCIAL_POST
            || event.content_len() == 0
            || tx
                .open_table(&crate::events_content_state::TABLE)?
                .get(&id)?
                .is_some()
        {
            return Ok(None);
        }
        let Some(origins) = tx
            .open_table(&crate::events_retention_origins::TABLE)?
            .get(&id)?
            .map(|row| row.value_try())
            .transpose()?
        else {
            return Ok(None);
        };
        let Some(first) = origins.materialized_at else {
            return Ok(None);
        };
        let Some(deadline) = first
            .as_u64()
            .checked_add(u64::from(policy.grace_seconds()))
        else {
            return Ok(None);
        };
        let verified = VerifiedEvent::assume_verified_from_signed(event.signed);
        if verified.event_id.to_short() != id {
            return crate::PayloadAccountingInvariantSnafu.fail();
        }
        Ok(Some((
            IndexEntry {
                author: verified.author(),
                key: policy
                    .key(
                        verified.event_id,
                        self.self_id,
                        verified.content_len(),
                        origins.effective_timestamp,
                    )
                    .to_bytes(),
                eligible_at: deadline.max(origins.effective_timestamp.as_u64()),
                promoted: false,
            },
            verified.event_id,
        )))
    }

    pub(crate) fn refresh_retention_index_tx(
        &self,
        tx: &WriteTransactionCtx,
        id: ShortEventId,
    ) -> DbResult<()> {
        let Some(record) = Self::retention_record_tx(tx)? else {
            return Ok(());
        };
        if matches!(record.stage, IndexStage::Clearing) {
            return Ok(());
        }
        let next = self
            .derive_retention_entry_tx(tx, record.generation, id)?
            .map(|(entry, _)| entry);
        let old = tx
            .open_table(&reverse::TABLE)?
            .get(&id)?
            .map(|row| row.value_try())
            .transpose()?;
        if let (Some(mut old), Some(next)) = (old, next) {
            old.promoted = false;
            if old == next {
                return Ok(());
            }
        }
        Self::remove_retention_entry_tx(tx, id)?;
        if let Some(entry) = next {
            tx.open_table(&grace::TABLE)?
                .insert(&(entry.eligible_at, id), &())?;
            tx.open_table(&reverse::TABLE)?.insert(&id, &entry)?;
        }
        Ok(())
    }

    /// Resume cleanup/backfill, visiting at most `limit` rows (maximum 4096).
    ///
    /// No payload values are read. Unknown legacy origins stay protected.
    /// Calls survive interruption/reopen; total replay discards this generation
    /// and requires explicit configuration and rebuilding again.
    pub async fn rebuild_retention_index(
        &self,
        limit: NonZeroUsize,
    ) -> DbResult<Option<RetentionIndexProgress>> {
        Self::retention_limit(limit)?;
        self.write_with(|tx| {
            let Some(mut record) = Self::retention_record_tx(tx)? else {
                return Ok(None);
            };
            let mut visited = 0;
            while visited < limit.get() {
                match record.stage {
                    IndexStage::Clearing => {
                        let next = tx
                            .open_table(&reverse::TABLE)?
                            .first()?
                            .map(|(key, _)| key.value_try())
                            .transpose()?;
                        let Some(id) = next else {
                            record.stage = IndexStage::Events(None);
                            tx.open_table(&state::TABLE)?.insert(&(), &record)?;
                            continue;
                        };
                        Self::remove_retention_entry_tx(tx, id)?;
                    }
                    IndexStage::Events(cursor) => {
                        let next = tx
                            .open_table(&crate::events::TABLE)?
                            .range((cursor.map_or(Unbounded, Excluded), Unbounded))?
                            .next()
                            .transpose()?
                            .map(|(key, _)| key.value_try())
                            .transpose()?;
                        let Some(id) = next else {
                            record.stage = IndexStage::Ready;
                            break;
                        };
                        self.refresh_retention_index_tx(tx, id)?;
                        record.stage = IndexStage::Events(Some(id));
                    }
                    IndexStage::Ready => break,
                }
                visited += 1;
            }
            tx.open_table(&state::TABLE)?.insert(&(), &record)?;
            Ok(Some(record.progress(visited)))
        })
        .await
    }

    /// Promote at most `limit` due grace rows without rescoring retained
    /// events.
    ///
    /// A trusted clock is an explicit caller assertion, never inferred here.
    /// Rollback does not require rescanning/demoting previously promoted rows:
    /// selection and destructive callers must recheck their eligibility time.
    /// `ready` describes backfill only. At a trusted fixed timestamp, a ready
    /// call visiting fewer than `limit` rows has exhausted the due prefix;
    /// drain that prefix before selecting the minimum of all eligible events.
    pub async fn promote_retention_grace(
        &self,
        clock: RetentionClock,
        limit: NonZeroUsize,
    ) -> DbResult<Option<RetentionIndexProgress>> {
        Self::retention_limit(limit)?;
        self.write_with(|tx| {
            let Some(record) = Self::retention_record_tx(tx)? else {
                return Ok(None);
            };
            let RetentionClock::Trusted(now) = clock else {
                return Ok(Some(record.progress(0)));
            };
            if !matches!(record.stage, IndexStage::Ready) {
                return Ok(Some(record.progress(0)));
            }
            let mut visited = 0;
            while visited < limit.get() {
                let next = tx
                    .open_table(&grace::TABLE)?
                    .first()?
                    .map(|(key, _)| key.value_try())
                    .transpose()?;
                let Some((deadline, id)) = next.filter(|(deadline, _)| *deadline <= now.as_u64())
                else {
                    break;
                };
                let mut entry = tx
                    .open_table(&reverse::TABLE)?
                    .get(&id)?
                    .context(crate::PayloadAccountingInvariantSnafu)?
                    .value_try()?;
                if entry.promoted || entry.eligible_at != deadline {
                    return crate::PayloadAccountingInvariantSnafu.fail();
                }
                tx.open_table(&grace::TABLE)?.remove(&(deadline, id))?;
                entry.promoted = true;
                tx.open_table(&global::TABLE)?.insert(&entry.key, &id)?;
                tx.open_table(&authors::TABLE)?
                    .insert(&(entry.author, entry.key), &id)?;
                tx.open_table(&reverse::TABLE)?.insert(&id, &entry)?;
                visited += 1;
            }
            Ok(Some(record.progress(visited)))
        })
        .await
    }

    /// Read a bounded author/global ordered advisory page, never a payload
    /// scan.
    ///
    /// `after` is exclusive and scoped to this generation and author filter.
    /// Each row visit costs one unit even if clock rollback makes it
    /// ineligible. Restart pagination after policy changes or when
    /// revisiting earlier keys after grace promotion. A page is only a
    /// transaction-local snapshot.
    pub async fn select_retention_candidates(
        &self,
        generation: RetentionGeneration,
        author: Option<RostraId>,
        clock: RetentionClock,
        after: Option<[u8; 48]>,
        limit: NonZeroUsize,
    ) -> DbResult<RetentionCandidates> {
        Self::retention_limit(limit)?;
        // The writer transaction allows phase 3 to reuse exactly this selection
        // boundary together with its pressure and lifecycle rechecks.
        self.write_with(|tx| {
            let mut page = RetentionCandidates {
                generation,
                ready: false,
                visited: 0,
                scanned_through: after,
                candidates: Vec::new(),
            };
            let Some(record) = Self::retention_record_tx(tx)? else {
                return Ok(page);
            };
            let RetentionClock::Trusted(now) = clock else {
                return Ok(page);
            };
            if record.generation != generation || !matches!(record.stage, IndexStage::Ready) {
                return Ok(page);
            }
            page.ready = true;
            let rows: Vec<([u8; 48], ShortEventId)> = if let Some(author) = author {
                tx.open_table(&authors::TABLE)?
                    .range((
                        after.map_or(Included((author, [0; 48])), |key| Excluded((author, key))),
                        Included((author, [255; 48])),
                    ))?
                    .take(limit.get())
                    .map(|row| {
                        let (key, value) = row?;
                        Ok((key.value_try()?.1, value.value_try()?))
                    })
                    .collect::<DbResult<_>>()?
            } else {
                tx.open_table(&global::TABLE)?
                    .range((after.map_or(Unbounded, Excluded), Unbounded))?
                    .take(limit.get())
                    .map(|row| {
                        let (key, value) = row?;
                        Ok((key.value_try()?, value.value_try()?))
                    })
                    .collect::<DbResult<_>>()?
            };
            for (key, id) in rows {
                page.visited += 1;
                page.scanned_through = Some(key);
                let Some((entry, event)) = self.derive_retention_entry_tx(tx, generation, id)?
                else {
                    return crate::PayloadAccountingInvariantSnafu.fail();
                };
                if entry.key != key || author.is_some_and(|author| author != entry.author) {
                    return crate::PayloadAccountingInvariantSnafu.fail();
                }
                if entry.eligible_at <= now.as_u64() {
                    page.candidates.push(RetentionCandidate {
                        event,
                        author: entry.author,
                        key,
                    });
                }
            }
            Ok(page)
        })
        .await
    }
}
