//! Preserve non-replayable messaging state across total migration.

use redb::TableHandle as _;

use crate::{Database, DbResult, WriteTransactionCtx};

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
        Self::rebuild_dm_history_index_tx(tx)?;
        Self::rebuild_dm_device_index_tx(tx)?;
        Ok(())
    }

    pub(crate) fn rebuild_dm_history_index_tx(tx: &WriteTransactionCtx) -> DbResult<()> {
        let mut index = tx.open_table(&crate::events_dm_history_by_conversation::TABLE)?;
        index.retain(|_, _| false)?;
        for row in tx.open_table(&crate::events_dm_history::TABLE)?.range(..)? {
            let (key, entry) = row?;
            let entry = entry.value_try()?;
            index.insert(
                &(
                    entry.sender.min(entry.recipient),
                    entry.sender.max(entry.recipient),
                    entry.timestamp,
                    entry.event_id,
                ),
                &key.value_try()?,
            )?;
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
        Ok(())
    }
}
