//! Bounded local views for session-authorized direct-message interfaces.

use std::ops::Bound;

use rostra_core::ShortEventId;
use rostra_core::id::RostraId;
use rostra_dm::DeviceState;
use snafu::ResultExt as _;

use crate::dm::HistoryEntry;
use crate::{Database, DbResult, DirectMessageSnafu};

/// Public local installation metadata, without epoch secrets.
pub struct LocalInstallation {
    /// Stable random identifier, replaced only by explicit re-enrollment.
    pub device_id: [u8; 16],
    /// Whether this installation has permanently stopped sending.
    pub retired: bool,
}

impl Database {
    /// Return at most 64 conversations in participant-pair order.
    ///
    /// Each conversation uses two index seeks, regardless of its history size.
    /// The exclusive cursor is the sorted participant pair of the last entry.
    /// Callers must require a full unlocked session before exposing plaintext.
    pub async fn dm_conversations(
        &self,
        after: Option<(RostraId, RostraId)>,
        limit: usize,
    ) -> DbResult<Vec<HistoryEntry>> {
        self.read_with(|tx| {
            let index = tx.open_table(&crate::events_dm_history_by_conversation::TABLE)?;
            let history = tx.open_table(&crate::events_dm_history::TABLE)?;
            let mut lower = after.map_or(Bound::Unbounded, |(first, second)| {
                Bound::Excluded((first, second, u64::MAX, ShortEventId::MAX))
            });
            let mut entries = Vec::new();
            for _ in 0..limit.min(64) {
                let Some(row) = index.range((lower, Bound::Unbounded))?.next() else {
                    break;
                };
                let (key, _) = row?;
                let (first, second, _, _) = key.value_try()?;
                let upper = (first, second, u64::MAX, ShortEventId::MAX);
                let (_, key) = index
                    .range((first, second, 0, ShortEventId::ZERO)..=upper)?
                    .next_back()
                    .ok_or(rostra_dm::Error::Invalid)
                    .context(DirectMessageSnafu)??;
                let entry = history
                    .get(&key.value_try()?)?
                    .ok_or(rostra_dm::Error::Invalid)
                    .context(DirectMessageSnafu)?
                    .value_try()?;
                entries.push(entry);
                lower = Bound::Excluded(upper);
            }
            Ok(entries)
        })
        .await
    }

    /// Read the current installation's public identity and retirement state.
    pub async fn dm_local_installation(&self) -> DbResult<Option<LocalInstallation>> {
        self.read_with(|tx| {
            Ok(tx
                .open_table(&crate::ids_dm_installation::TABLE)?
                .get(&())?
                .map(|row| {
                    row.value_try().map(|installation| LocalInstallation {
                        device_id: installation.device_id,
                        retired: installation.retired,
                    })
                })
                .transpose()?)
        })
        .await
    }

    /// Read at most 64 own-account device announcements in device-ID order.
    ///
    /// This includes permanent retirements and expired latest announcements;
    /// it never exposes local private epoch material.
    pub async fn dm_own_devices(
        &self,
        after: Option<[u8; 16]>,
        limit: usize,
    ) -> DbResult<Vec<([u8; 16], DeviceState)>> {
        self.read_with(|tx| {
            let lower = after.map_or(Bound::Included((self.self_id, [0; 16])), |device| {
                Bound::Excluded((self.self_id, device))
            });
            tx.open_table(&crate::ids_dm_devices::TABLE)?
                .range((lower, Bound::Included((self.self_id, [255; 16]))))?
                .take(limit.min(64))
                .map(|row| {
                    let (key, state) = row?;
                    Ok((key.value_try()?.1, state.value_try()?))
                })
                .collect()
        })
        .await
    }
}
