use std::collections::BTreeMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

use rostra_core::Timestamp;
use rostra_core::event::content_kind::{EventContentKind as _, SocialPost};
use rostra_core::event::{Event, EventExt as _, EventKind, VerifiedEvent, VerifiedEventContent};
use rostra_core::id::{RostraIdSecretKey, ToShort as _};
use rostra_core::retention::RetentionPolicy;

use crate::payload_runtime::{PayloadRuntime, RuntimeCursor, RuntimeLimits, RuntimeTurn};
use crate::{
    Database, PayloadAdmissionConfig, PayloadAdmissionLimits, PayloadIngestOutcome,
    PayloadReservationOutcome,
};

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

async fn install(
    db: &mut Database,
    global: u64,
    author: u64,
    operations: usize,
    bytes: u64,
) -> anyhow::Result<()> {
    let config = PayloadAdmissionConfig::new(PayloadAdmissionLimits {
        database_bytes: NonZeroU64::new(global).unwrap(),
        author_bytes: NonZeroU64::new(author).unwrap(),
        overrides: BTreeMap::new(),
        in_flight_count: NonZeroUsize::new(8).unwrap(),
        in_flight_bytes: NonZeroU64::new(100_000).unwrap(),
    })
    .unwrap();
    let policy = RetentionPolicy::new(1, 1, 0, 0, 1, 0).unwrap();
    let progress = db.configure_retention_index(policy).await?;
    db.payload_runtime = Some(
        PayloadRuntime::new(
            progress.generation,
            config.identity(),
            RuntimeLimits {
                operations: NonZeroUsize::new(operations).unwrap(),
                bytes,
                time: Duration::from_secs(1),
            },
        )
        .unwrap(),
    );
    db.payload_admission.state.lock().unwrap().config = Some(config);
    Ok(())
}

async fn settle(db: &Database) -> anyhow::Result<RuntimeCursor> {
    let runtime = db.payload_runtime.as_ref().unwrap();
    let mut cursor = RuntimeCursor::default();
    for _ in 0..1000 {
        if runtime.turn(db, &mut cursor).await? == RuntimeTurn::Wait {
            return Ok(cursor);
        }
    }
    panic!("bounded fixture maintenance failed to settle");
}

async fn prepare_with_worker(
    db: &Database,
    event: &VerifiedEvent,
) -> anyhow::Result<PayloadReservationOutcome> {
    let runtime = db.payload_runtime.as_ref().unwrap();
    Ok(tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = db.prepare_payload_acquisition(event) => result,
            result = runtime.run(db) => panic!("worker stopped: {result:?}"),
        }
    })
    .await??)
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_cap_minus_one_prepares_preempts_reserves_and_ingests() -> anyhow::Result<()> {
    let mut db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let author = RostraIdSecretKey::generate();
    let old = post(author, 1, "old");
    let incoming = post(author, 2, "new");
    db.try_process_event_with_content(&old).await?;
    let bytes = u64::from(old.content_len());
    install(&mut db, bytes + 1, 10000, 5, bytes).await?;
    assert!(db.get_payload_usage().await?.is_none());

    let PayloadReservationOutcome::Reserved(reservation) =
        prepare_with_worker(&db, &incoming.event).await?
    else {
        panic!("incoming acquisition did not reserve");
    };
    assert!(
        db.get_event_content(old.event_id().to_short())
            .await
            .is_none()
    );
    let usage = db.payload_admission_usage();
    assert_eq!(usage.logical_reserved_bytes, bytes);
    assert_eq!(usage.pending_demands, 0);
    assert_eq!(usage.buffers, 0);
    let buffer = reservation.try_acquire_buffer()?;
    assert_eq!(
        db.try_process_admitted_event_content(&incoming, Some(&buffer))
            .await?,
        PayloadIngestOutcome::Processed,
    );
    drop(buffer);
    drop(reservation);
    assert_eq!(db.payload_admission_usage(), Default::default());
    let usage = db.get_payload_usage().await?.unwrap();
    assert_eq!(usage.logical_current_bytes, bytes);
    // This slice does not collect: logical reduction is not physical removal.
    assert_eq!(usage.unique_stored_bytes, bytes * 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_independent_maintenance_finishes_with_single_operation_turns() -> anyhow::Result<()>
{
    let mut db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    for time in 1..5 {
        db.try_process_event_with_content(&post(RostraIdSecretKey::generate(), time, "same"))
            .await?;
    }
    install(&mut db, 10000, 10000, 1, 10000).await?;
    let mut cursor = settle(&db).await?;
    assert!(db.get_payload_usage().await?.is_some());
    assert!(db.retention_index_progress().await?.unwrap().ready);
    for turn in 0..20 {
        assert_eq!(
            db.payload_runtime
                .as_ref()
                .unwrap()
                .turn(&db, &mut cursor)
                .await?,
            if turn % 5 == 4 {
                RuntimeTurn::Wait
            } else {
                RuntimeTurn::Continue
            },
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_paused_prepare_deduplicates_and_cancels_without_buffers() -> anyhow::Result<()> {
    let mut db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let author = RostraIdSecretKey::generate();
    let old = post(author, 1, "old");
    let incoming = post(author, 2, "new");
    db.try_process_event_with_content(&old).await?;
    install(&mut db, u64::from(old.content_len()) + 1, 10000, 5, 10000).await?;
    let mut cursor = settle(&db).await?;
    {
        let first = db.prepare_payload_acquisition(&incoming.event);
        let second = db.prepare_payload_acquisition(&incoming.event);
        tokio::pin!(first, second);
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        let usage = db.payload_admission_usage();
        assert_eq!(usage.pending_demands, 1);
        assert_eq!(
            usage.pending_demand_bytes,
            u64::from(incoming.content_len())
        );
        assert_eq!(usage.buffers, 0);
        assert_eq!(usage.buffer_bytes, 0);
        assert_eq!(usage.logical_reserved_bytes, 0);
    }
    assert_eq!(db.payload_admission_usage().pending_demands, 0);
    assert_eq!(
        db.payload_runtime
            .as_ref()
            .unwrap()
            .turn(&db, &mut cursor)
            .await?,
        RuntimeTurn::Wait,
    );
    assert!(
        db.get_event_content(old.event_id().to_short())
            .await
            .is_some()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_expired_prepare_does_not_renew_or_prune() -> anyhow::Result<()> {
    let mut db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let author = RostraIdSecretKey::generate();
    let old = post(author, 1, "old");
    let incoming = post(author, 2, "new");
    db.try_process_event_with_content(&old).await?;
    install(&mut db, u64::from(old.content_len()) + 1, 10000, 5, 10000).await?;
    settle(&db).await?;
    let prepare = db.prepare_payload_acquisition(&incoming.event);
    tokio::pin!(prepare);
    assert!(futures::poll!(&mut prepare).is_pending());
    db.payload_admission
        .demands
        .lock()
        .unwrap()
        .entries
        .get_mut(&incoming.event_id())
        .unwrap()
        .expires = Timestamp::now();
    let result = tokio::time::timeout(Duration::from_secs(2), prepare).await??;
    assert!(matches!(result, PayloadReservationOutcome::Deferred(_)));
    assert_eq!(db.payload_admission_usage().pending_demands, 0);
    assert!(
        db.get_event_content(old.event_id().to_short())
            .await
            .is_some()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_shared_store_reuse_waits_without_retaining_a_copy() -> anyhow::Result<()> {
    let mut db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let author = RostraIdSecretKey::generate();
    let old = post(author, 1, "same");
    let incoming = post(author, 2, "same");
    db.try_process_event_with_content(&old).await?;
    let bytes = u64::from(old.content_len());
    install(&mut db, bytes + 1, 10000, 5, bytes).await?;
    settle(&db).await?;
    {
        let prepare = db.prepare_payload_acquisition(&incoming.event);
        tokio::pin!(prepare);
        assert!(futures::poll!(&mut prepare).is_pending());
        assert_eq!(db.payload_admission_usage().pending_demands, 1);
        assert_eq!(db.payload_admission_usage().buffers, 0);
    }
    assert!(matches!(
        prepare_with_worker(&db, &incoming.event).await?,
        PayloadReservationOutcome::Unneeded,
    ));
    assert!(
        db.get_event_content(incoming.event_id().to_short())
            .await
            .is_some()
    );
    assert_eq!(
        db.get_payload_usage().await?.unwrap().unique_stored_bytes,
        bytes
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_alternate_author_acquisition_passes_an_exhausted_higher_rank() -> anyhow::Result<()>
{
    let mut db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let b = RostraIdSecretKey::generate();
    let high_old = post(a, 95, "old");
    let high = post(a, 90, "new");
    let low_old = post(b, 1, "old");
    let low = post(b, 50, "new");
    db.try_process_event_with_content(&high_old).await?;
    db.try_process_event_with_content(&low_old).await?;
    let bytes = u64::from(low.content_len());
    install(&mut db, 10000, bytes + 1, 1, bytes).await?;
    settle(&db).await?;
    let high_prepare = db.prepare_payload_acquisition(&high.event);
    tokio::pin!(high_prepare);
    assert!(futures::poll!(&mut high_prepare).is_pending());
    assert!(matches!(
        prepare_with_worker(&db, &low.event).await?,
        PayloadReservationOutcome::Reserved(_),
    ));
    assert!(
        db.get_event_content(high_old.event_id().to_short())
            .await
            .is_some()
    );
    assert!(
        db.get_event_content(low_old.event_id().to_short())
            .await
            .is_none()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_byte_blockage_sleeps_and_runner_cancellation_releases_exclusivity()
-> anyhow::Result<()> {
    let mut db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let author = RostraIdSecretKey::generate();
    let old = post(author, 1, "old");
    let incoming = post(author, 2, "new");
    db.try_process_event_with_content(&old).await?;
    install(&mut db, u64::from(old.content_len()) + 1, 10000, 5, 1).await?;
    let mut cursor = settle(&db).await?;
    let prepare = db.prepare_payload_acquisition(&incoming.event);
    tokio::pin!(prepare);
    assert!(futures::poll!(&mut prepare).is_pending());
    let runtime = db.payload_runtime.as_ref().unwrap();
    for _ in 0..10 {
        assert_eq!(runtime.turn(&db, &mut cursor).await?, RuntimeTurn::Wait);
    }
    for _ in 0..2 {
        let run = runtime.run(&db);
        tokio::pin!(run);
        assert!(futures::poll!(&mut run).is_pending());
        assert!(runtime.run(&db).await.is_err());
        // Even repeated coalesced wakes cannot bypass minimum spacing.
        for _ in 0..10 {
            db.payload_admission.changed.notify_waiters();
            assert!(futures::poll!(&mut run).is_pending());
        }
    }
    assert!(
        db.get_event_content(old.event_id().to_short())
            .await
            .is_some()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_acquisition_crosses_a_previously_promoted_future_prefix() -> anyhow::Result<()> {
    let mut db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let author = RostraIdSecretKey::generate();
    let future = Timestamp::now().saturating_add_secs(3600);
    let mut protected = Vec::new();
    for time in 1..=12 {
        let event = post(author, time, "same");
        db.write_with(|tx| {
            db.process_event_tx(&event.event, Timestamp::now(), tx)?;
            db.process_event_content_tx(&event, future, tx)?;
            Ok(())
        })
        .await?;
        protected.push(event);
    }
    let old = post(author, 20, "same");
    let incoming = post(author, 50, "next");
    db.try_process_event_with_content(&old).await?;
    let bytes = u64::from(old.content_len());
    install(&mut db, 13 * bytes + 1, 10000, 1, bytes).await?;
    let one = NonZeroUsize::new(1).unwrap();
    while !db.rebuild_retention_index(one).await?.unwrap().ready {}
    while db
        .promote_retention_grace(crate::RetentionClock::Trusted(future), one)
        .await?
        .unwrap()
        .visited
        != 0
    {}
    // Model a backwards clock after promotion without demoting the index.
    // Fresh candidate checks must skip all twelve future materializations,
    // retaining bounded continuation instead of retrying the first row.
    assert!(matches!(
        prepare_with_worker(&db, &incoming.event).await?,
        PayloadReservationOutcome::Reserved(_),
    ));
    assert!(
        db.get_event_content(old.event_id().to_short())
            .await
            .is_none()
    );
    for event in protected {
        assert!(
            db.get_event_content(event.event_id().to_short())
                .await
                .is_some()
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_joined_abort_releases_the_last_database_owner() -> anyhow::Result<()> {
    let mut db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    install(&mut db, 10000, 10000, 5, 10000).await?;
    settle(&db).await?;
    let db = std::sync::Arc::new(db);
    let weak = std::sync::Arc::downgrade(&db);
    let (started, running) = tokio::sync::oneshot::channel();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        let run = db.payload_runtime.as_ref().unwrap().run(&db);
        tokio::pin!(run);
        assert!(futures::poll!(&mut run).is_pending());
        started.send(()).unwrap();
        run.await
    });
    running.await?;
    assert!(weak.upgrade().is_some());
    tasks.abort_all();
    assert!(tasks.join_next().await.unwrap().unwrap_err().is_cancelled());
    assert!(weak.upgrade().is_none());
    Ok(())
}
