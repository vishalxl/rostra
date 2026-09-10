use std::num::NonZeroUsize;

use rostra_core::event::content_kind::{EventContentKind as _, SocialPost};
use rostra_core::event::{Event, EventExt as _, EventKind, VerifiedEvent, VerifiedEventContent};
use rostra_core::id::{RostraIdSecretKey, ToShort as _};
use rostra_core::retention::RetentionPolicy;
use rostra_core::{ExternalEventId, Timestamp};

use crate::{
    Database, DbError, EventContentAvailability, EventContentState, PayloadUsage,
    QuotaPruneOutcome, QuotaPruneReason, QuotaPruneRequest, QuotaPruneTarget, RetentionClock,
};

#[tokio::test(flavor = "multi_thread")]
async fn quota_missing_edit_then_deleted_late_lineage_survives_reopen_replay() -> anyhow::Result<()>
{
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("late-edit.db");
    let local = RostraIdSecretKey::generate().id();
    let author = RostraIdSecretKey::generate();
    let parent = text(author, 1, "original");
    let edit = post(
        author,
        2,
        SocialPost::new_text("late edit".to_owned(), None, Default::default()),
        Some(parent.event_id().to_short()),
    );
    let req = request(&edit, QuotaPruneTarget::Missing);
    let tip;
    {
        let db = Database::open(&path, local).await?;
        ingest(&db, &parent, true).await?;
        tip = db.get_social_post_materialization_tip().await?;
        ingest(&db, &edit, false).await?;
        ready(&db).await?;
        db.prune_quota_payload(req).await?;
        delete(&db, author, &edit).await?;
        db.try_process_event_content(&edit).await?;
    }
    for replay in [false, true, false] {
        let db = Database::open(&path, local).await?;
        if replay {
            db.write_with(|tx| Database::prepare_total_migration(tx, 29))
                .await?;
            db.write_with(|tx| db.reprocess_migration_stash(tx)).await?;
            ready(&db).await?;
        }
        recover(&db).await?;
        db.collect_quota_payload_garbage(limit(10)).await?;
        assert_eq!(db.get_social_post_materialization_tip().await?, tip);
        assert!(db.get_event_content(req.id).await.is_none());
        assert!(matches!(
            db.get_event_content_state(req.id).await,
            Some(EventContentState::Deleted { .. })
        ));
        assert_eq!(
            db.get_payload_usage().await?.unwrap().logical_current_bytes,
            0
        );
        db.read_with(|tx| {
            let decision = tx
                .open_table(&crate::events_quota_pruned::TABLE)?
                .get(&req.id)?
                .unwrap()
                .value_try()?;
            assert_eq!(decision.reason, req.reason);
            assert_eq!(decision.pruned_at, Timestamp::from(200));
            assert!(
                tx.open_table(&crate::content_store::TABLE)?
                    .get(&edit.content_hash())?
                    .is_none()
            );
            assert!(
                tx.open_table(&crate::content_rc::TABLE)?
                    .get(&edit.content_hash())?
                    .is_none()
            );
            assert!(
                tx.open_table(&crate::events_content_missing::TABLE)?
                    .first()?
                    .is_none()
            );
            assert!(
                tx.open_table(&crate::social_posts_by_time::TABLE)?
                    .get(&(edit.timestamp(), req.id))?
                    .is_none()
            );
            assert!(
                tx.open_table(&crate::social_posts_received_at_keys::TABLE)?
                    .get(&req.id)?
                    .is_none()
            );
            assert!(
                tx.open_table(&crate::social_posts_replaces::TABLE)?
                    .get(&(author.id(), req.id, parent.event_id().to_short()))?
                    .is_some()
            );
            assert!(
                tx.open_table(&crate::social_posts_replaced_by::TABLE)?
                    .get(&(author.id(), parent.event_id().to_short(), req.id))?
                    .is_some()
            );
            Ok(())
        })
        .await?;
    }
    Ok(())
}

fn limit(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn post(
    secret: RostraIdSecretKey,
    time: u64,
    content: SocialPost,
    replaced: Option<rostra_core::ShortEventId>,
) -> VerifiedEventContent {
    let bytes = content.serialize_cbor().unwrap();
    let signed = Event::builder_raw_content()
        .author(secret.id())
        .timestamp(Timestamp::from(time).to_offset_date_time().unwrap())
        .kind(EventKind::SOCIAL_POST)
        .content(&bytes)
        .maybe_delete(replaced)
        .build()
        .signed_by(secret);
    VerifiedEventContent::assume_verified(
        VerifiedEvent::verify_signed(secret.id(), signed).unwrap(),
        bytes,
    )
}

fn text(secret: RostraIdSecretKey, time: u64, body: &str) -> VerifiedEventContent {
    post(
        secret,
        time,
        SocialPost::new_text(body.to_owned(), None, Default::default()),
        None,
    )
}

fn request(event: &VerifiedEventContent, target: QuotaPruneTarget) -> QuotaPruneRequest {
    QuotaPruneRequest {
        id: event.event_id().to_short(),
        target,
        reason: QuotaPruneReason::AuthorQuota,
        policy: RetentionPolicy::new(1, 1, 0, 0, 1, 10).unwrap(),
        clock: RetentionClock::Trusted(Timestamp::from(200)),
    }
}

async fn ingest(db: &Database, event: &VerifiedEventContent, content: bool) -> crate::DbResult<()> {
    db.write_with(|tx| {
        db.process_event_tx(&event.event, Timestamp::from(100), tx)?;
        if content {
            db.process_event_content_tx(event, Timestamp::from(100), tx)?;
        }
        Ok(())
    })
    .await
}

async fn ready(db: &Database) -> crate::DbResult<()> {
    for _ in 0..1000 {
        if db.rebuild_payload_accounting(limit(1)).await?.ready {
            return Ok(());
        }
    }
    panic!("fixture rebuild failed to finish");
}

async fn recover(db: &Database) -> crate::DbResult<()> {
    for _ in 0..1000 {
        let page = db.rebuild_quota_payload_nominations(limit(1)).await?;
        assert!(page.visited <= 1);
        if page.ready {
            return Ok(());
        }
    }
    panic!("fixture recovery failed to finish");
}

async fn delete(
    db: &Database,
    secret: RostraIdSecretKey,
    event: &VerifiedEventContent,
) -> crate::DbResult<()> {
    let signed = Event::builder_raw_content()
        .author(secret.id())
        .kind(EventKind::SOCIAL_POST)
        .timestamp(Timestamp::from(300).to_offset_date_time().unwrap())
        .delete(event.event_id().to_short())
        .build()
        .signed_by(secret);
    db.try_process_event(&VerifiedEvent::verify_signed(secret.id(), signed).unwrap())
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_checked_eligibility_and_stale_missing_selection() -> anyhow::Result<()> {
    let local = RostraIdSecretKey::generate();
    let remote = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(local.id()).await?;
    let event = text(remote, 1, "remote");
    let local_event = text(local, 2, "protected");
    ingest(&db, &event, true).await?;
    ingest(&db, &local_event, true).await?;
    let req = request(&event, QuotaPruneTarget::Processed);
    assert!(matches!(
        db.prune_quota_payload(req).await,
        Err(DbError::PayloadAccountingNotReady)
    ));
    ready(&db).await?;
    for clock in [
        RetentionClock::Untrusted,
        RetentionClock::Trusted(99.into()),
        RetentionClock::Trusted(109.into()),
    ] {
        assert_eq!(
            db.prune_quota_payload(QuotaPruneRequest { clock, ..req })
                .await?,
            QuotaPruneOutcome::Ineligible
        );
    }
    assert_eq!(
        db.prune_quota_payload(request(&local_event, QuotaPruneTarget::Processed))
            .await?,
        QuotaPruneOutcome::Ineligible
    );
    assert_eq!(
        db.prune_quota_payload(request(&event, QuotaPruneTarget::Missing))
            .await?,
        QuotaPruneOutcome::Unchanged
    );
    db.write_with(|tx| {
        tx.open_table(&crate::events_retention_origins::TABLE)?
            .remove(&req.id)?;
        Ok(())
    })
    .await?;
    assert_eq!(
        db.prune_quota_payload(req).await?,
        QuotaPruneOutcome::Ineligible
    );
    assert!(db.get_event_content(req.id).await.is_some());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn canonical_post_state_reports_a_pruned_latest_replacement() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let author = RostraIdSecretKey::generate();
    let original = text(author, 1, "original");
    let replacement = post(
        author,
        2,
        SocialPost::new_text("replacement".to_owned(), None, Default::default()),
        Some(original.event_id().to_short()),
    );
    ingest(&db, &original, true).await?;
    ingest(&db, &replacement, true).await?;
    ready(&db).await?;

    assert_eq!(
        db.prune_quota_payload(request(&replacement, QuotaPruneTarget::Processed))
            .await?,
        QuotaPruneOutcome::Pruned {
            logical_released_bytes: u64::from(replacement.content_len())
        }
    );
    let state = db
        .get_social_post_state(original.event_id().to_short())
        .await
        .expect("original envelope remains retained");
    assert_eq!(state.event_id, replacement.event_id().to_short());
    assert_eq!(state.author, author.id());
    assert_eq!(state.timestamp, replacement.timestamp());
    assert!(state.post.is_none());
    assert_eq!(
        state.availability,
        EventContentAvailability::Pruned {
            quota_reason: Some(QuotaPruneReason::AuthorQuota)
        }
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_shared_missing_release_requeues_consumed_nomination() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let b = RostraIdSecretKey::generate();
    let first = text(a, 1, "shared");
    let second = text(b, 2, "shared");
    ingest(&db, &first, true).await?;
    ingest(&db, &second, false).await?;
    ready(&db).await?;
    let bytes = u64::from(first.content_len());
    assert_eq!(
        db.prune_quota_payload(request(&first, QuotaPruneTarget::Processed))
            .await?,
        QuotaPruneOutcome::Pruned {
            logical_released_bytes: bytes
        }
    );
    assert_eq!(
        db.get_payload_usage().await?,
        Some(PayloadUsage {
            logical_current_bytes: 0,
            unique_stored_bytes: bytes
        })
    );
    let blocked = db.collect_quota_payload_garbage(limit(1)).await?;
    assert_eq!((blocked.visited, blocked.removed_bytes), (1, 0));
    // Ordinary signed deletion completes only the existing quota-owned work.
    delete(&db, b, &second).await?;
    assert_eq!(
        db.collect_quota_payload_garbage(limit(1))
            .await?
            .removed_bytes,
        bytes
    );
    assert_eq!(
        db.get_payload_usage().await?.unwrap().unique_stored_bytes,
        0
    );
    ingest(&db, &first, true).await?;
    assert!(db.get_event_content(first.event_id()).await.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_missing_decline_duplicate_races_and_deleted_precedence() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let author = RostraIdSecretKey::generate();
    let event = text(author, 1, "not yet fetched");
    ingest(&db, &event, false).await?;
    ready(&db).await?;
    let mut notifications = db.quota_pruned_subscribe();
    let req = request(&event, QuotaPruneTarget::Missing);
    let (a, b) = tokio::join!(db.prune_quota_payload(req), db.prune_quota_payload(req));
    assert!(matches!(
        (a?, b?),
        (
            QuotaPruneOutcome::Pruned {
                logical_released_bytes: 0
            },
            QuotaPruneOutcome::Unchanged
        ) | (
            QuotaPruneOutcome::Unchanged,
            QuotaPruneOutcome::Pruned {
                logical_released_bytes: 0
            }
        )
    ));
    assert_eq!(notifications.try_recv()?, req.id);
    assert!(notifications.try_recv().is_err());
    assert_eq!(
        db.get_event_content_availability(req.id).await,
        Some(EventContentAvailability::Pruned {
            quota_reason: Some(QuotaPruneReason::AuthorQuota)
        })
    );
    let (delivery, deletion) = tokio::join!(
        db.try_process_event_content(&event),
        delete(&db, author, &event)
    );
    delivery?;
    deletion?;
    assert!(matches!(
        db.get_event_content_state(req.id).await,
        Some(EventContentState::Deleted { .. })
    ));
    assert_eq!(
        db.get_event_content_availability(req.id).await,
        Some(EventContentAvailability::Deleted)
    );
    assert_eq!(
        db.prune_quota_payload(req).await?,
        QuotaPruneOutcome::Unchanged
    );
    db.read_with(|tx| {
        let decision = tx
            .open_table(&crate::events_quota_pruned::TABLE)?
            .get(&req.id)?
            .unwrap()
            .value_try()?;
        assert_eq!(decision.reason, QuotaPruneReason::AuthorQuota);
        assert_eq!(decision.pruned_at, Timestamp::from(200));
        assert!(
            tx.open_table(&crate::events_content_missing::TABLE)?
                .first()?
                .is_none()
        );
        assert!(
            tx.open_table(&crate::content_rc::TABLE)?
                .get(&event.content_hash())?
                .is_none()
        );
        Ok(())
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_reversion_failure_and_explicit_abort_are_atomic() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let author = RostraIdSecretKey::generate();
    let parent = text(author, 1, "parent");
    let event = post(
        author,
        2,
        SocialPost::new_text(
            "reply".to_owned(),
            Some(ExternalEventId::new(
                author.id(),
                parent.event_id().to_short(),
            )),
            Default::default(),
        ),
        None,
    );
    ingest(&db, &parent, true).await?;
    ingest(&db, &event, true).await?;
    ready(&db).await?;
    let req = request(&event, QuotaPruneTarget::Processed);
    let usage = db.get_payload_usage().await?;
    let mut notifications = db.quota_pruned_subscribe();
    let result: crate::DbResult<()> = db
        .write_with(|tx| {
            db.prune_quota_payload_tx(tx, req)?;
            crate::OverflowSnafu.fail()
        })
        .await;
    assert!(matches!(result, Err(DbError::Overflow)));
    assert_eq!(db.get_payload_usage().await?, usage);
    assert!(notifications.try_recv().is_err());
    db.write_with(|tx| {
        let mut posts = tx.open_table(&crate::social_posts::TABLE)?;
        let mut record = posts
            .get(&parent.event_id().to_short())?
            .unwrap()
            .value_try()?;
        record.reply_count = 0;
        posts.insert(&parent.event_id().to_short(), &record)?;
        Ok(())
    })
    .await?;
    assert!(matches!(
        db.prune_quota_payload(req).await,
        Err(DbError::Overflow)
    ));
    assert_eq!(db.get_payload_usage().await?, usage);
    assert!(db.get_event_content(req.id).await.is_some());
    assert!(notifications.try_recv().is_err());
    db.read_with(|tx| {
        assert!(
            tx.open_table(&crate::events_quota_pruned::TABLE)?
                .get(&req.id)?
                .is_none()
        );
        assert!(
            tx.open_table(&crate::content_quota_hashes::TABLE)?
                .get(&event.content_hash())?
                .is_none()
        );
        assert!(
            tx.open_table(&crate::social_posts_by_time::TABLE)?
                .get(&(event.timestamp(), req.id))?
                .is_some()
        );
        Ok(())
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_dematerializes_reply_reaction_news_mention_edit_and_preserves_feed()
-> anyhow::Result<()> {
    for edit in [false, true] {
        let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
        let author = RostraIdSecretKey::generate();
        let parent = text(author, 1, "original");
        let reply_target = text(author, 2, "reply target");
        let target_id = reply_target.event_id().to_short();
        let mut content = SocialPost::new_text(
            format!("body <rostra:{}>", db.self_id),
            Some(ExternalEventId::new(author.id(), target_id)),
            Default::default(),
        )
        .with_news_fields(None, Some("news".to_owned()));
        content.reaction = Some("👍".to_owned());
        let event = post(
            author,
            3,
            content,
            edit.then_some(parent.event_id().to_short()),
        );
        for event in [&parent, &reply_target, &event] {
            ingest(&db, event, true).await?;
        }
        ready(&db).await?;
        let req = request(&event, QuotaPruneTarget::Processed);
        let tip = db.get_social_post_materialization_tip().await?;
        db.read_with(|tx| {
            let record = tx
                .open_table(&crate::social_posts::TABLE)?
                .get(&target_id)?
                .unwrap()
                .value_try()?;
            assert_eq!((record.reply_count, record.reaction_count), (1, 1));
            assert!(
                tx.open_table(&crate::social_posts_self_mention::TABLE)?
                    .get(&req.id)?
                    .is_some()
            );
            assert!(
                tx.open_table(&crate::social_news_rank_by_post_id::TABLE)?
                    .get(&ExternalEventId::new(author.id(), req.id))?
                    .is_some()
            );
            Ok(())
        })
        .await?;
        assert!(matches!(
            db.prune_quota_payload(req).await?,
            QuotaPruneOutcome::Pruned { .. }
        ));
        assert!(matches!(
            db.get_event_content_state(req.id).await,
            Some(EventContentState::Pruned)
        ));
        for replay in [false, true] {
            if replay {
                db.write_with(|tx| Database::prepare_total_migration(tx, 29))
                    .await?;
                db.write_with(|tx| db.reprocess_migration_stash(tx)).await?;
                ready(&db).await?;
                recover(&db).await?;
            }
            ingest(&db, &event, true).await?;
            assert_eq!(db.get_social_post_materialization_tip().await?, tip);
            let page = db
                .scan_social_post_materializations(None, limit(10))
                .await?;
            assert!(
                page.items
                    .contains(&crate::SocialPostMaterialization::Removed {
                        post_id: ExternalEventId::new(author.id(), req.id),
                    })
            );
            db.read_with(|tx| {
                let record = tx
                    .open_table(&crate::social_posts::TABLE)?
                    .get(&target_id)?
                    .map(|r| r.value_try())
                    .transpose()?
                    .unwrap_or_default();
                assert_eq!((record.reply_count, record.reaction_count), (0, 0));
                assert!(
                    tx.open_table(&crate::social_posts_by_time::TABLE)?
                        .get(&(event.timestamp(), req.id))?
                        .is_none()
                );
                assert!(
                    tx.open_table(&crate::social_posts_received_at_keys::TABLE)?
                        .get(&req.id)?
                        .is_none()
                );
                assert!(
                    tx.open_table(&crate::social_posts_by_received_at::TABLE)?
                        .range(..)?
                        .all(|row| row.unwrap().1.value() != req.id)
                );
                assert!(
                    tx.open_table(&crate::social_posts_self_mention::TABLE)?
                        .get(&req.id)?
                        .is_none()
                );
                assert!(
                    tx.open_table(&crate::social_news_rank_by_post_id::TABLE)?
                        .get(&ExternalEventId::new(author.id(), req.id))?
                        .is_none()
                );
                assert!(
                    tx.open_table(&crate::social_posts_replies::TABLE)?
                        .get(&(target_id, event.timestamp(), req.id))?
                        .is_none()
                );
                assert!(
                    tx.open_table(&crate::social_posts_reactions::TABLE)?
                        .get(&(target_id, event.timestamp(), req.id))?
                        .is_none()
                );
                for present in [
                    tx.open_table(&crate::social_posts_replaces::TABLE)?
                        .get(&(author.id(), req.id, parent.event_id().to_short()))?
                        .is_some(),
                    tx.open_table(&crate::social_posts_replaced_by::TABLE)?
                        .get(&(author.id(), parent.event_id().to_short(), req.id))?
                        .is_some(),
                ] {
                    assert_eq!(present, edit);
                }
                Ok(())
            })
            .await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_protected_shared_history_survives_quota_release_and_recovery() -> anyhow::Result<()>
{
    for kind in [
        EventKind::SOCIAL_POST,
        EventKind::SOCIAL_VOTE,
        EventKind::from(65535),
    ] {
        let local = RostraIdSecretKey::generate();
        let remote = RostraIdSecretKey::generate();
        let db = Database::new_in_memory(local.id()).await?;
        let event = text(remote, 1, "shared protected bytes");
        let guard_author = if kind == EventKind::SOCIAL_POST {
            local
        } else {
            remote
        };
        let signed = Event::builder_raw_content()
            .author(guard_author.id())
            .kind(kind)
            .timestamp(Timestamp::from(2).to_offset_date_time().unwrap())
            .content(event.content.as_ref().unwrap())
            .build()
            .signed_by(guard_author);
        let guard = VerifiedEventContent::assume_verified(
            VerifiedEvent::verify_signed(guard_author.id(), signed).unwrap(),
            event.content.clone(),
        );
        ingest(&db, &event, true).await?;
        ingest(&db, &guard, false).await?;
        ready(&db).await?;
        assert_eq!(
            db.prune_quota_payload(request(&guard, QuotaPruneTarget::Missing))
                .await?,
            QuotaPruneOutcome::Ineligible
        );
        db.prune_quota_payload(request(&event, QuotaPruneTarget::Processed))
            .await?;
        delete(&db, guard_author, &guard).await?;
        assert_eq!(
            db.collect_quota_payload_garbage(limit(1))
                .await?
                .removed_bytes,
            0
        );
        recover(&db).await?;
        assert_eq!(
            db.collect_quota_payload_garbage(limit(1))
                .await?
                .removed_bytes,
            0
        );
        assert_eq!(
            db.get_payload_usage().await?.unwrap().unique_stored_bytes,
            u64::from(event.content_len())
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_recovery_reopen_replay_interleaving_and_rollback() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("quota.db");
    let local = RostraIdSecretKey::generate().id();
    let author = RostraIdSecretKey::generate();
    let first = text(author, 1, "shared recovery");
    let second = text(author, 2, "shared recovery");
    let missing = text(author, 3, "declined");
    {
        let db = Database::open(&path, local).await?;
        ingest(&db, &first, true).await?;
        ingest(&db, &second, true).await?;
        ingest(&db, &missing, false).await?;
        ready(&db).await?;
        db.prune_quota_payload(request(&first, QuotaPruneTarget::Processed))
            .await?;
        db.prune_quota_payload(request(&missing, QuotaPruneTarget::Missing))
            .await?;
        db.write_with(|tx| Database::prepare_total_migration(tx, 29))
            .await?;
        db.write_with(|tx| db.reprocess_migration_stash(tx)).await?;
        ready(&db).await?;
        assert_eq!(
            db.collect_quota_payload_garbage(limit(10)).await?.visited,
            0
        );
        let aborted: crate::DbResult<()> = db
            .write_with(|tx| {
                db.rebuild_quota_payload_nominations_tx(tx, limit(1))?;
                crate::OverflowSnafu.fail()
            })
            .await;
        assert!(aborted.is_err());
        assert_eq!(
            db.collect_quota_payload_garbage(limit(10)).await?.visited,
            0
        );
        let page = db.rebuild_quota_payload_nominations(limit(1)).await?;
        assert_eq!(page.visited, 1);
        assert!(!page.ready);
    }
    {
        let db = Database::open(&path, local).await?;
        // Release the surviving reference while recovery is only partially done.
        delete(&db, author, &second).await?;
        recover(&db).await?;
        let result = db.collect_quota_payload_garbage(limit(10)).await?;
        assert_eq!(result.removed_bytes, u64::from(first.content_len()));
        for event in [&first, &missing] {
            ingest(&db, event, true).await?;
            assert!(matches!(
                db.get_event_content_state(event.event_id()).await,
                Some(EventContentState::Pruned)
            ));
        }
        assert_eq!(
            db.get_payload_usage().await?.unwrap(),
            PayloadUsage::default()
        );
        assert_eq!(
            db.rebuild_quota_payload_nominations(limit(1))
                .await?
                .visited,
            0
        );
        assert!(
            db.rebuild_quota_payload_nominations(limit(4097))
                .await
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_missing_shared_prune_preserves_current_reference_and_exact_author_buckets()
-> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let b = RostraIdSecretKey::generate();
    let current = text(a, 1, "shared current and missing");
    let missing = text(b, 2, "shared current and missing");
    ingest(&db, &current, true).await?;
    ingest(&db, &missing, false).await?;
    ready(&db).await?;
    let bytes = u64::from(current.content_len());
    assert_eq!(
        db.prune_quota_payload(request(&missing, QuotaPruneTarget::Missing))
            .await?,
        QuotaPruneOutcome::Pruned {
            logical_released_bytes: 0
        }
    );
    assert_eq!(
        db.get_payload_usage().await?,
        Some(PayloadUsage {
            logical_current_bytes: bytes,
            unique_stored_bytes: bytes,
        })
    );
    assert_eq!(
        db.collect_quota_payload_garbage(limit(1))
            .await?
            .removed_bytes,
        0
    );
    assert!(db.get_event_content(current.event_id()).await.is_some());
    db.read_with(|tx| {
        let table = tx.open_table(&crate::ids_data_usage::TABLE)?;
        let a_usage = table.get(&a.id())?.unwrap().value_try()?;
        let b_usage = table.get(&b.id())?.unwrap().value_try()?;
        assert_eq!(
            (a_usage.current_content_size, a_usage.current_payload_num),
            (bytes, 1)
        );
        assert_eq!(
            (a_usage.pruned_payload_size, a_usage.pruned_payload_num),
            (0, 0)
        );
        assert_eq!(
            (b_usage.current_content_size, b_usage.current_payload_num),
            (0, 0)
        );
        assert_eq!(
            (b_usage.missing_payload_size, b_usage.missing_payload_num),
            (0, 0)
        );
        assert_eq!(
            (b_usage.pruned_payload_size, b_usage.pruned_payload_num),
            (bytes, 1)
        );
        assert_eq!(
            (b_usage.total_content_size, b_usage.total_payload_num),
            (bytes, 1)
        );
        Ok(())
    })
    .await?;
    db.prune_quota_payload(request(&current, QuotaPruneTarget::Processed))
        .await?;
    assert_eq!(
        db.collect_quota_payload_garbage(limit(1))
            .await?
            .removed_bytes,
        bytes
    );
    assert_eq!(db.get_payload_usage().await?, Some(PayloadUsage::default()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_processed_delete_and_delivery_race_has_one_reference_release() -> anyhow::Result<()>
{
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let author = RostraIdSecretKey::generate();
    let event = text(author, 1, "race");
    ingest(&db, &event, true).await?;
    ready(&db).await?;
    let req = request(&event, QuotaPruneTarget::Processed);
    let (prune, delivery, deletion) = tokio::join!(
        db.prune_quota_payload(req),
        db.try_process_event_content(&event),
        delete(&db, author, &event),
    );
    let outcome = prune?;
    delivery?;
    deletion?;
    assert!(matches!(
        outcome,
        QuotaPruneOutcome::Pruned { .. } | QuotaPruneOutcome::Unchanged
    ));
    assert!(matches!(
        db.get_event_content_state(req.id).await,
        Some(EventContentState::Deleted { .. })
    ));
    assert!(db.get_event_content(req.id).await.is_none());
    db.read_with(|tx| {
        let usage = tx
            .open_table(&crate::ids_data_usage::TABLE)?
            .get(&author.id())?
            .unwrap()
            .value_try()?;
        assert_eq!(usage.current_content_size, 0);
        assert_eq!(usage.pruned_payload_size, 0);
        assert_eq!(usage.deleted_payload_size, u64::from(event.content_len()));
        assert!(
            tx.open_table(&crate::content_rc::TABLE)?
                .get(&event.content_hash())?
                .is_none()
        );
        assert_eq!(
            tx.open_table(&crate::events_quota_pruned::TABLE)?
                .get(&req.id)?
                .is_some(),
            matches!(outcome, QuotaPruneOutcome::Pruned { .. }),
        );
        Ok(())
    })
    .await?;
    let collected = db.collect_quota_payload_garbage(limit(1)).await?;
    assert_eq!(
        collected.removed_bytes,
        if matches!(outcome, QuotaPruneOutcome::Pruned { .. }) {
            u64::from(event.content_len())
        } else {
            0
        }
    );
    Ok(())
}
