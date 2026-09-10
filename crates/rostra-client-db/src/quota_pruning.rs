//! Explicit quota transitions; no runtime worker or default pressure policy.

use rostra_core::event::{EventExt as _, EventKind, VerifiedEvent, VerifiedEventContent};
use rostra_core::retention::RetentionPolicy;
use rostra_core::{ShortEventId, Timestamp};
use snafu::OptionExt as _;

use crate::retention::{QuotaPruneDecision, QuotaPruneReason};
use crate::{Database, DbResult, EventContentState, WriteTransactionCtx};

/// The lifecycle state selected by the caller, rechecked under the writer lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaPruneTarget {
    /// Release an already materialized payload after its grace expires.
    Processed,
    /// Permanently decline admission before materialization.
    Missing,
}

/// Explicit external clock-trust assertion, not inferred from stored
/// timestamps.
#[derive(Debug, Clone, Copy)]
pub enum RetentionClock {
    /// No destructive decision is authorized.
    Untrusted,
    /// The caller established a reliable local clock, including forward jumps.
    Trusted(Timestamp),
}

/// Outcome of a checked local dematerialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaPruneOutcome {
    /// State changed; physical reclamation remains separate.
    Pruned {
        /// Processed signed bytes released, zero for Missing admission.
        logical_released_bytes: u64,
    },
    /// State no longer matches the selected target, including terminal states.
    Unchanged,
    /// Protection, origins, grace, or clock trust prevented the transition.
    Ineligible,
}

/// A selected victim and explicit retention-policy authorization inputs.
#[derive(Debug, Clone, Copy)]
pub struct QuotaPruneRequest {
    /// Retained event to reconsider transactionally.
    pub id: ShortEventId,
    /// Expected lifecycle state, guarding stale admission selections.
    pub target: QuotaPruneTarget,
    /// Original pressure source to preserve across replay.
    pub reason: QuotaPruneReason,
    /// Active policy supplying the first-materialization grace.
    pub policy: RetentionPolicy,
    /// Caller-established clock trust, never inferred by the database.
    pub clock: RetentionClock,
}

impl Database {
    /// Explicitly prune one remote SocialPost selected by an external quota
    /// policy.
    ///
    /// This does not select victims, establish pressure, or trust a wall clock.
    /// Callers must supply the active policy and an independently established
    /// clock trust decision. Accounting must be ready. A stale Missing
    /// selection cannot evict content delivered concurrently. Unknown
    /// origins fail closed; grace starts only at first materialization, not
    /// at Missing admission.
    pub async fn prune_quota_payload(
        &self,
        request: QuotaPruneRequest,
    ) -> DbResult<QuotaPruneOutcome> {
        self.write_with(|tx| self.prune_quota_payload_tx(tx, request))
            .await
    }

    pub(crate) fn prune_quota_payload_tx(
        &self,
        tx: &WriteTransactionCtx,
        request: QuotaPruneRequest,
    ) -> DbResult<QuotaPruneOutcome> {
        let QuotaPruneRequest {
            id,
            target,
            reason,
            policy,
            clock,
        } = request;
        Self::require_payload_accounting_ready_tx(tx)?;
        let Some(event) = tx
            .open_table(&crate::events::TABLE)?
            .get(&id)?
            .map(|row| row.value_try())
            .transpose()?
        else {
            return Ok(QuotaPruneOutcome::Unchanged);
        };
        let state = tx
            .open_table(&crate::events_content_state::TABLE)?
            .get(&id)?
            .map(|row| row.value_try())
            .transpose()?;
        if !matches!(
            (target, &state),
            (QuotaPruneTarget::Processed, None)
                | (
                    QuotaPruneTarget::Missing,
                    Some(EventContentState::Missing { .. })
                )
        ) {
            return Ok(QuotaPruneOutcome::Unchanged);
        }
        if event.author() == self.self_id
            || event.kind() != EventKind::SOCIAL_POST
            || event.content_len() == 0
        {
            return Ok(QuotaPruneOutcome::Ineligible);
        }
        let RetentionClock::Trusted(now) = clock else {
            return Ok(QuotaPruneOutcome::Ineligible);
        };
        let Some(origins) = tx
            .open_table(&crate::events_retention_origins::TABLE)?
            .get(&id)?
            .map(|row| row.value_try())
            .transpose()?
        else {
            return Ok(QuotaPruneOutcome::Ineligible);
        };
        if now < origins.effective_timestamp
            || origins.materialized_at.is_some_and(|first| now < first)
            || (target == QuotaPruneTarget::Processed
                && !policy.grace_elapsed(origins.materialized_at, now))
            || (target == QuotaPruneTarget::Missing && origins.materialized_at.is_some())
        {
            return Ok(QuotaPruneOutcome::Ineligible);
        }
        if tx
            .open_table(&crate::events_quota_pruned::TABLE)?
            .get(&id)?
            .is_some()
        {
            return crate::PayloadAccountingInvariantSnafu.fail();
        }
        let verified = VerifiedEvent::assume_verified_from_signed(event.signed);
        let before = self.payload_before_tx(tx, &verified)?;
        if target == QuotaPruneTarget::Processed {
            let bytes = tx
                .open_table(&crate::content_store::TABLE)?
                .get(&event.content_hash())?
                .context(crate::PayloadAccountingInvariantSnafu)?
                .value_try()?
                .0
                .into_owned();
            let content = VerifiedEventContent::verify(verified, bytes)
                .map_err(|_| crate::PayloadAccountingInvariantSnafu.build())?;
            match self.process_event_content_reverted_tx(&content, tx) {
                Ok(()) => {}
                Err(crate::ProcessEventError::Db { source }) => return Err(source),
                Err(crate::ProcessEventError::Invalid { .. }) => {
                    return crate::PayloadAccountingInvariantSnafu.fail();
                }
            }
        }
        Self::prune_event_content_tx(
            id,
            event.content_hash(),
            &mut tx.open_table(&crate::events_content_state::TABLE)?,
            &mut tx.open_table(&crate::content_rc::TABLE)?,
            &mut tx.open_table(&crate::events_content_missing::TABLE)?,
            Some((
                event.author(),
                event.content_len(),
                &mut tx.open_table(&crate::ids_data_usage::TABLE)?,
            )),
        )?;
        tx.open_table(&crate::events_quota_pruned::TABLE)?.insert(
            &id,
            &QuotaPruneDecision {
                reason,
                pruned_at: now,
            },
        )?;
        Self::nominate_quota_hash_tx(tx, event.content_hash())?;
        self.payload_after_tx(tx, before)?;
        if tx.commit_hooks_enabled() {
            let sender = self.quota_pruned_tx.clone();
            tx.on_commit(move || {
                let _ = sender.send(id);
            });
        }
        Ok(QuotaPruneOutcome::Pruned {
            logical_released_bytes: if target == QuotaPruneTarget::Processed {
                u64::from(event.content_len())
            } else {
                0
            },
        })
    }
}
