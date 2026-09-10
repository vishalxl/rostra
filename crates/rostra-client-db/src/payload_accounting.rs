//! Transactional payload accounting and quota-scoped physical reclamation.
//!
//! Event RC includes Missing references. A separate header-derived guard covers
//! historical local and non-social references even after they release their RC.
//! Only quota lifecycle provenance may authorize GC, including later releases
//! and bounded reconstruction of that provenance from immutable quota rows.

use std::num::NonZeroUsize;
use std::ops::Bound::{Excluded, Unbounded};

use bincode::{Decode, Encode};
use rostra_core::event::{EventExt as _, EventKind};
use rostra_core::{ContentHash, ShortEventId};
use snafu::OptionExt as _;

use crate::{
    Database, DbResult, EventContentState, OverflowSnafu, WriteTransactionCtx,
    content_accounting_authors as payload_accounting_authors,
    content_accounting_hashes as payload_accounting_hashes,
    content_accounting_state as payload_accounting, content_quota_gc as payload_gc, content_rc,
    content_store, events, events_content_state, ids_data_usage,
};

/// Maximum records visited by one accounting or garbage-collection call.
pub const PAYLOAD_MAINTENANCE_MAX: usize = 4096;

/// Exact payload totals, excluding envelopes, indexes and database file
/// overhead.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub struct PayloadUsage {
    /// Sum of processed event lengths; shared hashes count once per event.
    pub logical_current_bytes: u64,
    /// Sum of actual lengths of unique values in the content store.
    pub unique_stored_bytes: u64,
}

/// Result of one bounded maintenance transaction.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PayloadMaintenance {
    /// Number of source or queue rows visited, not bytes examined.
    pub visited: usize,
    /// Whether this operation's rebuild is complete (accounting or
    /// nominations).
    pub ready: bool,
    /// Actual unique content bytes removed by this call, never logical
    /// releases.
    pub removed_bytes: u64,
}

/// Durable rebuild cursor. Partial totals are never exposed as usable.
#[derive(Debug, Clone, Copy, Encode, Decode)]
pub(crate) enum AccountingStage {
    Events(Option<ShortEventId>),
    Content(Option<ContentHash>),
    Hashes(Option<ContentHash>),
    References(Option<ContentHash>),
    Authors(Option<rostra_core::id::RostraId>),
    ExpectedAuthors(Option<rostra_core::id::RostraId>),
    Ready,
}

/// One generation of disposable accounting, independent of policy indexes.
#[derive(Debug, Clone, Copy, Encode, Decode)]
pub(crate) struct AccountingRecord {
    /// Cursor and readiness for this schema's accounting contract.
    pub(crate) stage: AccountingStage,
    /// Partial during backfill; exact only at Ready.
    pub(crate) usage: PayloadUsage,
}

impl Default for AccountingRecord {
    fn default() -> Self {
        Self {
            stage: AccountingStage::Events(None),
            usage: PayloadUsage::default(),
        }
    }
}

/// Expected RC reconstructed from retained events and a historical replay
/// guard.
#[derive(Debug, Default, Clone, Copy, Encode, Decode)]
pub(crate) struct HashAccounting {
    /// Missing and Processed event references, independently checked against
    /// RC.
    pub(crate) references: u64,
    /// A retained local or non-SocialPost header references this hash.
    pub(crate) protected_history: bool,
}

/// Independent resumable scan of immutable quota decisions.
#[derive(Debug, Clone, Copy, Encode, Decode)]
pub(crate) enum QuotaRecovery {
    /// Last visited authoritative quota row.
    Scanning(Option<ShortEventId>),
    /// All historical nominations have been reconstructed.
    Ready,
}

/// Small lifecycle snapshot captured before a reducer changes an event.
pub(crate) struct PayloadBefore {
    /// Events and their previous contribution, including an auxiliary parent.
    events: Vec<(ShortEventId, Option<EventContribution>)>,
    /// Actual stored lengths for hashes the reducer can write.
    hashes: Vec<(ContentHash, u64)>,
}

/// An event's current contribution; terminal events still carry replay guards.
struct EventContribution {
    /// Full author identity used for logical accounting.
    author: rostra_core::id::RostraId,
    /// Shared content identifier.
    hash: ContentHash,
    /// Whether this event still owns one reference.
    referenced: bool,
    /// Processed event length, otherwise zero.
    logical: u64,
    /// Historical header's conservative replay protection.
    protected: bool,
}

impl Database {
    pub(crate) fn require_payload_accounting_ready_tx(tx: &WriteTransactionCtx) -> DbResult<()> {
        if !matches!(Self::accounting_tx(tx)?.stage, AccountingStage::Ready) {
            return crate::PayloadAccountingNotReadySnafu.fail();
        }
        Ok(())
    }

    pub(crate) fn nominate_quota_hash_tx(
        tx: &WriteTransactionCtx,
        hash: ContentHash,
    ) -> DbResult<()> {
        tx.open_table(&crate::content_quota_hashes::TABLE)?
            .insert(&hash, &())?;
        tx.open_table(&payload_gc::TABLE)?.insert(&hash, &())?;
        Ok(())
    }

    /// Reconstruct quota-only GC provenance and work in bounded, resumable
    /// batches.
    ///
    /// Call after replay or upgrade, even if accounting is already ready.
    /// Concurrent quota transitions nominate atomically; later reference
    /// releases requeue hashes whose provenance has already been scanned.
    /// Unscanned quota rows will nominate their hashes when visited. No
    /// worker is started here.
    pub async fn rebuild_quota_payload_nominations(
        &self,
        limit: NonZeroUsize,
    ) -> DbResult<PayloadMaintenance> {
        Self::check_payload_limit(limit)?;
        self.write_with(|tx| self.rebuild_quota_payload_nominations_tx(tx, limit))
            .await
    }

    pub(crate) fn rebuild_quota_payload_nominations_tx(
        &self,
        tx: &WriteTransactionCtx,
        limit: NonZeroUsize,
    ) -> DbResult<PayloadMaintenance> {
        let mut stage = tx
            .open_table(&crate::content_quota_recovery::TABLE)?
            .get(&())?
            .map(|row| row.value_try())
            .transpose()?
            .unwrap_or(QuotaRecovery::Scanning(None));
        let mut result = PayloadMaintenance::default();
        while result.visited < limit.get() {
            let QuotaRecovery::Scanning(cursor) = stage else {
                break;
            };
            let next = tx
                .open_table(&crate::events_quota_pruned::TABLE)?
                .range((cursor.map_or(Unbounded, Excluded), Unbounded))?
                .next()
                .transpose()?
                .map(|(key, value)| Ok::<_, crate::DbError>((key.value_try()?, value.value_try()?)))
                .transpose()?;
            let Some((id, _decision)) = next else {
                stage = QuotaRecovery::Ready;
                break;
            };
            let event = tx
                .open_table(&events::TABLE)?
                .get(&id)?
                .context(crate::PayloadAccountingInvariantSnafu)?
                .value_try()?;
            let state = tx
                .open_table(&events_content_state::TABLE)?
                .get(&id)?
                .context(crate::PayloadAccountingInvariantSnafu)?
                .value_try()?;
            if event.author() == self.self_id
                || event.kind() != EventKind::SOCIAL_POST
                || !matches!(
                    state,
                    EventContentState::Pruned | EventContentState::Deleted { .. }
                )
            {
                return crate::PayloadAccountingInvariantSnafu.fail();
            }
            Self::nominate_quota_hash_tx(tx, event.content_hash())?;
            stage = QuotaRecovery::Scanning(Some(id));
            result.visited += 1;
        }
        result.ready = matches!(stage, QuotaRecovery::Ready);
        tx.open_table(&crate::content_quota_recovery::TABLE)?
            .insert(&(), &stage)?;
        Ok(result)
    }

    fn accounting_tx(tx: &WriteTransactionCtx) -> DbResult<AccountingRecord> {
        Ok(tx
            .open_table(&payload_accounting::TABLE)?
            .get(&())?
            .map(|row| row.value_try())
            .transpose()?
            .unwrap_or_default())
    }

    fn contribution_tx(
        &self,
        tx: &WriteTransactionCtx,
        id: ShortEventId,
    ) -> DbResult<Option<EventContribution>> {
        let Some(event) = tx
            .open_table(&events::TABLE)?
            .get(&id)?
            .map(|row| row.value_try())
            .transpose()?
        else {
            return Ok(None);
        };
        let state = tx
            .open_table(&events_content_state::TABLE)?
            .get(&id)?
            .map(|row| row.value_try())
            .transpose()?;
        if state.is_none() {
            let stored = tx.open_table(&content_store::TABLE)?;
            let stored = stored
                .get(&event.content_hash())?
                .context(crate::PayloadAccountingInvariantSnafu)?
                .value_try()?;
            if stored.0.as_slice().len() as u64 != u64::from(event.content_len()) {
                return crate::PayloadAccountingInvariantSnafu.fail();
            }
        }
        Ok(Some(EventContribution {
            author: event.author(),
            hash: event.content_hash(),
            referenced: matches!(state, None | Some(EventContentState::Missing { .. })),
            logical: if state.is_none() {
                u64::from(event.content_len())
            } else {
                0
            },
            protected: event.author() == self.self_id || event.kind() != EventKind::SOCIAL_POST,
        }))
    }

    fn event_counted(stage: AccountingStage, id: ShortEventId) -> bool {
        match stage {
            AccountingStage::Events(cursor) => cursor.is_some_and(|cursor| id <= cursor),
            _ => true,
        }
    }

    fn hash_counted(stage: AccountingStage, hash: ContentHash) -> bool {
        match stage {
            AccountingStage::Events(_) => false,
            AccountingStage::Content(cursor) => cursor.is_some_and(|cursor| hash <= cursor),
            _ => true,
        }
    }

    fn stored_len_tx(tx: &WriteTransactionCtx, hash: ContentHash) -> DbResult<u64> {
        Ok(tx
            .open_table(&content_store::TABLE)?
            .get(&hash)?
            .map(|row| row.value_try().map(|value| value.0.as_slice().len() as u64))
            .transpose()?
            .unwrap_or(0))
    }

    /// Capture all events and store values that an envelope/content reducer can
    /// affect. Its matching finish must run before the transaction commits.
    pub(crate) fn payload_before_tx(
        &self,
        tx: &WriteTransactionCtx,
        event: &rostra_core::event::VerifiedEvent,
    ) -> DbResult<PayloadBefore> {
        use rostra_core::id::ToShort as _;
        let mut ids = vec![event.event_id.to_short()];
        if let Some(parent) = event.parent_aux()
            && parent != ids[0]
        {
            ids.push(parent);
        }
        let mut events = Vec::new();
        let mut hashes = vec![event.content_hash()];
        for id in ids {
            let old = self.contribution_tx(tx, id)?;
            if let Some(old) = &old
                && !hashes.contains(&old.hash)
            {
                hashes.push(old.hash);
            }
            events.push((id, old));
        }
        Ok(PayloadBefore {
            events,
            hashes: hashes
                .into_iter()
                .map(|hash| Ok((hash, Self::stored_len_tx(tx, hash)?)))
                .collect::<DbResult<_>>()?,
        })
    }

    fn adjust_contribution_tx(
        tx: &WriteTransactionCtx,
        accounting: &mut AccountingRecord,
        contribution: &EventContribution,
        add: bool,
    ) -> DbResult<()> {
        let mut hashes = tx.open_table(&payload_accounting_hashes::TABLE)?;
        let mut hash = hashes
            .get(&contribution.hash)?
            .map(|row| row.value_try())
            .transpose()?
            .unwrap_or_default();
        if contribution.referenced {
            hash.references = if add {
                hash.references.checked_add(1)
            } else {
                hash.references.checked_sub(1)
            }
            .context(OverflowSnafu)?;
        }
        // Headers are retained forever; lifecycle changes cannot release this
        // guard. It is not RC and does not count as current logical usage.
        hash.protected_history |= contribution.protected;
        hashes.insert(&contribution.hash, &hash)?;
        let mut authors = tx.open_table(&payload_accounting_authors::TABLE)?;
        let previous = authors
            .get(&contribution.author)?
            .map(|row| row.value_try())
            .transpose()?
            .unwrap_or(0);
        let (author, global) = if add {
            (
                previous.checked_add(contribution.logical),
                accounting
                    .usage
                    .logical_current_bytes
                    .checked_add(contribution.logical),
            )
        } else {
            (
                previous.checked_sub(contribution.logical),
                accounting
                    .usage
                    .logical_current_bytes
                    .checked_sub(contribution.logical),
            )
        };
        authors.insert(&contribution.author, &author.context(OverflowSnafu)?)?;
        accounting.usage.logical_current_bytes = global.context(OverflowSnafu)?;
        Ok(())
    }

    fn validate_hash_tx(tx: &WriteTransactionCtx, hash: ContentHash) -> DbResult<()> {
        let expected = tx
            .open_table(&payload_accounting_hashes::TABLE)?
            .get(&hash)?
            .map(|row| row.value_try())
            .transpose()?
            .unwrap_or_default()
            .references;
        let actual = tx
            .open_table(&content_rc::TABLE)?
            .get(&hash)?
            .map(|row| row.value_try())
            .transpose()?;
        if actual == Some(0) || actual.unwrap_or(0) != expected {
            return crate::PayloadAccountingInvariantSnafu.fail();
        }
        Ok(())
    }

    fn validate_author_tx(
        tx: &WriteTransactionCtx,
        author: rostra_core::id::RostraId,
    ) -> DbResult<()> {
        let expected = tx
            .open_table(&payload_accounting_authors::TABLE)?
            .get(&author)?
            .map(|row| row.value_try())
            .transpose()?
            .unwrap_or(0);
        let actual = tx
            .open_table(&ids_data_usage::TABLE)?
            .get(&author)?
            .map(|row| row.value_try())
            .transpose()?;
        if actual.map(|usage| usage.current_content_size) != Some(expected) {
            return crate::PayloadAccountingInvariantSnafu.fail();
        }
        Ok(())
    }

    /// Finish a lifecycle reducer's accounting in the same transaction.
    pub(crate) fn payload_after_tx(
        &self,
        tx: &WriteTransactionCtx,
        before: PayloadBefore,
    ) -> DbResult<()> {
        let mut accounting = Self::accounting_tx(tx)?;
        let mut authors = Vec::new();
        for (id, old) in before.events {
            self.release_completed_admission_tx(tx, id)?;
            self.refresh_retention_index_tx(tx, id)?;
            if Self::event_counted(accounting.stage, id) {
                let new = self.contribution_tx(tx, id)?;
                if old.as_ref().map(|e| (e.author, e.logical))
                    != new.as_ref().map(|e| (e.author, e.logical))
                {
                    self.payload_admission.invalidate_pressure();
                }
                if let Some(old) = old {
                    Self::adjust_contribution_tx(tx, &mut accounting, &old, false)?;
                }
                if let Some(new) = new {
                    if !authors.contains(&new.author) {
                        authors.push(new.author);
                    }
                    Self::adjust_contribution_tx(tx, &mut accounting, &new, true)?;
                }
            }
        }
        if matches!(accounting.stage, AccountingStage::Ready) {
            for author in authors {
                Self::validate_author_tx(tx, author)?;
            }
        }
        for (hash, previous_len) in before.hashes {
            if Self::hash_counted(accounting.stage, hash) {
                let current_len = Self::stored_len_tx(tx, hash)?;
                accounting.usage.unique_stored_bytes = accounting
                    .usage
                    .unique_stored_bytes
                    .checked_sub(previous_len)
                    .and_then(|value| value.checked_add(current_len))
                    .context(OverflowSnafu)?;
            }
            if matches!(accounting.stage, AccountingStage::Ready) {
                Self::validate_hash_tx(tx, hash)?;
            }
            // Provenance, unlike queue membership, survives a blocked collection.
            // A final signed-delete/invalid release may complete quota-owned work,
            // but unrelated legacy garbage must never gain nomination authority.
            if tx
                .open_table(&crate::content_quota_hashes::TABLE)?
                .get(&hash)?
                .is_some()
                && tx.open_table(&content_rc::TABLE)?.get(&hash)?.is_none()
            {
                tx.open_table(&payload_gc::TABLE)?.insert(&hash, &())?;
            }
        }
        tx.open_table(&payload_accounting::TABLE)?
            .insert(&(), &accounting)?;
        Ok(())
    }

    /// Return exact logical and unique-byte totals, or None while rebuilding.
    ///
    /// Readiness here does not imply readiness of policy candidate
    /// indexes.
    pub async fn get_payload_usage(&self) -> DbResult<Option<PayloadUsage>> {
        self.read_with(|tx| {
            Ok(tx
                .open_table(&payload_accounting::TABLE)?
                .get(&())?
                .map(|row| row.value_try())
                .transpose()?
                .filter(|record| matches!(record.stage, AccountingStage::Ready))
                .map(|record| record.usage))
        })
        .await
    }

    pub(crate) fn payload_usage_tx(tx: &WriteTransactionCtx) -> DbResult<Option<PayloadUsage>> {
        let accounting = Self::accounting_tx(tx)?;
        Ok(matches!(accounting.stage, AccountingStage::Ready).then_some(accounting.usage))
    }

    fn check_payload_limit(limit: NonZeroUsize) -> DbResult<()> {
        if PAYLOAD_MAINTENANCE_MAX < limit.get() {
            return crate::PayloadMaintenanceLimitSnafu.fail();
        }
        Ok(())
    }

    /// Resume a bounded, transactional accounting and replay-guard rebuild.
    ///
    /// Ordinary ingestion may interleave with calls. Cursor-covered deltas are
    /// applied immediately; later keys are counted by their future scan. No
    /// wall clock or retention origin is manufactured. Each visited row
    /// costs one unit, including validation; there is no unbounded
    /// end-of-scan cleanup.
    pub async fn rebuild_payload_accounting(
        &self,
        limit: NonZeroUsize,
    ) -> DbResult<PayloadMaintenance> {
        Self::check_payload_limit(limit)?;
        self.write_with(|tx| self.rebuild_payload_accounting_tx(tx, limit))
            .await
    }

    pub(crate) fn rebuild_payload_accounting_tx(
        &self,
        tx: &WriteTransactionCtx,
        limit: NonZeroUsize,
    ) -> DbResult<PayloadMaintenance> {
        let mut record = Self::accounting_tx(tx)?;
        let mut result = PayloadMaintenance::default();
        while result.visited < limit.get() {
            match record.stage {
                AccountingStage::Events(cursor) => {
                    let next = tx
                        .open_table(&events::TABLE)?
                        .range((cursor.map_or(Unbounded, Excluded), Unbounded))?
                        .next()
                        .transpose()?
                        .map(|(key, _)| key.value_try())
                        .transpose()?;
                    let Some(id) = next else {
                        record.stage = AccountingStage::Content(None);
                        continue;
                    };
                    let contribution = self
                        .contribution_tx(tx, id)?
                        .context(crate::PayloadAccountingInvariantSnafu)?;
                    Self::adjust_contribution_tx(tx, &mut record, &contribution, true)?;
                    record.stage = AccountingStage::Events(Some(id));
                }
                AccountingStage::Content(cursor) => {
                    let next = tx
                        .open_table(&content_store::TABLE)?
                        .range((cursor.map_or(Unbounded, Excluded), Unbounded))?
                        .next()
                        .transpose()?
                        .map(|(key, value)| {
                            Ok::<_, crate::DbError>((
                                key.value_try()?,
                                value.value_try()?.0.as_slice().len() as u64,
                            ))
                        })
                        .transpose()?;
                    let Some((hash, len)) = next else {
                        record.stage = AccountingStage::Hashes(None);
                        continue;
                    };
                    record.usage.unique_stored_bytes = record
                        .usage
                        .unique_stored_bytes
                        .checked_add(len)
                        .context(OverflowSnafu)?;
                    Self::validate_hash_tx(tx, hash)?;
                    record.stage = AccountingStage::Content(Some(hash));
                }
                AccountingStage::Hashes(cursor) => {
                    let next = tx
                        .open_table(&payload_accounting_hashes::TABLE)?
                        .range((cursor.map_or(Unbounded, Excluded), Unbounded))?
                        .next()
                        .transpose()?
                        .map(|(key, _)| key.value_try())
                        .transpose()?;
                    let Some(hash) = next else {
                        record.stage = AccountingStage::References(None);
                        continue;
                    };
                    Self::validate_hash_tx(tx, hash)?;
                    record.stage = AccountingStage::Hashes(Some(hash));
                }
                AccountingStage::References(cursor) => {
                    let next = tx
                        .open_table(&content_rc::TABLE)?
                        .range((cursor.map_or(Unbounded, Excluded), Unbounded))?
                        .next()
                        .transpose()?
                        .map(|(key, _)| key.value_try())
                        .transpose()?;
                    let Some(hash) = next else {
                        record.stage = AccountingStage::Authors(None);
                        continue;
                    };
                    Self::validate_hash_tx(tx, hash)?;
                    record.stage = AccountingStage::References(Some(hash));
                }
                AccountingStage::Authors(cursor) => {
                    let next = tx
                        .open_table(&ids_data_usage::TABLE)?
                        .range((cursor.map_or(Unbounded, Excluded), Unbounded))?
                        .next()
                        .transpose()?
                        .map(|(key, value)| {
                            Ok::<_, crate::DbError>((key.value_try()?, value.value_try()?))
                        })
                        .transpose()?;
                    let Some((author, usage)) = next else {
                        record.stage = AccountingStage::ExpectedAuthors(None);
                        continue;
                    };
                    let expected = tx
                        .open_table(&payload_accounting_authors::TABLE)?
                        .get(&author)?
                        .map(|row| row.value_try())
                        .transpose()?
                        .unwrap_or(0);
                    if expected != usage.current_content_size {
                        return crate::PayloadAccountingInvariantSnafu.fail();
                    }
                    record.stage = AccountingStage::Authors(Some(author));
                }
                AccountingStage::ExpectedAuthors(cursor) => {
                    let next = tx
                        .open_table(&payload_accounting_authors::TABLE)?
                        .range((cursor.map_or(Unbounded, Excluded), Unbounded))?
                        .next()
                        .transpose()?
                        .map(|(key, _)| key.value_try())
                        .transpose()?;
                    let Some(author) = next else {
                        record.stage = AccountingStage::Ready;
                        continue;
                    };
                    Self::validate_author_tx(tx, author)?;
                    record.stage = AccountingStage::ExpectedAuthors(Some(author));
                }
                AccountingStage::Ready => break,
            }
            result.visited += 1;
        }
        result.ready = matches!(record.stage, AccountingStage::Ready);
        tx.open_table(&payload_accounting::TABLE)?
            .insert(&(), &record)?;
        Ok(result)
    }

    /// Collect at most `limit` quota-nominated hashes in one transaction.
    ///
    /// This is not a general collector of signed-deleted, invalid or
    /// legacy-pruned bytes: a quota release must have nominated the hash.
    /// Rebuild readiness, actual RC and historical replay guards are rechecked
    /// before every removal. Shared Missing references protect bytes too.
    ///
    /// A blocked nomination is consumed; retained quota-hash provenance lets
    /// subsequent final reference releases nominate it again. Logical
    /// usage never changes here. Bounded work does not bound total stored
    /// bytes.
    pub async fn collect_quota_payload_garbage(
        &self,
        limit: NonZeroUsize,
    ) -> DbResult<PayloadMaintenance> {
        Self::check_payload_limit(limit)?;
        self.write_with(|tx| Self::collect_quota_payload_garbage_tx(tx, limit))
            .await
    }

    pub(crate) fn collect_quota_payload_garbage_tx(
        tx: &WriteTransactionCtx,
        limit: NonZeroUsize,
    ) -> DbResult<PayloadMaintenance> {
        Self::require_payload_accounting_ready_tx(tx)?;
        let mut result = PayloadMaintenance {
            ready: true,
            ..Default::default()
        };
        for _ in 0..limit.get() {
            let (_, step) = Self::collect_quota_payload_step_tx(tx, None, u64::MAX)?;
            if step.visited == 0 {
                break;
            }
            result.visited += step.visited;
            result.removed_bytes = result
                .removed_bytes
                .checked_add(step.removed_bytes)
                .context(OverflowSnafu)?;
        }
        Ok(result)
    }

    /// Visit one quota nomination with a strict unique-store removal allowance.
    /// Oversized values remain nominated; the exclusive cursor lets smaller
    /// hashes behind them progress. The caller retries a completed sweep only
    /// after waiting. Reading/validating one value remains indivisible.
    pub(crate) async fn collect_quota_payload_step(
        &self,
        after: Option<ContentHash>,
        max_bytes: u64,
    ) -> DbResult<(Option<ContentHash>, PayloadMaintenance)> {
        self.write_with(|tx| Self::collect_quota_payload_step_tx(tx, after, max_bytes))
            .await
    }

    /// Atomically validate and visit one quota-only nomination within a strict
    /// unique-store removal allowance, retaining oversized work for later
    /// sweeps.
    pub(crate) fn collect_quota_payload_step_tx(
        tx: &WriteTransactionCtx,
        after: Option<ContentHash>,
        max_bytes: u64,
    ) -> DbResult<(Option<ContentHash>, PayloadMaintenance)> {
        let mut accounting = Self::accounting_tx(tx)?;
        Self::require_payload_accounting_ready_tx(tx)?;
        let next = tx
            .open_table(&payload_gc::TABLE)?
            .range((after.map_or(Unbounded, Excluded), Unbounded))?
            .next()
            .transpose()?
            .map(|(key, _)| key.value_try())
            .transpose()?;
        let mut result = PayloadMaintenance {
            ready: true,
            ..Default::default()
        };
        let Some(hash) = next else {
            return Ok((None, result));
        };
        Self::validate_hash_tx(tx, hash)?;
        let guard = tx
            .open_table(&payload_accounting_hashes::TABLE)?
            .get(&hash)?
            .context(crate::PayloadAccountingInvariantSnafu)?
            .value_try()?;
        result.visited = 1;
        if guard.references == 0 && !guard.protected_history {
            let bytes = Self::stored_len_tx(tx, hash)?;
            if bytes > max_bytes {
                return Ok((Some(hash), result));
            }
            tx.open_table(&content_store::TABLE)?.remove(&hash)?;
            accounting.usage.unique_stored_bytes = accounting
                .usage
                .unique_stored_bytes
                .checked_sub(bytes)
                .context(OverflowSnafu)?;
            result.removed_bytes = bytes;
        }
        tx.open_table(&payload_gc::TABLE)?.remove(&hash)?;
        tx.open_table(&payload_accounting::TABLE)?
            .insert(&(), &accounting)?;
        Ok((Some(hash), result))
    }
}
