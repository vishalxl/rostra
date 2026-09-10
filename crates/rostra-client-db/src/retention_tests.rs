use std::num::NonZeroUsize;

use rostra_core::Timestamp;
use rostra_core::event::content_kind::{EventContentKind as _, SocialPost};
use rostra_core::event::{Event, EventExt as _, VerifiedEvent, VerifiedEventContent};
use rostra_core::id::{RostraIdSecretKey, ToShort as _};

use crate::retention::{QuotaPruneDecision, QuotaPruneReason, RetentionOrigins};
use crate::{Database, EventContentState, SocialPostMaterialization};

fn ts(seconds: u64) -> Timestamp {
    Timestamp::ZERO.saturating_add_secs(seconds)
}

fn post(secret: RostraIdSecretKey, authored: i64, body: &str) -> VerifiedEventContent {
    let content = SocialPost::new(body.to_owned(), None, Default::default())
        .serialize_cbor()
        .unwrap();
    let signed = Event::builder_raw_content()
        .author(secret.id())
        .kind(rostra_core::event::EventKind::SOCIAL_POST)
        .timestamp(time::OffsetDateTime::from_unix_timestamp(authored).unwrap())
        .content(&content)
        .build()
        .signed_by(secret);
    VerifiedEventContent::assume_verified(
        VerifiedEvent::verify_signed(secret.id(), signed).unwrap(),
        content,
    )
}

async fn origins(
    db: &Database,
    post: &VerifiedEventContent,
) -> crate::DbResult<Option<RetentionOrigins>> {
    db.read_with(|tx| {
        Ok(tx
            .open_table(&crate::events_retention_origins::TABLE)?
            .get(&post.event_id().to_short())?
            .map(|row| row.value_try())
            .transpose()?)
    })
    .await
}

async fn ingest(db: &Database, post: &VerifiedEventContent, now: Timestamp) -> crate::DbResult<()> {
    db.write_with(|tx| {
        db.process_event_tx(&post.event, now, tx)?;
        db.process_event_content_tx(post, now, tx)
    })
    .await
}

// Build an authoritative source fixture, not a production quota mutation API.
// Later lifecycle work must provide the fully checked transactional boundary.
async fn quota_fixture(
    db: &Database,
    post: &VerifiedEventContent,
    decision: QuotaPruneDecision,
) -> anyhow::Result<()> {
    db.write_with(|tx| {
        let mut states = tx.open_table(&crate::events_content_state::TABLE)?;
        if states.get(&post.event_id().to_short())?.is_none() {
            db.process_event_content_reverted_tx(post, tx)
                .map_err(|error| match error {
                    crate::process_event_content_ops::ProcessEventError::Db { source } => source,
                    _ => panic!("fixture must have valid social content"),
                })?;
        }
        Database::prune_event_content_tx(
            post.event_id(),
            post.content_hash(),
            &mut states,
            &mut tx.open_table(&crate::content_rc::TABLE)?,
            &mut tx.open_table(&crate::events_content_missing::TABLE)?,
            Some((
                post.author(),
                post.content_len(),
                &mut tx.open_table(&crate::ids_data_usage::TABLE)?,
            )),
        )?;
        tx.open_table(&crate::events_quota_pruned::TABLE)?
            .insert(&post.event_id().to_short(), &decision)?;
        Ok(())
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_origins_are_immutable_and_replay_is_not_receipt() -> anyhow::Result<()> {
    let secret = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(secret.id()).await?;
    let future = post(secret, 500, "future");
    db.write_with(|tx| {
        db.process_event_tx(&future.event, ts(100), tx)?;
        Ok(())
    })
    .await?;
    assert_eq!(
        origins(&db, &future).await?,
        Some(RetentionOrigins {
            effective_timestamp: ts(100),
            materialized_at: None,
        })
    );
    ingest(&db, &future, ts(200)).await?;
    let expected = origins(&db, &future).await?;
    assert_eq!(expected.unwrap().materialized_at, Some(ts(200)));
    ingest(&db, &future, ts(50)).await?;
    ingest(&db, &future, Timestamp::MAX).await?;
    assert_eq!(origins(&db, &future).await?, expected);

    let historical = post(secret, 10, "historical");
    ingest(&db, &historical, ts(300)).await?;
    assert_eq!(
        origins(&db, &historical).await?,
        Some(RetentionOrigins {
            effective_timestamp: ts(10),
            materialized_at: Some(ts(300)),
        })
    );
    db.write_with(|tx| Database::prepare_total_migration(tx, 27))
        .await?;
    db.write_with(|tx| db.reprocess_migration_stash(tx)).await?;
    assert_eq!(origins(&db, &future).await?, expected);
    assert_eq!(
        origins(&db, &historical).await?.unwrap().materialized_at,
        Some(ts(300))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_replay_preserves_quota_decisions_with_shared_surviving_bytes()
-> anyhow::Result<()> {
    let secret = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(secret.id()).await?;
    let pruned = post(secret, 10, "shared");
    let retained = post(secret, 11, "shared");
    let missing = post(secret, 12, "never materialized");
    ingest(&db, &pruned, ts(100)).await?;
    ingest(&db, &retained, ts(101)).await?;
    db.write_with(|tx| {
        db.process_event_tx(&missing.event, ts(102), tx)?;
        Ok(())
    })
    .await?;
    let decision = QuotaPruneDecision {
        reason: QuotaPruneReason::GlobalQuota,
        pruned_at: ts(200),
    };
    quota_fixture(&db, &pruned, decision).await?;
    quota_fixture(
        &db,
        &missing,
        QuotaPruneDecision {
            reason: QuotaPruneReason::AuthorQuota,
            ..decision
        },
    )
    .await?;
    let expected_origins = origins(&db, &pruned).await?;

    for _ in 0..2 {
        db.write_with(|tx| Database::prepare_total_migration(tx, 27))
            .await?;
        db.write_with(|tx| db.reprocess_migration_stash(tx)).await?;
        ingest(&db, &pruned, ts(300)).await?;
        ingest(&db, &missing, ts(300)).await?;
        assert_eq!(origins(&db, &pruned).await?, expected_origins);
        assert_eq!(origins(&db, &missing).await?.unwrap().materialized_at, None);
        db.read_with(|tx| {
            for event in [&pruned, &missing] {
                assert!(matches!(
                    tx.open_table(&crate::events_content_state::TABLE)?
                        .get(&event.event_id().to_short())?
                        .unwrap()
                        .value(),
                    EventContentState::Pruned
                ));
                assert!(
                    tx.open_table(&crate::social_posts::TABLE)?
                        .get(&event.event_id().to_short())?
                        .is_none()
                );
            }
            assert_eq!(
                tx.open_table(&crate::events_quota_pruned::TABLE)?
                    .get(&pruned.event_id().to_short())?
                    .unwrap()
                    .value(),
                decision
            );
            assert_eq!(
                tx.open_table(&crate::content_rc::TABLE)?
                    .get(&pruned.content_hash())?
                    .unwrap()
                    .value(),
                1
            );
            assert!(
                tx.open_table(&crate::events_content_missing::TABLE)?
                    .first()?
                    .is_none()
            );
            let usage = tx
                .open_table(&crate::ids_data_usage::TABLE)?
                .get(&secret.id())?
                .unwrap()
                .value();
            assert_eq!(
                usage.current_content_size,
                u64::from(retained.content_len())
            );
            assert_eq!(
                usage.pruned_payload_size,
                u64::from(pruned.content_len()) + u64::from(missing.content_len())
            );
            Ok(())
        })
        .await?;
        let feed = db
            .scan_social_post_materializations(None, NonZeroUsize::new(10).unwrap())
            .await?;
        assert_eq!(feed.items.len(), 2);
        assert!(matches!(
            feed.items[0],
            SocialPostMaterialization::Removed { .. }
        ));
        assert!(
            db.get_social_post(retained.event_id().to_short())
                .await
                .is_some()
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_upgrade_leaves_unknown_legacy_origins_protected() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("retention.redb");
    let secret = RostraIdSecretKey::generate();
    let old = post(secret, 10, "old");
    let missing = post(secret, 11, "missing");
    let db = Database::open(&path, secret.id()).await?;
    ingest(&db, &old, ts(100)).await?;
    db.write_with(|tx| {
        db.process_event_tx(&missing.event, ts(100), tx)?;
        Ok(())
    })
    .await?;
    drop(db);
    let raw = redb_bincode::Database::from(redb::Database::open(&path)?);
    let tx = raw.begin_write()?;
    tx.as_raw()
        .delete_table(crate::events_retention_origins::TABLE.as_raw())?;
    tx.as_raw()
        .delete_table(crate::events_quota_pruned::TABLE.as_raw())?;
    tx.open_table(&crate::db_version::TABLE)?.insert(&(), &26)?;
    tx.commit()?;
    drop(raw);

    let db = Database::open(&path, secret.id()).await?;
    assert_eq!(origins(&db, &old).await?, None);
    ingest(&db, &old, ts(200)).await?;
    ingest(&db, &missing, ts(200)).await?;
    assert_eq!(origins(&db, &old).await?, None);
    assert_eq!(origins(&db, &missing).await?, None);
    db.write_with(|tx| Database::prepare_total_migration(tx, 27))
        .await?;
    db.write_with(|tx| db.reprocess_migration_stash(tx)).await?;
    assert_eq!(origins(&db, &old).await?, None);
    assert_eq!(origins(&db, &missing).await?, None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_deleted_is_stronger_and_keeps_original_quota_reason() -> anyhow::Result<()> {
    let secret = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(secret.id()).await?;
    let target = post(secret, 10, "deleted after quota");
    ingest(&db, &target, ts(100)).await?;
    let decision = QuotaPruneDecision {
        reason: QuotaPruneReason::AuthorQuota,
        pruned_at: ts(150),
    };
    quota_fixture(&db, &target, decision).await?;
    let signed = Event::builder_raw_content()
        .author(secret.id())
        .kind(rostra_core::event::EventKind::SOCIAL_POST)
        .timestamp(time::OffsetDateTime::from_unix_timestamp(20)?)
        .parent_prev(target.event_id().into())
        .delete(target.event_id().into())
        .content(&rostra_core::event::EventContentRaw::new(vec![]))
        .build()
        .signed_by(secret);
    let deletion = VerifiedEvent::verify_signed(secret.id(), signed)?;
    db.write_with(|tx| {
        db.process_event_tx(&deletion, ts(200), tx)?;
        Ok(())
    })
    .await?;
    for _ in 0..2 {
        db.write_with(|tx| Database::prepare_total_migration(tx, 27))
            .await?;
        db.write_with(|tx| db.reprocess_migration_stash(tx)).await?;
        ingest(&db, &target, ts(300)).await?;
        db.read_with(|tx| {
            assert!(matches!(
                tx.open_table(&crate::events_content_state::TABLE)?
                    .get(&target.event_id().to_short())?
                    .unwrap()
                    .value(),
                EventContentState::Deleted { deleted_by } if deleted_by == deletion.event_id.to_short()
            ));
            assert_eq!(
                tx.open_table(&crate::events_quota_pruned::TABLE)?
                    .get(&target.event_id().to_short())?
                    .unwrap()
                    .value(),
                decision
            );
            let usage = tx.open_table(&crate::ids_data_usage::TABLE)?
                .get(&secret.id())?.unwrap().value();
            assert_eq!(usage.pruned_payload_size, 0);
            assert_eq!(usage.deleted_payload_size, u64::from(target.content_len()));
            assert!(tx.open_table(&crate::content_rc::TABLE)?
                .get(&target.content_hash())?.is_none());
            Ok(())
        }).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_aborted_ingestion_does_not_consume_origins() -> anyhow::Result<()> {
    let secret = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(secret.id()).await?;
    let event = post(secret, 500, "abort");
    let result = db
        .write_with(|tx| {
            db.process_event_tx(&event.event, ts(100), tx)?;
            db.process_event_content_tx(&event, ts(100), tx)?;
            crate::OverflowSnafu.fail::<()>()
        })
        .await;
    assert!(result.is_err());
    assert_eq!(origins(&db, &event).await?, None);
    ingest(&db, &event, ts(200)).await?;
    assert_eq!(
        origins(&db, &event).await?,
        Some(RetentionOrigins {
            effective_timestamp: ts(200),
            materialized_at: Some(ts(200)),
        })
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_stash_decode_failure_is_retryable_across_reopen() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("retry.redb");
    let secret = RostraIdSecretKey::generate();
    let event = post(secret, 10, "retry");
    let db = Database::open(&path, secret.id()).await?;
    ingest(&db, &event, ts(100)).await?;
    let expected = origins(&db, &event).await?;
    db.write_with(|tx| Database::prepare_total_migration(tx, 27))
        .await?;
    drop(db);

    // Deliberately corrupt only a disposable test database's stash value.
    let raw = redb::Database::open(&path)?;
    let tx = raw.begin_write()?;
    let definition =
        redb::TableDefinition::<&[u8], &[u8]>::new("_total_migration_retention_origins");
    let (key, value) = {
        use redb::ReadableTable as _;
        let mut table = tx.open_table(definition)?;
        let (key, value) = {
            let (key, value) = table.first()?.unwrap();
            (key.value().to_vec(), value.value().to_vec())
        };
        let mut invalid = value.clone();
        invalid.push(0xff);
        table.insert(key.as_slice(), invalid.as_slice())?;
        (key, value)
    };
    tx.commit()?;
    drop(raw);
    assert!(Database::open(&path, secret.id()).await.is_err());
    let raw = redb::Database::open(&path)?;
    let tx = raw.begin_write()?;
    tx.open_table(definition)?
        .insert(key.as_slice(), value.as_slice())?;
    tx.commit()?;
    drop(raw);
    let db = Database::open(&path, secret.id()).await?;
    assert_eq!(origins(&db, &event).await?, expected);
    assert!(
        db.get_social_post(event.event_id().to_short())
            .await
            .is_some()
    );
    db.write_with(|tx| {
        assert!(!Database::has_pending_migration_stash(tx)?);
        Ok(())
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_missing_required_stash_fails_closed() -> anyhow::Result<()> {
    let secret = RostraIdSecretKey::generate();
    for name in [
        "_total_migration_retention_origins",
        "_total_migration_quota_pruned",
    ] {
        let db = Database::new_in_memory(secret.id()).await?;
        ingest(&db, &post(secret, 10, "stash"), ts(100)).await?;
        db.write_with(|tx| Database::prepare_total_migration(tx, 27))
            .await?;
        db.write_with(|tx| {
            tx.as_raw()
                .delete_table(redb::TableDefinition::<&[u8], &[u8]>::new(name))?;
            Ok(())
        })
        .await?;
        assert!(matches!(
            db.write_with(|tx| db.reprocess_migration_stash(tx)).await,
            Err(crate::DbError::MissingMigrationStashTable { table, .. }) if table == name
        ));
        db.write_with(|tx| {
            assert!(Database::has_pending_migration_stash(tx)?);
            Ok(())
        })
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_old_total_replay_does_not_invent_origins() -> anyhow::Result<()> {
    let secret = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(secret.id()).await?;
    let event = post(secret, 10, "old total replay");
    ingest(&db, &event, ts(100)).await?;
    db.write_with(|tx| Database::prepare_total_migration(tx, 26))
        .await?;
    db.write_with(|tx| db.reprocess_migration_stash(tx)).await?;
    assert_eq!(origins(&db, &event).await?, None);
    ingest(&db, &event, ts(200)).await?;
    assert_eq!(origins(&db, &event).await?, None);

    let missing = post(secret, 20, "new header, late materialization");
    db.write_with(|tx| {
        db.process_event_tx(&missing.event, ts(300), tx)?;
        Ok(())
    })
    .await?;
    db.write_with(|tx| Database::prepare_total_migration(tx, 27))
        .await?;
    db.write_with(|tx| db.reprocess_migration_stash(tx)).await?;
    assert_eq!(origins(&db, &missing).await?.unwrap().materialized_at, None);
    ingest(&db, &missing, ts(400)).await?;
    assert_eq!(
        origins(&db, &missing).await?,
        Some(RetentionOrigins {
            effective_timestamp: ts(20),
            materialized_at: Some(ts(400)),
        })
    );
    Ok(())
}
