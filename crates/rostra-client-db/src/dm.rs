//! Authoritative installation state and retained plaintext direct-message
//! history.
//!
//! These records are not disposable event projections. Replaying ciphertext
//! cannot recover erased epoch secrets or local sent history.

use bincode::{Decode, Encode};
use rostra_core::ShortEventId;
use rostra_core::id::RostraId;
use rostra_dm::{Announcement, LocalEpoch};
use snafu::ResultExt as _;

use crate::{Database, DbResult, DirectMessageSnafu};

type RankedDestination = (
    (u64, ShortEventId, [u8; 16]),
    rostra_dm::PublicKey,
    u64,
    u64,
    RostraId,
    rostra_dm::DeviceState,
);

/// Selected destinations with a deadline rechecked inside the send transaction.
pub struct SendPermit {
    pub(crate) sender: RostraId,
    pub(crate) recipient: RostraId,
    pub(crate) keys: Vec<rostra_dm::PublicKey>,
    pub(crate) send_until: u64,
    pub(crate) send_from: u64,
    pub(crate) selected: Vec<((RostraId, [u8; 16]), rostra_dm::DeviceState)>,
    pub(crate) sending_device: Option<[u8; 16]>,
}

impl SendPermit {
    /// Real keys selected by the fixed device-allocation rule.
    pub fn keys(&self) -> &[rostra_dm::PublicKey] {
        &self.keys
    }
}

/// Stable installation identity. Retirement requires explicit re-enrollment.
#[derive(Encode, Decode)]
pub(crate) struct Installation {
    pub(crate) device_id: [u8; 16],
    pub(crate) retired: bool,
}

/// Locally retained authenticated message, independent of ciphertext retention.
///
/// No automatic plaintext expiry is enabled. TODO: separately design local
/// age-based history retention and authenticated device-to-device history sync.
#[derive(Clone, Encode, Decode)]
pub struct HistoryEntry {
    /// Signed sender, also bound inside the authenticated plaintext.
    pub sender: RostraId,
    /// Recipient bound inside the authenticated plaintext.
    pub recipient: RostraId,
    /// Random message identifier; deduplication key is `(sender, message_id)`.
    pub message_id: [u8; 16],
    /// First authenticated content committed under this identifier.
    pub text: String,
    /// Original source event; later replay must not change the winning message.
    pub event_id: ShortEventId,
    /// Signed timestamp of that original source event.
    pub timestamp: u64,
    /// A different authenticated body reused this message identifier.
    pub conflicted: bool,
}

impl Database {
    /// Read a bounded newest-first page of retained plaintext with one peer.
    ///
    /// Trusted callers must enforce full unlocked-session access. The cursor is
    /// exclusive and remains valid even if ciphertext or epoch secrets
    /// disappear.
    pub async fn dm_history_with(
        &self,
        peer: RostraId,
        before: Option<(u64, ShortEventId)>,
        limit: usize,
    ) -> DbResult<Vec<HistoryEntry>> {
        self.read_with(|tx| {
            use std::ops::Bound;

            let limit = limit.min(64);
            let mut entries = Vec::new();
            if limit == 0 {
                return Ok(entries);
            }
            let first = self.self_id.min(peer);
            let second = self.self_id.max(peer);
            let upper = before.map_or(
                Bound::Included((first, second, u64::MAX, ShortEventId::MAX)),
                |(timestamp, event)| Bound::Excluded((first, second, timestamp, event)),
            );
            let history = tx.open_table(&crate::events_dm_history::TABLE)?;
            let index = tx.open_table(&crate::events_dm_history_by_conversation::TABLE)?;
            for row in index
                .range((
                    Bound::Included((first, second, 0, ShortEventId::ZERO)),
                    upper,
                ))?
                .rev()
                .take(limit)
            {
                let (_, key) = row?;
                let entry = history
                    .get(&key.value_try()?)?
                    .ok_or(rostra_dm::Error::Invalid)
                    .context(DirectMessageSnafu)?;
                entries.push(entry.value_try()?);
            }
            Ok(entries)
        })
        .await
    }

    /// Whether local reduction already contains this installation announcement.
    pub async fn dm_announcement_known(&self, announcement: &Announcement) -> DbResult<bool> {
        self.read_with(|tx| {
            let state = tx
                .open_table(&crate::ids_dm_devices::TABLE)?
                .get(&(self.self_id, announcement.device_id))?
                .map(|row| row.value_try())
                .transpose()?;
            Ok(state.is_some_and(|state| match &announcement.epoch {
                None => state.retired(),
                Some(epoch) => state.latest().is_some_and(|(_, _, known)| known == epoch),
            }))
        })
        .await
    }

    /// Explicitly enroll a fresh installation ID after permanent retirement.
    /// Old receive-only secrets keep their original deletion deadlines.
    pub async fn dm_reenroll(&self) -> DbResult<[u8; 16]> {
        self.write_with(|tx| {
            if Self::has_pending_migration_stash(tx)? {
                return crate::DmMigrationPendingSnafu.fail();
            }
            let mut table = tx.open_table(&crate::ids_dm_installation::TABLE)?;
            let current = table.get(&())?.map(|row| row.value_try()).transpose()?;
            if let Some(current) = current
                && !current.retired
            {
                return Ok(current.device_id);
            }
            let installation = Installation {
                device_id: rand::random(),
                retired: false,
            };
            table.insert(&(), &installation)?;
            Ok(installation.device_id)
        })
        .await
    }

    pub(crate) fn dm_expire_keys_tx(tx: &crate::WriteTransactionCtx, now: u64) -> DbResult<()> {
        if Self::has_pending_migration_stash(tx)? {
            return crate::DmMigrationPendingSnafu.fail();
        }
        let mut epochs = tx.open_table(&crate::ids_dm_epochs::TABLE)?;
        let mut expired = Vec::new();
        for row in epochs.range(..)? {
            let (key, value) = row?;
            if value.value_try()?.public().decrypt_until <= now {
                expired.push(key.value_try()?);
            }
        }
        for key in expired {
            epochs.remove(&key)?;
        }
        Ok(())
    }

    /// Select at most eight eligible destinations using the fixed 4+4 spillover
    /// rule. Missing recipient capacity fails before callers create an event.
    pub async fn dm_destinations_now(&self, recipient: RostraId) -> DbResult<SendPermit> {
        self.dm_destinations_with_clock(recipient, || rostra_core::Timestamp::now().as_u64())
            .await
    }

    #[cfg(test)]
    pub(crate) async fn dm_destinations(
        &self,
        recipient: RostraId,
        now: u64,
    ) -> DbResult<SendPermit> {
        self.dm_destinations_with_clock(recipient, || now).await
    }

    async fn dm_destinations_with_clock(
        &self,
        recipient: RostraId,
        clock: impl Fn() -> u64,
    ) -> DbResult<SendPermit> {
        self.read_with(|tx| {
            let now = clock();
            let installation = tx
                .open_table(&crate::ids_dm_installation::TABLE)?
                .get(&())?
                .map(|row| row.value_try())
                .transpose()?;
            if installation
                .as_ref()
                .is_some_and(|installation| installation.retired)
            {
                return crate::DmRecipientUnavailableSnafu.fail();
            }
            let sending_device = installation.map(|installation| installation.device_id);
            let devices = tx.open_table(&crate::ids_dm_devices::TABLE)?;
            let index = tx.open_table(&crate::ids_dm_devices_by_interval::TABLE)?;
            let candidates = |account: RostraId| -> DbResult<Vec<RankedDestination>> {
                let mut selected = Vec::new();
                for prefix in crate::dm_index::point_prefixes(now) {
                    for row in index
                        .range(
                            (account, prefix, crate::dm_index::MIN_RANK)
                                ..=(account, prefix, crate::dm_index::MAX_RANK),
                        )?
                        .rev()
                        .take(rostra_dm::SLOT_COUNT + 1)
                    {
                        let (key, _) = row?;
                        let (_, _, (indexed_time, indexed_event, device)) = key.value_try()?;
                        if account == self.self_id && Some(device) == sending_device {
                            continue;
                        }
                        let state = devices
                            .get(&(account, device))?
                            .ok_or(rostra_dm::Error::Invalid)
                            .context(DirectMessageSnafu)?
                            .value_try()?;
                        let Some((timestamp, event, epoch)) = state.latest() else {
                            return Err(rostra_dm::Error::Invalid).context(DirectMessageSnafu);
                        };
                        if (timestamp, event) != (indexed_time, indexed_event)
                            || !epoch.eligible(now, timestamp)
                        {
                            return Err(rostra_dm::Error::Invalid).context(DirectMessageSnafu);
                        }
                        let public = rostra_dm::PublicKey::from_bytes(epoch.public_key)
                            .context(DirectMessageSnafu)?;
                        selected.push((
                            (timestamp, event, device),
                            public,
                            epoch.send_until,
                            epoch.send_from.max(
                                timestamp.saturating_sub(rostra_dm::DM_FUTURE_ANNOUNCEMENT_SKEW),
                            ),
                            account,
                            state,
                        ));
                        selected.sort_by_key(|entry| std::cmp::Reverse(entry.0));
                        selected.truncate(rostra_dm::SLOT_COUNT);
                    }
                }
                Ok(selected)
            };
            let recipients = candidates(recipient)?;
            if recipients.is_empty() {
                return crate::DmRecipientUnavailableSnafu.fail();
            }
            let senders = if recipient == self.self_id {
                Vec::new()
            } else {
                candidates(self.self_id)?
            };
            let recipient_count = recipients.len().min(4)
                + recipients
                    .len()
                    .saturating_sub(4)
                    .min(4usize.saturating_sub(senders.len()));
            let sender_count = senders.len().min(rostra_dm::SLOT_COUNT - recipient_count);
            let selected = recipients
                .into_iter()
                .take(recipient_count)
                .chain(senders.into_iter().take(sender_count))
                .collect::<Vec<_>>();
            let send_until = selected
                .iter()
                .map(|entry| entry.2)
                .min()
                .expect("recipient required");
            let send_from = selected
                .iter()
                .map(|entry| entry.3)
                .max()
                .expect("recipient required");
            Ok(SendPermit {
                sender: self.self_id,
                recipient,
                send_until,
                send_from,
                sending_device,
                selected: selected
                    .iter()
                    .map(|entry| ((entry.4, entry.0.2), entry.5.clone()))
                    .collect(),
                keys: selected
                    .into_iter()
                    .map(|(_, public, _, _, _, _)| public)
                    .collect(),
            })
        })
        .await
    }

    pub(crate) fn dm_apply_announcement_tx(
        &self,
        author: RostraId,
        announcement: &Announcement,
        timestamp: u64,
        event_id: ShortEventId,
        tx: &crate::WriteTransactionCtx,
    ) -> DbResult<()> {
        let mut devices = tx.open_table(&crate::ids_dm_devices::TABLE)?;
        let key = (author, announcement.device_id);
        let mut state = devices
            .get(&key)?
            .map(|row| row.value_try())
            .transpose()?
            .unwrap_or_default();
        Self::dm_index_device_tx(tx, author, announcement.device_id, &state, true)?;
        state.apply(announcement, timestamp, event_id);
        Self::dm_index_device_tx(tx, author, announcement.device_id, &state, false)?;
        devices.insert(&key, &state)?;
        if author == self.self_id && state.retired() {
            let mut installations = tx.open_table(&crate::ids_dm_installation::TABLE)?;
            let installation = installations
                .get(&())?
                .map(|row| row.value_try())
                .transpose()?;
            if let Some(mut installation) = installation
                && installation.device_id == announcement.device_id
            {
                installation.retired = true;
                installations.insert(&(), &installation)?;
            }
        }
        Ok(())
    }

    /// Purge expired live keys and maintain one installation's current epoch.
    ///
    /// Call only for an unlocked active client. Capture ordinary wall time once
    /// for this transaction; monotonic timers only schedule the next call. The
    /// returned public announcement may be retried, but its assigned deadlines
    /// must never be rewritten. Completion commits deletion before any queued
    /// decryption is allowed to start.
    pub async fn dm_maintain_local_now(&self) -> DbResult<Announcement> {
        self.dm_maintain_local_with_clock(|| rostra_core::Timestamp::now().as_u64())
            .await
    }

    #[cfg(test)]
    pub(crate) async fn dm_maintain_local(&self, now: u64) -> DbResult<Announcement> {
        self.dm_maintain_local_with_clock(|| now).await
    }

    async fn dm_maintain_local_with_clock(
        &self,
        clock: impl Fn() -> u64,
    ) -> DbResult<Announcement> {
        self.write_with(|tx| {
            let now = clock();
            Self::dm_expire_keys_tx(tx, now)?;
            let mut epochs = tx.open_table(&crate::ids_dm_epochs::TABLE)?;
            let mut installations = tx.open_table(&crate::ids_dm_installation::TABLE)?;
            let existing = installations.get(&())?.map(|v| v.value_try()).transpose()?;
            let installation = match existing {
                Some(installation) => installation,
                None => {
                    let installation = Installation {
                        device_id: rand::random(),
                        retired: false,
                    };
                    installations.insert(&(), &installation)?;
                    installation
                }
            };
            if installation.retired {
                return Ok(Announcement {
                    device_id: installation.device_id,
                    epoch: None,
                });
            }
            let mut newest = None;
            for row in epochs
                .range((installation.device_id, [0; 32])..=(installation.device_id, [255; 32]))?
            {
                let (_, value) = row?;
                let epoch = value.value_try()?;
                let public = epoch.public();
                if newest.as_ref().is_none_or(|old: &rostra_dm::EpochPublic| {
                    (old.send_from, old.public_key) < (public.send_from, public.public_key)
                }) {
                    newest = Some(public.clone());
                }
            }
            // A future newest key pauses generation after backward correction.
            // Expiry is never extended and deleted keys are never reconstructed.
            let public = match newest {
                Some(public) if now < public.send_until => public,
                _ => {
                    let epoch = LocalEpoch::generate(now).context(DirectMessageSnafu)?;
                    let public = epoch.public().clone();
                    epochs.insert(&(installation.device_id, public.public_key), &epoch)?;
                    public
                }
            };
            Ok(Announcement {
                device_id: installation.device_id,
                epoch: Some(public),
            })
        })
        .await
    }
}
