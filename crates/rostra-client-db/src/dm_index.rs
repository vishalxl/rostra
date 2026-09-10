//! Bounded point queries over arbitrary device eligibility intervals.
//!
//! A disjoint dyadic cover stores at most 128 rows per latest device. A point
//! belongs to 65 prefixes; taking the newest nine rows per prefix suffices for
//! the newest eight devices after excluding at most one local installation.
//! Older skipped rows already have eight better non-excluded devices in their
//! own bucket. Expired/future intervals need neither scanning nor clock sweeps.

use rostra_core::ShortEventId;
use rostra_core::id::RostraId;
use rostra_dm::DeviceState;

use crate::{Database, DbResult, WriteTransactionCtx};

pub(crate) type Prefix = (u8, u64);

/// Canonical disjoint cover of `[start, end)`, including empty intervals.
pub(crate) fn interval_prefixes(start: u64, end: u64) -> Vec<Prefix> {
    let mut cursor = u128::from(start);
    let end = u128::from(end);
    let mut prefixes = Vec::new();
    while cursor < end {
        let alignment = cursor.trailing_zeros().min(64);
        let remaining = 127 - (end - cursor).leading_zeros();
        let power = alignment.min(remaining);
        prefixes.push(((64 - power) as u8, cursor as u64));
        cursor += 1u128 << power;
    }
    prefixes
}

pub(crate) fn point_prefixes(now: u64) -> impl Iterator<Item = Prefix> {
    (0u8..=64).map(move |depth| {
        let start = if depth == 0 {
            0
        } else {
            now & (u64::MAX << (64 - depth))
        };
        (depth, start)
    })
}

impl Database {
    pub(crate) fn dm_index_device_tx(
        tx: &WriteTransactionCtx,
        account: RostraId,
        device: [u8; 16],
        state: &DeviceState,
        remove: bool,
    ) -> DbResult<()> {
        let Some((timestamp, event, epoch)) = state.latest() else {
            return Ok(());
        };
        let start = epoch
            .send_from
            .max(timestamp.saturating_sub(rostra_dm::DM_FUTURE_ANNOUNCEMENT_SKEW));
        let mut index = tx.open_table(&crate::ids_dm_devices_by_interval::TABLE)?;
        for prefix in interval_prefixes(start, epoch.send_until) {
            let key = (account, prefix, (timestamp, event, device));
            if remove {
                index.remove(&key)?;
            } else {
                index.insert(&key, &())?;
            }
        }
        Ok(())
    }

    pub(crate) fn rebuild_dm_device_index_tx(tx: &WriteTransactionCtx) -> DbResult<()> {
        tx.open_table(&crate::ids_dm_devices_by_interval::TABLE)?
            .retain(|_, _| false)?;
        for row in tx.open_table(&crate::ids_dm_devices::TABLE)?.range(..)? {
            let (key, state) = row?;
            let (account, device) = key.value_try()?;
            Self::dm_index_device_tx(tx, account, device, &state.value_try()?, false)?;
        }
        Ok(())
    }
}

pub(crate) const MIN_RANK: (u64, ShortEventId, [u8; 16]) = (0, ShortEventId::ZERO, [0; 16]);
pub(crate) const MAX_RANK: (u64, ShortEventId, [u8; 16]) = (u64::MAX, ShortEventId::MAX, [255; 16]);
