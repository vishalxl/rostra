use std::num::NonZeroUsize;

use rostra_core::Timestamp;
use rostra_core::event::content_kind::{EventContentKind as _, SocialPost};
use rostra_core::event::{Event, EventExt as _, EventKind, VerifiedEvent, VerifiedEventContent};
use rostra_core::id::{RostraIdSecretKey, ToShort as _};

use crate::{Database, DbError, PayloadUsage};

fn limit(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

fn post(secret: RostraIdSecretKey, timestamp: u64, body: &str) -> VerifiedEventContent {
    let content = SocialPost::new_text(body.to_owned(), None, Default::default())
        .serialize_cbor()
        .unwrap();
    let signed = Event::builder_raw_content()
        .author(secret.id())
        .kind(EventKind::SOCIAL_POST)
        .timestamp(Timestamp::from(timestamp).to_offset_date_time().unwrap())
        .content(&content)
        .build()
        .signed_by(secret);
    VerifiedEventContent::assume_verified(
        VerifiedEvent::verify_signed(secret.id(), signed).unwrap(),
        content,
    )
}

async fn ready(db: &Database) -> anyhow::Result<PayloadUsage> {
    for _ in 0..1000 {
        let page = db.rebuild_payload_accounting(limit(1)).await?;
        assert!(page.visited <= 1);
        if page.ready {
            return Ok(db.get_payload_usage().await?.unwrap());
        }
        assert_eq!(db.get_payload_usage().await?, None);
    }
    panic!("bounded fixture rebuild did not finish");
}

async fn nominate(db: &Database, event: &VerifiedEventContent) -> crate::DbResult<()> {
    db.write_with(|tx| {
        tx.open_table(&crate::content_quota_gc::TABLE)?
            .insert(&event.content_hash(), &())?;
        Ok(())
    })
    .await
}

async fn delete(
    db: &Database,
    secret: RostraIdSecretKey,
    event: &VerifiedEventContent,
) -> crate::DbResult<()> {
    let signed = Event::builder_raw_content()
        .author(secret.id())
        .kind(EventKind::SOCIAL_POST)
        .timestamp(Timestamp::from(1000).to_offset_date_time().unwrap())
        .delete(event.event_id().to_short())
        .build()
        .signed_by(secret);
    db.try_process_event(&VerifiedEvent::verify_signed(secret.id(), signed).unwrap())
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn accounting_distinguishes_logical_release_shared_missing_and_unique_gc()
-> anyhow::Result<()> {
    let self_id = RostraIdSecretKey::generate().id();
    let author = RostraIdSecretKey::generate();
    let other = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(self_id).await?;
    let first = post(author, 1, "shared bytes");
    let second = post(other, 2, "shared bytes");
    assert_eq!(first.content_hash(), second.content_hash());
    db.try_process_event_with_content(&first).await?;
    // Header-only insertion keeps a Missing reference even with shared bytes.
    db.write_with(|tx| {
        db.process_event_tx(&second.event, Timestamp::from(2), tx)?;
        Ok(())
    })
    .await?;
    let bytes = u64::from(first.content_len());
    assert_eq!(
        ready(&db).await?,
        PayloadUsage {
            logical_current_bytes: bytes,
            unique_stored_bytes: bytes,
        }
    );

    delete(&db, author, &first).await?;
    assert_eq!(
        db.get_payload_usage().await?.unwrap().logical_current_bytes,
        0
    );
    nominate(&db, &first).await?;
    assert_eq!(
        db.collect_quota_payload_garbage(limit(1))
            .await?
            .removed_bytes,
        0
    );
    db.try_process_event_content(&second).await?;
    assert_eq!(
        db.get_payload_usage().await?.unwrap().logical_current_bytes,
        bytes
    );
    let third = post(author, 3, "shared bytes");
    db.try_process_event_with_content(&third).await?;
    assert_eq!(
        db.get_payload_usage().await?.unwrap(),
        PayloadUsage {
            logical_current_bytes: 2 * bytes,
            unique_stored_bytes: bytes,
        }
    );
    delete(&db, author, &third).await?;
    assert_eq!(
        db.get_payload_usage().await?.unwrap(),
        PayloadUsage {
            logical_current_bytes: bytes,
            unique_stored_bytes: bytes,
        }
    );
    delete(&db, other, &second).await?;
    // General deletion does not nominate any bytes.
    assert_eq!(db.collect_quota_payload_garbage(limit(1)).await?.visited, 0);
    nominate(&db, &first).await?;
    let collected = db.collect_quota_payload_garbage(limit(1)).await?;
    assert_eq!(collected.visited, 1);
    assert_eq!(collected.removed_bytes, bytes);
    assert_eq!(
        db.get_payload_usage().await?.unwrap(),
        PayloadUsage::default()
    );
    // Empty delete envelopes have zero physical bytes and do not affect totals.
    db.try_process_event_content(&first).await?;
    assert_eq!(
        db.get_payload_usage().await?.unwrap(),
        PayloadUsage::default()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn accounting_historical_protected_hash_collision_blocks_quota_gc() -> anyhow::Result<()> {
    for local in [false, true] {
        let holder = RostraIdSecretKey::generate();
        let remote = RostraIdSecretKey::generate();
        let db = Database::new_in_memory(holder.id()).await?;
        let expendable = post(remote, 1, "same bytes across kinds");
        db.try_process_event_with_content(&expendable).await?;
        let protected_author = if local {
            holder
        } else {
            RostraIdSecretKey::generate()
        };
        let content = expendable.content.as_ref().unwrap();
        let signed = Event::builder_raw_content()
            .author(protected_author.id())
            .kind(if local {
                EventKind::SOCIAL_POST
            } else {
                EventKind::RAW
            })
            .timestamp(Timestamp::from(2).to_offset_date_time().unwrap())
            .content(content)
            .build()
            .signed_by(protected_author);
        let protected = VerifiedEventContent::assume_verified(
            VerifiedEvent::verify_signed(protected_author.id(), signed).unwrap(),
            content.clone(),
        );
        db.try_process_event_with_content(&protected).await?;
        delete(&db, protected_author, &protected).await?;
        delete(&db, remote, &expendable).await?;
        let before = ready(&db).await?;
        nominate(&db, &expendable).await?;
        assert_eq!(
            db.collect_quota_payload_garbage(limit(1))
                .await?
                .removed_bytes,
            0
        );
        assert_eq!(db.get_payload_usage().await?.unwrap(), before);
        assert_eq!(before.logical_current_bytes, 0);
        assert_eq!(
            before.unique_stored_bytes,
            u64::from(expendable.content_len())
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn accounting_backfill_interleaves_ingestion_reopen_and_total_replay() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("disposable.redb");
    let holder = RostraIdSecretKey::generate().id();
    let author = RostraIdSecretKey::generate();
    let mut posts = (1..=8)
        .map(|n| post(author, n, &format!("post {n}")))
        .collect::<Vec<_>>();
    posts.sort_by_key(|post| post.event_id().to_short());
    let db = Database::open(&path, holder).await?;
    for post in &posts[1..7] {
        db.write_with(|tx| {
            db.process_event_tx(&post.event, Timestamp::from(50), tx)?;
            Ok(())
        })
        .await?;
    }
    assert!(matches!(
        db.collect_quota_payload_garbage(limit(1)).await,
        Err(DbError::PayloadAccountingNotReady)
    ));
    assert!(!db.rebuild_payload_accounting(limit(3)).await?.ready);
    // Insert on both sides of the durable event cursor and deliver old Missing
    // content on both sides. Every contribution must be counted exactly once.
    for post in &posts {
        db.try_process_event_with_content(post).await?;
    }
    delete(&db, author, &posts[2]).await?;
    let partial = db.get_payload_usage().await?;
    assert_eq!(partial, None);
    drop(db);
    let db = Database::open(&path, holder).await?;
    let expected = ready(&db).await?;
    assert_eq!(
        expected.unique_stored_bytes,
        posts
            .iter()
            .map(|p| u64::from(p.content_len()))
            .sum::<u64>()
    );
    assert_eq!(
        expected.logical_current_bytes,
        expected.unique_stored_bytes - u64::from(posts[2].content_len())
    );
    db.write_with(|tx| Database::prepare_total_migration(tx, 28))
        .await?;
    drop(db);
    let db = Database::open(&path, holder).await?;
    assert_eq!(db.get_payload_usage().await?, None);
    // Existing total replay restores only still-usable bytes, not general
    // unreferenced Deleted garbage. Accounting must describe the rebuilt store.
    let replayed = ready(&db).await?;
    assert_eq!(
        replayed.logical_current_bytes,
        expected.logical_current_bytes
    );
    assert_eq!(replayed.unique_stored_bytes, expected.logical_current_bytes);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn accounting_and_gc_fail_closed_and_rollback_together() -> anyhow::Result<()> {
    let holder = RostraIdSecretKey::generate().id();
    let author = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(holder).await?;
    let event = post(author, 1, "rollback bytes");
    db.try_process_event_with_content(&event).await?;
    ready(&db).await?;
    let before = db.get_payload_usage().await?;
    let aborted: crate::DbResult<()> = db
        .write_with(|tx| {
            let next = post(author, 2, "aborted");
            db.process_event_tx(&next.event, Timestamp::from(2), tx)?;
            db.process_event_content_tx(&next, Timestamp::from(2), tx)?;
            crate::PayloadAccountingInvariantSnafu.fail()
        })
        .await;
    assert!(aborted.is_err());
    assert_eq!(db.get_payload_usage().await?, before);
    delete(&db, author, &event).await?;
    nominate(&db, &event).await?;
    let before_gc = db.get_payload_usage().await?;
    let aborted: crate::DbResult<()> = db
        .write_with(|tx| {
            let page = Database::collect_quota_payload_garbage_tx(tx, limit(1))?;
            assert_eq!(page.removed_bytes, u64::from(event.content_len()));
            crate::PayloadAccountingInvariantSnafu.fail()
        })
        .await;
    assert!(aborted.is_err());
    assert_eq!(db.get_payload_usage().await?, before_gc);
    assert_eq!(
        db.collect_quota_payload_garbage(limit(1))
            .await?
            .removed_bytes,
        u64::from(event.content_len())
    );
    assert!(matches!(
        db.rebuild_payload_accounting(limit(crate::PAYLOAD_MAINTENANCE_MAX + 1))
            .await,
        Err(DbError::PayloadMaintenanceLimit)
    ));
    assert!(matches!(
        db.collect_quota_payload_garbage(limit(crate::PAYLOAD_MAINTENANCE_MAX + 1))
            .await,
        Err(DbError::PayloadMaintenanceLimit)
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn accounting_corrupt_rc_and_usage_never_become_ready() -> anyhow::Result<()> {
    for corrupt_rc in [false, true] {
        let author = RostraIdSecretKey::generate();
        let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
        let event = post(author, 1, "corruption fixture");
        db.try_process_event_with_content(&event).await?;
        db.write_with(|tx| {
            if corrupt_rc {
                tx.open_table(&crate::content_rc::TABLE)?
                    .remove(&event.content_hash())?;
            } else {
                tx.open_table(&crate::ids_data_usage::TABLE)?
                    .remove(&author.id())?;
            }
            Ok(())
        })
        .await?;
        assert!(matches!(
            db.rebuild_payload_accounting(limit(100)).await,
            Err(DbError::PayloadAccountingInvariant)
        ));
        assert_eq!(db.get_payload_usage().await?, None);
        assert!(matches!(
            db.collect_quota_payload_garbage(limit(1)).await,
            Err(DbError::PayloadAccountingNotReady)
        ));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn accounting_checked_reducers_reject_underflow_and_overflow() -> anyhow::Result<()> {
    let author = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(author.id()).await?;
    let event = post(author, 1, "checked arithmetic");
    let result = db
        .write_with(|tx| {
            Database::decrement_content_rc_tx(
                event.content_hash(),
                &mut tx.open_table(&crate::content_rc::TABLE)?,
            )
        })
        .await;
    assert!(matches!(result, Err(DbError::PayloadAccountingInvariant)));
    let result = db
        .write_with(|tx| {
            let mut rc = tx.open_table(&crate::content_rc::TABLE)?;
            rc.insert(&event.content_hash(), &u64::MAX)?;
            Database::increment_content_rc_tx(event.content_hash(), &mut rc)
        })
        .await;
    assert!(matches!(result, Err(DbError::Overflow)));
    let result = db
        .write_with(|tx| {
            Database::track_payload_processed_tx(
                author.id(),
                event.content_len(),
                &mut tx.open_table(&crate::ids_data_usage::TABLE)?,
            )
        })
        .await;
    assert!(matches!(result, Err(DbError::Overflow)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn accounting_content_cursor_and_gc_recheck_late_protected_headers() -> anyhow::Result<()> {
    use crate::payload_accounting::AccountingStage;

    let holder = RostraIdSecretKey::generate();
    let author = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(holder.id()).await?;
    let mut posts = (1..=8)
        .map(|n| post(author, n, &format!("content cursor {n}")))
        .collect::<Vec<_>>();
    posts.sort_by_key(|event| event.content_hash());
    for event in &posts {
        db.write_with(|tx| {
            db.process_event_tx(&event.event, Timestamp::from(20), tx)?;
            Ok(())
        })
        .await?;
    }
    for event in &posts[2..6] {
        db.try_process_event_content(event).await?;
    }
    loop {
        let progress = db.rebuild_payload_accounting(limit(1)).await?;
        assert!(!progress.ready);
        let stage = db
            .read_with(|tx| {
                Ok(tx
                    .open_table(&crate::content_accounting_state::TABLE)?
                    .get(&())?
                    .unwrap()
                    .value_try()?
                    .stage)
            })
            .await?;
        if matches!(stage, AccountingStage::Content(Some(hash)) if hash == posts[3].content_hash())
        {
            break;
        }
    }
    // Store new bytes both before and after the unique-content cursor.
    for event in &posts {
        db.try_process_event_content(event).await?;
    }
    let bytes = posts
        .iter()
        .map(|event| u64::from(event.content_len()))
        .sum::<u64>();
    assert_eq!(
        ready(&db).await?,
        PayloadUsage {
            logical_current_bytes: bytes,
            unique_stored_bytes: bytes,
        }
    );
    delete(&db, author, &posts[0]).await?;
    nominate(&db, &posts[0]).await?;

    // A protected header arrives AFTER nomination and releases its Missing RC
    // before GC. The historical guard must still prevent this collision.
    let protected = post(holder, 20, "content cursor 1");
    let target = posts
        .iter()
        .find(|event| event.content_hash() == protected.content_hash())
        .unwrap();
    if target.event_id() != posts[0].event_id() {
        delete(&db, author, target).await?;
        nominate(&db, target).await?;
    }
    db.write_with(|tx| {
        db.process_event_tx(&protected.event, Timestamp::from(20), tx)?;
        Ok(())
    })
    .await?;
    delete(&db, holder, &protected).await?;
    let before = db.get_payload_usage().await?.unwrap();
    let collected = db.collect_quota_payload_garbage(limit(1)).await?;
    assert_eq!(collected.visited, 1);
    // Drain the remaining nomination with the same bound.
    db.collect_quota_payload_garbage(limit(1)).await?;
    db.read_with(|tx| {
        assert!(
            tx.open_table(&crate::content_store::TABLE)?
                .get(&protected.content_hash())?
                .is_some()
        );
        Ok(())
    })
    .await?;
    let after = db.get_payload_usage().await?.unwrap();
    assert_eq!(before.logical_current_bytes, after.logical_current_bytes);
    assert!(after.unique_stored_bytes <= before.unique_stored_bytes);
    Ok(())
}
