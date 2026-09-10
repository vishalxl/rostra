use std::num::NonZeroUsize;

use rostra_core::Timestamp;
use rostra_core::event::content_kind::{EventContentKind as _, SocialPost, SocialProfileUpdate};
use rostra_core::event::{Event, EventExt as _, EventKind, VerifiedEvent, VerifiedEventContent};
use rostra_core::id::{RostraIdSecretKey, ToShort as _};
use rostra_core::retention::RetentionPolicy;

use crate::{
    Database, QuotaPruneOutcome, QuotaPruneReason, QuotaPruneRequest, QuotaPruneTarget,
    RetentionClock, RetentionGeneration,
};

fn limit(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn policy() -> RetentionPolicy {
    RetentionPolicy::new(1, 1, 0, 0, 1, 10).unwrap()
}

fn clock(now: u64) -> RetentionClock {
    RetentionClock::Trusted(Timestamp::from(now))
}

fn post(secret: RostraIdSecretKey, time: u64) -> VerifiedEventContent {
    let bytes = SocialPost::new_text(format!("post {time}"), None, Default::default())
        .serialize_cbor()
        .unwrap();
    let signed = Event::builder_raw_content()
        .author(secret.id())
        .kind(EventKind::SOCIAL_POST)
        .timestamp(Timestamp::from(time).to_offset_date_time().unwrap())
        .content(&bytes)
        .build()
        .signed_by(secret);
    VerifiedEventContent::assume_verified(
        VerifiedEvent::verify_signed(secret.id(), signed).unwrap(),
        bytes,
    )
}

async fn ingest(
    db: &Database,
    event: &VerifiedEventContent,
    materialize: bool,
) -> crate::DbResult<()> {
    db.write_with(|tx| {
        db.process_event_tx(&event.event, Timestamp::from(100), tx)?;
        if materialize {
            db.process_event_content_tx(event, Timestamp::from(100), tx)?;
        }
        Ok(())
    })
    .await
}

async fn ready(db: &Database) -> crate::DbResult<()> {
    for _ in 0..1000 {
        let progress = db.rebuild_retention_index(limit(1)).await?.unwrap();
        assert!(progress.visited <= 1);
        if progress.ready {
            return Ok(());
        }
    }
    panic!("index did not finish");
}

async fn accounting(db: &Database) -> crate::DbResult<()> {
    while !db.rebuild_payload_accounting(limit(1)).await?.ready {}
    Ok(())
}

async fn delete(
    db: &Database,
    secret: RostraIdSecretKey,
    target: &VerifiedEventContent,
) -> crate::DbResult<()> {
    let signed = Event::builder_raw_content()
        .author(secret.id())
        .kind(EventKind::SOCIAL_POST)
        .timestamp(Timestamp::from(300).to_offset_date_time().unwrap())
        .delete(target.event_id().to_short())
        .build()
        .signed_by(secret);
    let event = VerifiedEvent::verify_signed(secret.id(), signed).unwrap();
    db.write_with(|tx| {
        db.process_event_tx(&event, Timestamp::from(300), tx)
            .map(|_| ())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_index_static_order_and_independent_readiness() -> anyhow::Result<()> {
    let holder = RostraIdSecretKey::generate().id();
    let db = Database::new_in_memory(holder).await?;
    let a = RostraIdSecretKey::generate();
    let b = RostraIdSecretKey::generate();
    let events = [post(a, 3), post(b, 1), post(a, 2), post(b, 2)];
    for event in &events {
        ingest(&db, event, true).await?;
    }
    assert!(db.retention_index_progress().await?.is_none());
    accounting(&db).await?;
    let generation = db.configure_retention_index(policy()).await?.generation;
    assert_eq!(generation.holder(), holder);
    assert_eq!(generation.policy_bytes(), policy().to_bytes());
    assert!(
        !db.select_retention_candidates(generation, None, clock(200), None, limit(10))
            .await?
            .ready
    );
    ready(&db).await?;
    let empty = db
        .select_retention_candidates(generation, None, clock(200), None, limit(10))
        .await?;
    assert!(empty.ready);
    assert!(empty.candidates.is_empty());
    let waiting = crate::RetentionCandidate {
        event: events[0].event_id(),
        author: a.id(),
        key: policy()
            .key(
                events[0].event_id(),
                holder,
                events[0].content_len(),
                events[0].timestamp(),
            )
            .to_bytes(),
    };
    assert!(
        !db.retention_candidate_is_current(generation, waiting, clock(200))
            .await?
    );
    assert_eq!(
        db.promote_retention_grace(clock(109), limit(10))
            .await?
            .unwrap()
            .visited,
        0
    );
    assert_eq!(
        db.promote_retention_grace(clock(110), limit(2))
            .await?
            .unwrap()
            .visited,
        2
    );
    assert_eq!(
        db.promote_retention_grace(clock(110), limit(2))
            .await?
            .unwrap()
            .visited,
        2
    );
    let page = db
        .select_retention_candidates(generation, None, clock(110), None, limit(10))
        .await?;
    assert!(
        db.retention_candidate_is_current(generation, waiting, clock(110))
            .await?
    );
    let mut expected: Vec<_> = events
        .iter()
        .map(|event| {
            policy()
                .key(
                    event.event_id(),
                    holder,
                    event.content_len(),
                    event.timestamp(),
                )
                .to_bytes()
        })
        .collect();
    expected.sort();
    assert_eq!(
        page.candidates
            .iter()
            .map(|candidate| candidate.key)
            .collect::<Vec<_>>(),
        expected
    );
    for candidate in &page.candidates {
        assert_eq!(&candidate.key[16..], candidate.event.as_slice());
        assert!(
            db.retention_candidate_is_current(generation, *candidate, clock(110))
                .await?
        );
    }
    // Zero exponents pin the canonical sign-flipped, big-endian Q32 prefix.
    assert_eq!(
        &page.candidates[0].key[..16],
        &[128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]
    );
    let author = db
        .select_retention_candidates(generation, Some(a.id()), clock(110), None, limit(10))
        .await?;
    assert_eq!(author.candidates.len(), 2);
    assert!(
        author
            .candidates
            .iter()
            .all(|candidate| candidate.author == a.id())
    );
    let first = db
        .select_retention_candidates(generation, None, clock(110), None, limit(1))
        .await?;
    let rest = db
        .select_retention_candidates(
            generation,
            None,
            clock(110),
            first.scanned_through,
            limit(10),
        )
        .await?;
    assert_eq!(rest.candidates.len(), 3);
    assert_eq!(rest.candidates[0], page.candidates[1]);
    assert!(db.rebuild_retention_index(limit(4097)).await.is_err());
    assert!(
        db.promote_retention_grace(clock(110), limit(4097))
            .await
            .is_err()
    );
    assert!(
        db.select_retention_candidates(generation, None, clock(110), None, limit(4097))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_index_interleaved_rebuild_policy_replacement_and_stale_advice()
-> anyhow::Result<()> {
    let holder = RostraIdSecretKey::generate().id();
    let db = Database::new_in_memory(holder).await?;
    let author = RostraIdSecretKey::generate();
    let first = post(author, 1);
    let late = post(author, 2);
    let deleted = post(author, 3);
    ingest(&db, &first, true).await?;
    ingest(&db, &deleted, true).await?;
    ingest(&db, &late, false).await?;
    let old = db.configure_retention_index(policy()).await?.generation;
    db.rebuild_retention_index(limit(1)).await?;
    ingest(&db, &late, true).await?;
    delete(&db, author, &deleted).await?;
    ready(&db).await?;
    assert!(db.get_payload_usage().await?.is_none());
    db.promote_retention_grace(clock(110), limit(10)).await?;
    let candidates = db
        .select_retention_candidates(old, None, clock(110), None, limit(10))
        .await?
        .candidates;
    assert_eq!(candidates.len(), 2);
    // Duplicate processing does not demote a promoted member or refresh grace.
    ingest(&db, &first, true).await?;
    assert_eq!(
        db.select_retention_candidates(old, None, clock(110), None, limit(10))
            .await?
            .candidates,
        candidates
    );
    let changed = RetentionPolicy::new(1, 3, 1, 65536, 8, 20).unwrap();
    let generation = db.configure_retention_index(changed).await?.generation;
    assert_ne!(old, generation);
    assert!(
        !db.retention_candidate_is_current(old, candidates[0], clock(200))
            .await?
    );
    db.rebuild_retention_index(limit(1)).await?;
    // Cleanup skips all lifecycle upserts until old ownership is exhausted.
    let newest = post(author, 4);
    ingest(&db, &newest, true).await?;
    delete(&db, author, &first).await?;
    assert!(!db.configure_retention_index(changed).await?.ready);
    ready(&db).await?;
    db.promote_retention_grace(clock(200), limit(10)).await?;
    assert!(
        !db.select_retention_candidates(old, None, clock(200), None, limit(10))
            .await?
            .ready
    );
    let page = db
        .select_retention_candidates(generation, None, clock(200), None, limit(10))
        .await?;
    assert_eq!(page.candidates.len(), 2);
    for event in [&late, &newest] {
        assert!(page.candidates.iter().any(|candidate| {
            candidate.key
                == changed
                    .key(
                        event.event_id(),
                        holder,
                        event.content_len(),
                        event.timestamp(),
                    )
                    .to_bytes()
        }));
    }
    let wrong_holder = RetentionGeneration::new(changed, RostraIdSecretKey::generate().id());
    assert!(
        !db.select_retention_candidates(wrong_holder, None, clock(200), None, limit(10))
            .await?
            .ready
    );
    assert!(
        !db.retention_candidate_is_current(wrong_holder, page.candidates[0], clock(200))
            .await?
    );
    accounting(&db).await?;
    let victim = page.candidates[0];
    let outcome = db
        .prune_quota_payload(QuotaPruneRequest {
            id: victim.event.to_short(),
            target: QuotaPruneTarget::Processed,
            reason: QuotaPruneReason::GlobalQuota,
            policy: changed,
            clock: clock(200),
        })
        .await?;
    assert!(matches!(outcome, QuotaPruneOutcome::Pruned { .. }));
    assert!(
        !db.retention_candidate_is_current(generation, victim, clock(200))
            .await?
    );
    assert_eq!(
        db.select_retention_candidates(generation, None, clock(200), None, limit(10))
            .await?
            .candidates
            .len(),
        1
    );
    ingest(
        &db,
        if victim.event == late.event_id() {
            &late
        } else {
            &newest
        },
        true,
    )
    .await?;
    assert_eq!(
        db.select_retention_candidates(generation, None, clock(200), None, limit(10))
            .await?
            .candidates
            .len(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_index_clocks_protection_and_bounded_filtered_visits() -> anyhow::Result<()> {
    let local = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(local.id()).await?;
    let remote = RostraIdSecretKey::generate();
    let retained = [post(remote, 1), post(remote, 2), post(remote, 3)];
    for event in &retained {
        ingest(&db, event, true).await?;
    }
    let local_post = post(local, 4);
    let legacy = post(remote, 5);
    let unknown_materialization = post(remote, 6);
    let future = post(remote, 7);
    let overflow = post(remote, 8);
    let missing = post(remote, 9);
    for event in [
        &local_post,
        &legacy,
        &unknown_materialization,
        &future,
        &overflow,
    ] {
        ingest(&db, event, true).await?;
    }
    ingest(&db, &missing, false).await?;
    db.write_with(|tx| {
        let mut origins = tx.open_table(&crate::events_retention_origins::TABLE)?;
        origins.remove(&legacy.event_id().to_short())?;
        for (event, header, first) in [
            (&unknown_materialization, 6, None),
            (&future, 500, Some(Timestamp::from(100))),
            (&overflow, 8, Some(Timestamp::from(u64::MAX))),
        ] {
            origins.insert(
                &event.event_id().to_short(),
                &crate::retention::RetentionOrigins {
                    effective_timestamp: Timestamp::from(header),
                    materialized_at: first,
                },
            )?;
        }
        Ok(())
    })
    .await?;
    let generation = db.configure_retention_index(policy()).await?.generation;
    ready(&db).await?;
    assert_eq!(
        db.promote_retention_grace(RetentionClock::Untrusted, limit(10))
            .await?
            .unwrap()
            .visited,
        0
    );
    assert_eq!(
        db.promote_retention_grace(clock(200), limit(10))
            .await?
            .unwrap()
            .visited,
        3
    );
    let page = db
        .select_retention_candidates(generation, None, clock(200), None, limit(10))
        .await?;
    assert_eq!(page.candidates.len(), 3);
    let untrusted = db
        .select_retention_candidates(generation, None, RetentionClock::Untrusted, None, limit(10))
        .await?;
    assert!(!untrusted.ready);
    assert_eq!(untrusted.visited, 0);
    let rollback = db
        .select_retention_candidates(generation, None, clock(109), None, limit(2))
        .await?;
    assert_eq!(rollback.visited, 2);
    assert!(rollback.candidates.is_empty());
    assert!(rollback.scanned_through.is_some());
    let rest = db
        .select_retention_candidates(
            generation,
            None,
            clock(99),
            rollback.scanned_through,
            limit(2),
        )
        .await?;
    assert_eq!(rest.visited, 1);
    assert!(rest.candidates.is_empty());
    let candidate = page.candidates[0];
    assert!(
        !db.retention_candidate_is_current(generation, candidate, clock(109))
            .await?
    );
    assert!(
        !db.retention_candidate_is_current(generation, candidate, RetentionClock::Untrusted)
            .await?
    );
    accounting(&db).await?;
    assert_eq!(
        db.prune_quota_payload(QuotaPruneRequest {
            id: candidate.event.to_short(),
            target: QuotaPruneTarget::Processed,
            reason: QuotaPruneReason::GlobalQuota,
            policy: policy(),
            clock: clock(109),
        })
        .await?,
        QuotaPruneOutcome::Ineligible
    );
    assert_eq!(
        db.promote_retention_grace(clock(500), limit(10))
            .await?
            .unwrap()
            .visited,
        1
    );
    assert_eq!(
        db.select_retention_candidates(generation, None, clock(499), None, limit(10))
            .await?
            .candidates
            .len(),
        3
    );
    assert!(
        db.read_with(|tx| Ok(tx
            .open_table(&crate::events_retention_origins::TABLE)?
            .get(&legacy.event_id().to_short())?
            .is_none()))
            .await?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_index_reopen_abort_and_total_replay() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("retention.db");
    let holder = RostraIdSecretKey::generate().id();
    let author = RostraIdSecretKey::generate();
    let events = [post(author, 1), post(author, 2), post(author, 3)];
    let generation;
    {
        let db = Database::open(&path, holder).await?;
        for event in &events {
            ingest(&db, event, true).await?;
        }
        generation = db.configure_retention_index(policy()).await?.generation;
        db.rebuild_retention_index(limit(1)).await?;
    }
    {
        let db = Database::open(&path, holder).await?;
        assert!(!db.retention_index_progress().await?.unwrap().ready);
        ready(&db).await?;
        db.promote_retention_grace(clock(200), limit(1)).await?;
    }
    {
        let db = Database::open(&path, holder).await?;
        db.promote_retention_grace(clock(200), limit(10)).await?;
        let before = db
            .select_retention_candidates(generation, None, clock(200), None, limit(10))
            .await?;
        assert_eq!(before.candidates.len(), 3);
        let abort: crate::DbResult<()> = db
            .write_with(|tx| {
                let snapshot = db.payload_before_tx(tx, &events[0].event)?;
                tx.open_table(&crate::events_retention_origins::TABLE)?
                    .remove(&events[0].event_id().to_short())?;
                db.payload_after_tx(tx, snapshot)?;
                crate::PayloadAccountingInvariantSnafu.fail()
            })
            .await;
        assert!(abort.is_err());
        assert_eq!(
            db.select_retention_candidates(generation, None, clock(200), None, limit(10))
                .await?,
            before
        );
        db.write_with(|tx| Database::prepare_total_migration(tx, 30))
            .await?;
        db.write_with(|tx| db.reprocess_migration_stash(tx)).await?;
        assert!(db.retention_index_progress().await?.is_none());
        assert!(
            !db.retention_candidate_is_current(generation, before.candidates[0], clock(200))
                .await?
        );
        assert!(db.get_payload_usage().await?.is_none());
        db.configure_retention_index(policy()).await?;
        ready(&db).await?;
        db.promote_retention_grace(clock(200), limit(10)).await?;
        assert_eq!(
            db.select_retention_candidates(generation, None, clock(200), None, limit(10))
                .await?,
            before
        );
        assert!(db.get_payload_usage().await?.is_none());
        db.configure_retention_index(RetentionPolicy::experimental())
            .await?;
        db.rebuild_retention_index(limit(1)).await?;
    }
    {
        let db = Database::open(&path, holder).await?;
        ready(&db).await?;
        assert_eq!(
            db.retention_index_progress()
                .await?
                .unwrap()
                .generation
                .policy_bytes(),
            RetentionPolicy::experimental().to_bytes()
        );
        assert!(
            !db.select_retention_candidates(generation, None, clock(200), None, limit(10))
                .await?
                .ready
        );
    }
    assert!(
        Database::open(&path, RostraIdSecretKey::generate().id())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_index_safe_kinds_and_negative_keys_during_live_updates() -> anyhow::Result<()> {
    let holder = RostraIdSecretKey::from_bytes([41; 32]).id();
    let remote = RostraIdSecretKey::from_bytes([42; 32]);
    let db = Database::new_in_memory(holder).await?;
    let policy = RetentionPolicy::new(1, 1000, 65536, 65536, 8, 10).unwrap();
    let generation = db.configure_retention_index(policy).await?.generation;
    ready(&db).await?;
    let mut posts: Vec<_> = (1..=12).map(|time| post(remote, time)).collect();
    // Deliberately insert descending short IDs, not event or score order.
    posts.sort_by_key(|event| std::cmp::Reverse(event.event_id().to_short()));
    for event in &posts {
        ingest(&db, event, true).await?;
    }
    let profile_bytes = SocialProfileUpdate {
        display_name: "protected".to_owned(),
        bio: String::new(),
        avatar: None,
    }
    .serialize_cbor()?;
    for kind in [EventKind::SOCIAL_PROFILE_UPDATE, EventKind::from(65535)] {
        let signed = Event::builder_raw_content()
            .author(remote.id())
            .kind(kind)
            .timestamp(Timestamp::from(50).to_offset_date_time().unwrap())
            .content(&profile_bytes)
            .build()
            .signed_by(remote);
        let content = VerifiedEventContent::assume_verified(
            VerifiedEvent::verify_signed(remote.id(), signed)?,
            profile_bytes.clone(),
        );
        ingest(&db, &content, true).await?;
        assert!(
            db.get_event_content_state(content.event_id())
                .await
                .is_none()
        );
        assert!(
            db.read_with(|tx| Ok(tx
                .open_table(&crate::content_retention_reverse::TABLE)?
                .get(&content.event_id().to_short())?
                .is_none()))
                .await?
        );
    }
    let empty = VerifiedEvent::verify_signed(
        remote.id(),
        Event::builder_raw_content()
            .author(remote.id())
            .kind(EventKind::SOCIAL_POST)
            .timestamp(Timestamp::from(51).to_offset_date_time().unwrap())
            .build()
            .signed_by(remote),
    )?;
    db.write_with(|tx| {
        db.process_event_tx(&empty, Timestamp::from(100), tx)
            .map(|_| ())
    })
    .await?;
    assert!(
        db.read_with(|tx| Ok(tx
            .open_table(&crate::content_retention_reverse::TABLE)?
            .get(&empty.event_id.to_short())?
            .is_none()))
            .await?
    );
    // Delete one waiting row and another after promotion; both own exact removals.
    delete(&db, remote, &posts[0]).await?;
    let mut promoted = 0;
    loop {
        let progress = db
            .promote_retention_grace(clock(110), limit(2))
            .await?
            .unwrap();
        assert!(progress.visited <= 2);
        promoted += progress.visited;
        if progress.visited == 0 {
            break;
        }
    }
    assert_eq!(promoted, posts.len() - 1);
    delete(&db, remote, &posts[1]).await?;
    let mut expected: Vec<_> = posts[2..]
        .iter()
        .map(|event| {
            policy
                .key(
                    event.event_id(),
                    holder,
                    event.content_len(),
                    event.timestamp(),
                )
                .to_bytes()
        })
        .collect();
    expected.sort();
    assert!(
        expected.iter().all(|key| key[0] < 128),
        "fixture must cover negative score encoding"
    );
    let mut keys = Vec::new();
    let mut after = None;
    loop {
        let page = db
            .select_retention_candidates(generation, Some(remote.id()), clock(110), after, limit(2))
            .await?;
        assert!(page.visited <= 2);
        keys.extend(page.candidates.iter().map(|candidate| candidate.key));
        after = page.scanned_through;
        if page.visited == 0 {
            break;
        }
    }
    assert_eq!(keys, expected);
    assert!(db.get_payload_usage().await?.is_none());
    Ok(())
}
