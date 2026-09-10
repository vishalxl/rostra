//! Preserve non-replayable messaging state across total migration.

use redb::TableHandle as _;
use snafu::ResultExt as _;

use crate::{Database, DbResult, DirectMessageSnafu, WriteTransactionCtx};

pub(crate) const DM_SCHEMA_VERSION: u64 = 32;

macro_rules! dm_tables {
    ($action:ident, $tx:expr) => {
        $action!($tx, ids_dm_installation);
        $action!($tx, ids_dm_epochs);
        $action!($tx, ids_dm_devices);
        $action!($tx, events_dm_history);
    };
}

impl Database {
    pub(crate) fn init_dm_tables_tx(tx: &WriteTransactionCtx) -> DbResult<()> {
        tx.open_table(&crate::events_dm_pending::TABLE)?;
        tx.open_table(&crate::events_dm_history_by_conversation::TABLE)?;
        tx.open_table(&crate::events_dm_incoming::TABLE)?;
        tx.open_table(&crate::events_dm_incoming_by_peer::TABLE)?;
        tx.open_table(&crate::events_dm_incoming_sequence::TABLE)?;
        tx.open_table(&crate::events_dm_incoming_count_by_peer::TABLE)?;
        tx.open_table(&crate::ids_dm_read::TABLE)?;
        tx.open_table(&crate::ids_dm_read_count::TABLE)?;
        tx.open_table(&crate::ids_dm_read_count_by_peer::TABLE)?;
        tx.open_table(&crate::ids_dm_devices_by_interval::TABLE)?;
        macro_rules! init {
            ($tx:expr, $name:ident) => {
                $tx.open_table(&crate::$name::TABLE)?;
            };
        }
        dm_tables!(init, tx);
        Ok(())
    }

    pub(crate) fn stash_dm_tables_tx(tx: &WriteTransactionCtx, source_ver: u64) -> DbResult<()> {
        if source_ver < DM_SCHEMA_VERSION {
            return Ok(());
        }
        macro_rules! stash {
            ($tx:expr, $name:ident) => {
                let temp =
                    redb_bincode::TableDefinition::<crate::$name::Key, crate::$name::Value>::new(
                        concat!("_total_migration_", stringify!($name)),
                    );
                let source = $tx.open_table(&crate::$name::TABLE)?;
                let mut destination = $tx.open_table(&temp)?;
                for row in source.range(..)? {
                    let (key, value) = row?;
                    destination.insert(&key.value_try()?, &value.value_try()?)?;
                }
            };
        }
        dm_tables!(stash, tx);
        if 33 <= source_ver {
            stash!(tx, events_dm_incoming_sequence);
            stash!(tx, ids_dm_read);
            stash!(tx, ids_dm_read_count);
            stash!(tx, ids_dm_read_count_by_peer);
        }
        Ok(())
    }

    pub(crate) fn restore_dm_tables_tx(tx: &WriteTransactionCtx, source_ver: u64) -> DbResult<()> {
        if source_ver < DM_SCHEMA_VERSION {
            return Ok(());
        }
        let names = tx
            .as_raw()
            .list_tables()?
            .map(|table| table.name().to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        macro_rules! restore {
            ($tx:expr, $name:ident) => {{
                let name = concat!("_total_migration_", stringify!($name));
                if !names.contains(name) {
                    return crate::MissingMigrationStashTableSnafu {
                        table: name.to_owned(),
                    }
                    .fail();
                }
                let temp =
                    redb_bincode::TableDefinition::<crate::$name::Key, crate::$name::Value>::new(
                        name,
                    );
                let source = $tx.open_table(&temp)?;
                let mut destination = $tx.open_table(&crate::$name::TABLE)?;
                for row in source.range(..)? {
                    let (key, value) = row?;
                    destination.insert(&key.value_try()?, &value.value_try()?)?;
                }
            }};
        }
        dm_tables!(restore, tx);
        if 33 <= source_ver {
            restore!(tx, events_dm_incoming_sequence);
            restore!(tx, ids_dm_read);
            restore!(tx, ids_dm_read_count);
            restore!(tx, ids_dm_read_count_by_peer);
        }
        Self::rebuild_dm_history_index_tx(tx)?;
        Self::rebuild_dm_device_index_tx(tx)?;
        Ok(())
    }

    pub(crate) fn rebuild_dm_history_index_tx(tx: &WriteTransactionCtx) -> DbResult<()> {
        let mut index = tx.open_table(&crate::events_dm_history_by_conversation::TABLE)?;
        let mut incoming = tx.open_table(&crate::events_dm_incoming::TABLE)?;
        let mut incoming_by_peer = tx.open_table(&crate::events_dm_incoming_by_peer::TABLE)?;
        let mut incoming_sequence = tx.open_table(&crate::events_dm_incoming_sequence::TABLE)?;
        let mut incoming_count_by_peer =
            tx.open_table(&crate::events_dm_incoming_count_by_peer::TABLE)?;
        index.retain(|_, _| false)?;
        incoming.retain(|_, _| false)?;
        incoming_by_peer.retain(|_, _| false)?;
        incoming_count_by_peer.retain(|_, _| false)?;
        let self_id = tx
            .open_table(&crate::ids_self::TABLE)?
            .get(&())?
            .ok_or(rostra_dm::Error::Invalid)
            .context(DirectMessageSnafu)?
            .value_try()?
            .rostra_id;
        let mut next_sequence = incoming_sequence
            .range(..)?
            .filter_map(|row| row.ok())
            .filter_map(|(_, sequence)| sequence.value_try().ok())
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(crate::DbError::Overflow)?;
        for row in tx.open_table(&crate::events_dm_history::TABLE)?.range(..)? {
            let (key, entry) = row?;
            let entry = entry.value_try()?;
            let key = key.value_try()?;
            index.insert(
                &(
                    entry.sender.min(entry.recipient),
                    entry.sender.max(entry.recipient),
                    entry.timestamp,
                    entry.event_id,
                ),
                &key,
            )?;
            if entry.recipient == self_id {
                let sequence = incoming_sequence
                    .get(&key)?
                    .map(|value| value.value_try())
                    .transpose()?
                    .unwrap_or_else(|| {
                        let sequence = next_sequence;
                        next_sequence = next_sequence.saturating_add(1);
                        sequence
                    });
                incoming.insert(&sequence, &key)?;
                incoming_by_peer.insert(&(entry.sender, sequence), &key)?;
                incoming_sequence.insert(&key, &sequence)?;
                let count = incoming_count_by_peer
                    .get(&entry.sender)?
                    .map(|value| value.value_try())
                    .transpose()?
                    .unwrap_or(0)
                    .checked_add(1)
                    .ok_or(crate::DbError::Overflow)?;
                incoming_count_by_peer.insert(&entry.sender, &count)?;
            }
        }
        Ok(())
    }

    pub(crate) fn cleanup_dm_stash_tx(tx: &WriteTransactionCtx) -> DbResult<()> {
        macro_rules! cleanup {
            ($tx:expr, $name:ident) => {
                let temp = redb::TableDefinition::<&[u8], &[u8]>::new(concat!(
                    "_total_migration_",
                    stringify!($name)
                ));
                $tx.as_raw().delete_table(temp)?;
            };
        }
        dm_tables!(cleanup, tx);
        cleanup!(tx, events_dm_incoming_sequence);
        cleanup!(tx, ids_dm_read);
        cleanup!(tx, ids_dm_read_count);
        cleanup!(tx, ids_dm_read_count_by_peer);
        Ok(())
    }
}
