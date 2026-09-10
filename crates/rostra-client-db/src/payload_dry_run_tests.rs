use std::collections::BTreeMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

use redb::TableHandle as _;
use rostra_core::Timestamp;
use rostra_core::event::content_kind::{EventContentKind as _, SocialPost};
use rostra_core::event::{Event, EventExt as _, EventKind, VerifiedEvent, VerifiedEventContent};
use rostra_core::id::{RostraId, RostraIdSecretKey, ToShort as _};
use rostra_core::retention::RetentionPolicy;

use super::payload_dry_run::{DryRun, DryRunLimits, DryRunStatus};
use crate::payload_runtime::{PayloadRuntime, RuntimeCursor, RuntimeTurn};
use crate::{
    Database, PayloadAdmissionConfig, PayloadAdmissionLimits, PayloadReservationOutcome,
    QuotaPruneReason, RetentionGeneration,
};

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_long_attempt_serializes_and_rechecks_config_before_publication()
-> anyhow::Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let holder = RostraIdSecretKey::generate().id();
    let db = Arc::new(Database::new_in_memory(holder).await?);
    ready(&db).await?;
    let dry = Arc::new(DryRun::new(config(1, 1), limits()).unwrap());
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let worker_db = db.clone();
    let worker_dry = dry.clone();
    let task = tokio::spawn(async move {
        worker_dry
            .turn_with_clock(&worker_db, generation(holder, 0), move || {
                entered.send(()).unwrap();
                released.recv().unwrap();
                Timestamp::now()
            })
            .await
    });
    entering.await?;
    // Exceed the pacing period and cooperative deadline while one indivisible
    // operation holds ownership. A second direct turn must not start scanning.
    tokio::time::sleep(Duration::from_millis(1010)).await;
    let overlapped = AtomicBool::new(false);
    dry.turn_with_clock(&db, generation(holder, 0), || {
        overlapped.store(true, Ordering::Relaxed);
        Timestamp::now()
    })
    .await?;
    assert!(!overlapped.load(Ordering::Relaxed));
    assert!(dry.report().is_none());
    db.payload_admission.state.lock().unwrap().config = Some(config(1, 1));
    release.send(())?;
    assert!(task.await?.is_err());
    assert!(dry.report().is_none());
    db.payload_admission.state.lock().unwrap().config = None;
    // In-progress ownership was released on the failed publication.
    dry.turn(&db, generation(holder, 0)).await?;
    assert_eq!(dry.report().unwrap().status, DryRunStatus::Complete);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_attempt_abort_and_panic_release_ownership() -> anyhow::Result<()> {
    use std::sync::Arc;

    use futures::FutureExt as _;

    let holder = RostraIdSecretKey::generate().id();
    let db = Arc::new(Database::new_in_memory(holder).await?);
    ready(&db).await?;
    let dry = Arc::new(DryRun::new(config(1, 1), limits()).unwrap());
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let worker_db = db.clone();
    let worker_dry = dry.clone();
    let task = tokio::spawn(async move {
        worker_dry
            .turn_with_clock(&worker_db, generation(holder, 0), move || {
                entered.send(()).unwrap();
                released.recv().unwrap();
                Timestamp::now()
            })
            .await
    });
    entering.await?;
    task.abort();
    assert!(
        !task.is_finished(),
        "an indivisible read must finish before join"
    );
    release.send(())?;
    // Abort cannot interrupt block_in_place; either its completed read or a
    // cancelled join is valid, but only the actual join releases task ownership.
    let _ = task.await;
    tokio::time::sleep(Duration::from_millis(1010)).await;
    assert!(
        std::panic::AssertUnwindSafe(
            dry.turn_with_clock(&db, generation(holder, 0), || panic!("fixture panic"))
        )
        .catch_unwind()
        .await
        .is_err()
    );
    assert!(dry.report().is_none());
    tokio::time::sleep(Duration::from_millis(1010)).await;
    dry.turn(&db, generation(holder, 0)).await?;
    assert_eq!(dry.report().unwrap().status, DryRunStatus::Complete);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_supported_projection_matches_disposable_enforce() -> anyhow::Result<()> {
    let local = RostraIdSecretKey::generate();
    let mut db = Database::new_in_memory(local.id()).await?;
    let a = RostraIdSecretKey::generate();
    let b = RostraIdSecretKey::generate();
    let events = [
        post(a, 1, "same"),
        post(a, 2, "same"),
        post(b, 3, "same"),
        post(b, 4, "same"),
        post(local, 5, "same"),
    ];
    for event in &events {
        db.try_process_event_with_content(event).await?;
    }
    ready(&db).await?;
    let len = u64::from(events[0].event.content_len());
    let config = config_overrides(
        2 * len,
        10 * len,
        BTreeMap::from([(a.id(), NonZeroU64::new(len).unwrap())]),
    );
    let dry = DryRun::new(config.clone(), limits()).unwrap();
    let projection = dry
        .snapshot(&db, generation(local.id(), 0), Timestamp::now)
        .await?
        .projection
        .unwrap();
    let policy = RetentionPolicy::new(1, 1, 0, 0, 1, 0).unwrap();
    let progress = db.configure_retention_index(policy).await?;
    db.payload_runtime = Some(
        PayloadRuntime::new(
            progress.generation,
            config.identity(),
            crate::payload_runtime::RuntimeLimits {
                operations: NonZeroUsize::new(64).unwrap(),
                bytes: 100_000,
                gc_bytes: 100_000,
                time: Duration::from_secs(1),
            },
        )
        .unwrap(),
    );
    db.payload_admission.state.lock().unwrap().config = Some(config);
    let mut cursor = RuntimeCursor::default();
    let mut finished = false;
    // Enforce deliberately waits on revision invalidation between reductions;
    // a single Wait is not a claim that all pressure has been resolved.
    for _ in 0..100 {
        if db
            .payload_runtime
            .as_ref()
            .unwrap()
            .turn(&db, &mut cursor)
            .await?
            == RuntimeTurn::Wait
        {
            finished = true;
        }
    }
    assert!(finished);
    assert_eq!(
        db.get_payload_usage().await?.unwrap().logical_current_bytes,
        projection.logical_remaining_bytes
    );
    for event in &events {
        let decision = db
            .read_with(|tx| {
                Ok(tx
                    .open_table(&crate::events_quota_pruned::TABLE)?
                    .get(&event.event.event_id.to_short())?
                    .map(|row| row.value_try())
                    .transpose()?)
            })
            .await?;
        let projected = projection
            .victims
            .iter()
            .find(|(id, _)| *id == event.event.event_id)
            .map(|(_, reason)| *reason);
        assert_eq!(decision.map(|decision| decision.reason), projected);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_runner_cancellation_pacing_and_incomplete_replacement() -> anyhow::Result<()> {
    let holder = RostraIdSecretKey::generate().id();
    let mut db = Database::new_in_memory(holder).await?;
    let author = RostraIdSecretKey::generate();
    db.try_process_event_with_content(&post(author, 1, "first"))
        .await?;
    ready(&db).await?;
    db.payload_runtime = Some(PayloadRuntime::new_dry_run(
        &db,
        generation(holder, 0),
        config(1, 1),
        DryRunLimits {
            events: NonZeroUsize::new(1).unwrap(),
            authors: NonZeroUsize::new(1).unwrap(),
            ..limits()
        },
    )?);
    let before = durable(&db).await?;
    let runtime = db.payload_runtime.as_ref().unwrap();
    for _ in 0..2 {
        let run = runtime.run(&db);
        tokio::pin!(run);
        assert!(futures::poll!(&mut run).is_pending());
        assert!(runtime.run(&db).await.is_err());
        let report = runtime.dry_run_report();
        assert_eq!(report.as_ref().unwrap().status, DryRunStatus::Complete);
        for _ in 0..100 {
            db.payload_admission.changed.notify_waiters();
            assert!(futures::poll!(&mut run).is_pending());
            assert_eq!(runtime.dry_run_report(), report);
        }
    }
    assert_eq!(durable(&db).await?, before);
    db.try_process_event_with_content(&post(author, 2, "next"))
        .await?;
    tokio::time::sleep(Duration::from_millis(1010)).await;
    let mut cursor = RuntimeCursor::default();
    runtime.turn(&db, &mut cursor).await?;
    let incomplete = runtime.dry_run_report().unwrap();
    assert_eq!(incomplete.status, DryRunStatus::EventLimit);
    assert!(incomplete.projection.is_none());
    db.payload_admission.state.lock().unwrap().config = Some(config(1, 1));
    assert!(runtime.turn(&db, &mut cursor).await.is_err());
    assert!(runtime.dry_run_report().is_none());
    Ok(())
}

fn limits() -> DryRunLimits {
    DryRunLimits {
        events: NonZeroUsize::new(64).unwrap(),
        authors: NonZeroUsize::new(16).unwrap(),
        logical_bytes: 100_000,
        time: Duration::from_secs(1),
    }
}

fn config(global: u64, author: u64) -> PayloadAdmissionConfig {
    config_overrides(global, author, BTreeMap::new())
}

fn config_overrides(
    global: u64,
    author: u64,
    overrides: BTreeMap<RostraId, NonZeroU64>,
) -> PayloadAdmissionConfig {
    PayloadAdmissionConfig::new(PayloadAdmissionLimits {
        database_bytes: NonZeroU64::new(global).unwrap(),
        author_bytes: NonZeroU64::new(author).unwrap(),
        overrides,
        in_flight_count: NonZeroUsize::new(8).unwrap(),
        in_flight_bytes: NonZeroU64::new(100_000).unwrap(),
    })
    .unwrap()
}

fn generation(holder: RostraId, grace: u32) -> RetentionGeneration {
    RetentionGeneration::new(RetentionPolicy::new(1, 1, 0, 0, 1, grace).unwrap(), holder)
}

fn post(author: RostraIdSecretKey, timestamp: u64, text: &str) -> VerifiedEventContent {
    let content = SocialPost::new_text(text.to_owned(), None, Default::default())
        .serialize_cbor()
        .unwrap();
    let signed = Event::builder_raw_content()
        .author(author.id())
        .kind(EventKind::SOCIAL_POST)
        .timestamp(Timestamp::from(timestamp).to_offset_date_time().unwrap())
        .content(&content)
        .build()
        .signed_by(author);
    VerifiedEventContent::assume_verified(
        VerifiedEvent::verify_signed(author.id(), signed).unwrap(),
        content,
    )
}

async fn ready(db: &Database) -> anyhow::Result<()> {
    for _ in 0..100 {
        if db
            .rebuild_payload_accounting(NonZeroUsize::new(64).unwrap())
            .await?
            .ready
        {
            return Ok(());
        }
    }
    anyhow::bail!("fixture accounting did not settle")
}

/// Compare every durable table, including canonical source, projection, index,
/// GC queue, lifecycle, RC, counters, headers and content bytes.
async fn durable(db: &Database) -> anyhow::Result<BTreeMap<String, Vec<(Vec<u8>, Vec<u8>)>>> {
    db.read_with(|tx| {
        let mut all = BTreeMap::new();
        for table in tx.as_raw().list_tables()? {
            let name = table.name().to_owned();
            let definition = redb::TableDefinition::<&[u8], &[u8]>::new(&name);
            let table = tx.as_raw().open_table(definition)?;
            let mut rows = Vec::new();
            for row in table.range::<&[u8]>(..)? {
                let (key, value) = row?;
                rows.push((key.value().to_vec(), value.value().to_vec()));
            }
            all.insert(name, rows);
        }
        Ok(all)
    })
    .await
    .map_err(Into::into)
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_whole_snapshot_is_nonmutating_and_never_fresh_work() -> anyhow::Result<()> {
    let holder = RostraIdSecretKey::generate().id();
    let mut db = Database::new_in_memory(holder).await?;
    let author = RostraIdSecretKey::generate();
    let first = post(author, 1, "shared");
    let second = post(author, 2, "shared");
    let missing = post(author, 3, "missing");
    db.try_process_event_with_content(&first).await?;
    db.try_process_event_with_content(&second).await?;
    db.try_process_event(&missing.event).await?;
    ready(&db).await?;
    // A real quota-owned collectible hash ensures no-GC is not vacuous.
    let garbage = post(author, 4, "collectible");
    db.try_process_event_with_content(&garbage).await?;
    db.prune_quota_payload(crate::QuotaPruneRequest {
        id: garbage.event.event_id.to_short(),
        target: crate::QuotaPruneTarget::Processed,
        reason: QuotaPruneReason::GlobalQuota,
        policy: RetentionPolicy::new(1, 1, 0, 0, 1, 0).unwrap(),
        clock: crate::RetentionClock::Trusted(Timestamp::now()),
    })
    .await?;
    let before = durable(&db).await?;
    let admission = db.payload_admission_usage();
    let dry = DryRun::new(config(1, 1), limits()).unwrap();
    let now = Timestamp::now();
    let expected = dry.snapshot(&db, generation(holder, 0), || now).await?;
    assert_eq!(expected.status, DryRunStatus::Complete);
    let projection = expected.projection.as_ref().unwrap();
    assert_eq!(projection.victims.len(), 2);
    assert_eq!(projection.victim_details.len(), projection.victims.len());
    for (detail, (event, _)) in projection.victim_details.iter().zip(&projection.victims) {
        assert_eq!(detail.event, *event);
        assert_eq!(detail.author, author.id());
        assert_eq!(detail.bytes, u64::from(first.event.content_len()));
        assert!(detail.age_seconds > 0);
        assert_eq!(detail.distance_credit_ticks, 0);
    }
    assert_eq!(
        projection.logical_victim_bytes,
        u64::from(first.event.content_len()) * 2
    );
    assert!(expected.observed_usage.unwrap().unique_stored_bytes > 0);
    for _ in 0..20 {
        assert_eq!(
            dry.snapshot(&db, generation(holder, 0), || now).await?,
            expected
        );
    }
    db.payload_runtime = Some(PayloadRuntime::new_dry_run(
        &db,
        generation(holder, 0),
        config(1, 1),
        limits(),
    )?);
    let runtime = db.payload_runtime.as_ref().unwrap();
    let mut cursor = RuntimeCursor::default();
    assert_eq!(runtime.turn(&db, &mut cursor).await?, RuntimeTurn::Wait);
    let first_report = runtime.dry_run_report();
    for _ in 0..100 {
        assert_eq!(runtime.turn(&db, &mut cursor).await?, RuntimeTurn::Wait);
        assert_eq!(runtime.dry_run_report(), first_report);
    }
    assert_eq!(durable(&db).await?, before);
    assert_eq!(db.payload_admission_usage(), admission);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_bounds_discard_prefix_projection_and_do_not_maintain() -> anyhow::Result<()> {
    let holder = RostraIdSecretKey::generate().id();
    let db = Database::new_in_memory(holder).await?;
    let first = post(RostraIdSecretKey::generate(), 1, "first");
    let second = post(RostraIdSecretKey::generate(), 2, "second");
    db.try_process_event_with_content(&first).await?;
    db.try_process_event_with_content(&second).await?;
    let before = durable(&db).await?;
    let dry = DryRun::new(config(1, 1), limits()).unwrap();
    let report = dry
        .snapshot(&db, generation(holder, 0), Timestamp::now)
        .await?;
    assert_eq!(report.status, DryRunStatus::AccountingNotReady);
    assert!(report.projection.is_none());
    assert_eq!(durable(&db).await?, before);
    ready(&db).await?;
    let before = durable(&db).await?;
    for (bounds, status) in [
        (
            DryRunLimits {
                events: NonZeroUsize::new(1).unwrap(),
                authors: NonZeroUsize::new(1).unwrap(),
                ..limits()
            },
            DryRunStatus::EventLimit,
        ),
        (
            DryRunLimits {
                authors: NonZeroUsize::new(1).unwrap(),
                ..limits()
            },
            DryRunStatus::AuthorLimit,
        ),
        (
            DryRunLimits {
                logical_bytes: 1,
                ..limits()
            },
            DryRunStatus::LogicalByteLimit,
        ),
        (
            DryRunLimits {
                time: Duration::from_nanos(1),
                ..limits()
            },
            DryRunStatus::TimeLimit,
        ),
    ] {
        let dry = DryRun::new(config(1, 1), bounds).unwrap();
        for _ in 0..3 {
            let report = dry
                .snapshot(&db, generation(holder, 0), Timestamp::now)
                .await?;
            assert_eq!(report.status, status);
            assert!(report.projection.is_none());
            assert!(report.visited <= bounds.events.get());
        }
    }
    assert_eq!(durable(&db).await?, before);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_author_override_global_shared_and_protected_targets() -> anyhow::Result<()> {
    let local = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(local.id()).await?;
    let a = RostraIdSecretKey::generate();
    let b = RostraIdSecretKey::generate();
    let events = [
        post(a, 1, "same"),
        post(a, 2, "same"),
        post(b, 3, "same"),
        post(b, 4, "same"),
        post(local, 5, "same"),
    ];
    for event in &events {
        db.try_process_event_with_content(event).await?;
    }
    ready(&db).await?;
    let len = u64::from(events[0].event.content_len());
    let dry = DryRun::new(
        config_overrides(
            2 * len,
            10 * len,
            BTreeMap::from([(a.id(), NonZeroU64::new(len).unwrap())]),
        ),
        limits(),
    )
    .unwrap();
    let report = dry
        .snapshot(&db, generation(local.id(), 0), Timestamp::now)
        .await?;
    assert_eq!(report.database_high_water, 2 * len);
    let projection = report.projection.unwrap();
    assert_eq!(
        projection.author_high_waters,
        BTreeMap::from([(a.id(), len), (b.id(), 10 * len), (local.id(), 10 * len)])
    );
    assert_eq!(
        projection.victims,
        vec![
            (events[0].event.event_id, QuotaPruneReason::AuthorQuota),
            (events[1].event.event_id, QuotaPruneReason::AuthorQuota),
            (events[2].event.event_id, QuotaPruneReason::GlobalQuota),
            (events[3].event.event_id, QuotaPruneReason::GlobalQuota),
        ]
    );
    assert_eq!(projection.logical_remaining_bytes, len);
    assert_eq!(report.observed_usage.unwrap().unique_stored_bytes, len);
    assert!(!projection.unmet_global);
    let protected = DryRun::new(config(1, 1), limits())
        .unwrap()
        .snapshot(&db, generation(local.id(), 0), Timestamp::now)
        .await?
        .projection
        .unwrap();
    assert!(protected.unmet_global);
    assert_eq!(protected.unmet_authors, 1);
    assert_eq!(protected.logical_remaining_bytes, len);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_new_source_clock_and_config_replace_not_accumulate() -> anyhow::Result<()> {
    let holder = RostraIdSecretKey::generate().id();
    let db = Database::new_in_memory(holder).await?;
    let author = RostraIdSecretKey::generate();
    let event = post(author, 1, "first");
    db.try_process_event_with_content(&event).await?;
    ready(&db).await?;
    let now = Timestamp::now();
    let dry = DryRun::new(config(1, 1), limits()).unwrap();
    let young = dry.snapshot(&db, generation(holder, 100), || now).await?;
    assert!(young.projection.unwrap().victims.is_empty());
    let later = Timestamp::from(now.as_u64() + 101);
    let old = dry.snapshot(&db, generation(holder, 100), || later).await?;
    assert_eq!(old.projection.unwrap().victims.len(), 1);
    assert!(
        dry.snapshot(&db, generation(holder, 100), || Timestamp::from(0))
            .await?
            .projection
            .unwrap()
            .victims
            .is_empty()
    );
    let next = post(author, 2, "next");
    db.try_process_event_with_content(&next).await?;
    assert_eq!(
        dry.snapshot(&db, generation(holder, 0), || later)
            .await?
            .projection
            .unwrap()
            .victims
            .len(),
        2
    );
    let changed = DryRun::new(config(100_000, 100_000), limits()).unwrap();
    assert!(
        changed
            .snapshot(&db, generation(holder, 0), || later)
            .await?
            .projection
            .unwrap()
            .victims
            .is_empty()
    );
    db.write_with(|tx| {
        tx.open_table(&crate::events_retention_origins::TABLE)?
            .remove(&event.event.event_id.to_short())?;
        Ok(())
    })
    .await?;
    let unknown = dry.snapshot(&db, generation(holder, 0), || later).await?;
    assert_eq!(
        unknown.projection.unwrap().victims,
        vec![(next.event.event_id, QuotaPruneReason::AuthorQuota)]
    );
    db.payload_admission.state.lock().unwrap().config = Some(config(1, 1));
    assert!(
        dry.snapshot(&db, generation(holder, 0), || later)
            .await
            .is_err()
    );
    assert!(
        PayloadRuntime::new_dry_run(&db, generation(holder, 0), config(1, 1), limits()).is_err()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_real_prepare_and_ingest_remain_disabled_above_forecast_caps() -> anyhow::Result<()>
{
    let local = RostraIdSecretKey::generate();
    let mut db = Database::new_in_memory(local.id()).await?;
    db.payload_runtime = Some(PayloadRuntime::new_dry_run(
        &db,
        generation(local.id(), 0),
        config(1, 1),
        limits(),
    )?);
    let author = RostraIdSecretKey::generate();
    let first = post(author, 1, "shared");
    assert!(matches!(
        db.prepare_payload_acquisition(&first.event).await?,
        PayloadReservationOutcome::Disabled
    ));
    db.try_process_event_with_content(&first).await?;
    let shared = post(author, 2, "shared");
    assert!(matches!(
        db.prepare_payload_acquisition(&shared.event).await?,
        PayloadReservationOutcome::Unneeded
    ));
    let own = post(local, 3, "local");
    db.try_process_event_with_content(&own).await?;
    assert_eq!(db.payload_admission_usage(), Default::default());
    assert!(db.payload_admission.state.lock().unwrap().config.is_none());
    ready(&db).await?;
    assert!(db.get_payload_usage().await?.unwrap().logical_current_bytes > 1);
    Ok(())
}
