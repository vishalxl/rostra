use std::collections::BTreeMap;
use std::num::{NonZeroU64, NonZeroUsize};

use rostra_core::Timestamp;
use rostra_core::event::content_kind::{EventContentKind as _, SocialPost};
use rostra_core::event::{Event, EventExt as _, EventKind, VerifiedEvent, VerifiedEventContent};
use rostra_core::id::{RostraIdSecretKey, ToShort as _};

use crate::{
    Database, DbError, PayloadAcquisitionPreparation, PayloadAdmissionConfig,
    PayloadAdmissionLimits, PayloadAdmissionPause, PayloadIngestOutcome, PayloadReservation,
    PayloadReservationOutcome,
};

fn content(author: RostraIdSecretKey, time: u64, text: &str) -> VerifiedEventContent {
    let raw = SocialPost::new_text(text.to_owned(), None, Default::default())
        .serialize_cbor()
        .unwrap();
    let event = Event::builder_raw_content()
        .author(author.id())
        .timestamp(Timestamp::from(time).to_offset_date_time().unwrap())
        .kind(EventKind::SOCIAL_POST)
        .content(&raw)
        .build();
    VerifiedEventContent::verify(
        VerifiedEvent::verify_signed(author.id(), event.signed_by(author)).unwrap(),
        raw,
    )
    .unwrap()
}

fn config(global: u64, author: u64, count: usize, bytes: u64) -> PayloadAdmissionConfig {
    PayloadAdmissionConfig::new(PayloadAdmissionLimits {
        database_bytes: NonZeroU64::new(global).unwrap(),
        author_bytes: NonZeroU64::new(author).unwrap(),
        overrides: BTreeMap::new(),
        in_flight_count: NonZeroUsize::new(count).unwrap(),
        in_flight_bytes: NonZeroU64::new(bytes).unwrap(),
    })
    .unwrap()
}

async fn ready(db: &Database) -> anyhow::Result<()> {
    while !db
        .rebuild_payload_accounting(NonZeroUsize::new(100).unwrap())
        .await?
        .ready
    {}
    Ok(())
}

// Primitive fixtures can replace config to exercise stale-owner rejection;
// production configuration is immutable account startup input.
async fn configure(db: &Database, config: PayloadAdmissionConfig) -> anyhow::Result<()> {
    db.write_with(|_| {
        let mut state = db.payload_admission.state.lock().unwrap();
        state.events.clear();
        state.config = Some(config);
        Ok(())
    })
    .await?;
    Ok(())
}

fn reserved(outcome: PayloadReservationOutcome) -> PayloadReservation {
    match outcome {
        PayloadReservationOutcome::Reserved(lease) => lease,
        other => panic!("expected reservation, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_account_preopen_ownership_and_exclusive_attachment() -> anyhow::Result<()> {
    let id = RostraIdSecretKey::generate().id();
    let account = crate::PayloadAccount::disabled(id);
    assert!(account.reserve_payload_allocation(u64::MAX)?.is_none());
    let mut first = Database::new_in_memory(id).await?;
    first.attach_payload_account(&account)?;
    assert_eq!(
        first.attach_payload_account(&account),
        Err(crate::PayloadAccountAttachError::DatabaseAlreadyAttached)
    );
    configure(&first, config(1000, 1000, 5, 1000)).await?;
    let mut second = Database::new_in_memory(id).await?;
    assert_eq!(
        second.attach_payload_account(&account),
        Err(crate::PayloadAccountAttachError::AccountAlreadyAttached)
    );
    let mut foreign = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    assert_eq!(
        foreign.attach_payload_account(&account),
        Err(crate::PayloadAccountAttachError::IdentityMismatch {
            database: foreign.self_id,
            account: id,
        })
    );
    drop(first);

    // No database is attached while this unverified request starts.
    let allocation = account.reserve_payload_allocation(700)?.unwrap();
    assert!(matches!(
        account.reserve_payload_allocation(301),
        Err(PayloadAdmissionPause::InFlightBytes)
    ));
    second.attach_payload_account(&account)?;
    assert_eq!(second.payload_admission_usage().buffer_bytes, 700);
    assert!(matches!(
        second.reserve_payload_allocation(301),
        Err(PayloadAdmissionPause::InFlightBytes)
    ));
    drop(allocation);
    assert_eq!(second.payload_admission_usage().buffer_bytes, 0);
    let mut independently_configured = Database::new_in_memory(id).await?;
    configure(&independently_configured, config(1000, 1000, 5, 1000)).await?;
    assert_eq!(
        independently_configured.attach_payload_account(&crate::PayloadAccount::disabled(id)),
        Err(crate::PayloadAccountAttachError::DatabaseConfigured)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_account_reattach_invalidates_logical_not_buffer_ownership() -> anyhow::Result<()>
{
    let id = RostraIdSecretKey::generate().id();
    let account = crate::PayloadAccount::disabled(id);
    let mut first = Database::new_in_memory(id).await?;
    first.attach_payload_account(&account)?;
    ready(&first).await?;
    configure(&first, config(1000, 1000, 5, 1000)).await?;
    let event = content(RostraIdSecretKey::generate(), 1, "old attachment");
    let reservation = reserved(first.reserve_payload(&event.event).await?);
    let buffer = reservation.try_acquire_buffer()?;
    let mut provisional = account.reserve_payload_allocation(100)?.unwrap();
    drop(first);

    let mut second = Database::new_in_memory(id).await?;
    second.attach_payload_account(&account)?;
    assert_eq!(second.payload_admission_usage().acquisitions, 0);
    assert_eq!(
        second.payload_admission_usage().buffer_bytes,
        100 + u64::from(event.content_len())
    );
    assert!(matches!(
        provisional.bind_to_reservation(&reservation),
        Err(PayloadAdmissionPause::ReservationExpired)
    ));
    ready(&second).await?;
    let replacement = reserved(second.reserve_payload(&event.event).await?);
    drop(buffer);
    drop(reservation);
    assert_eq!(second.payload_admission_usage().acquisitions, 1);
    let transferred = provisional.bind_to_reservation(&replacement)?;
    drop(transferred);
    drop(replacement);
    assert_eq!(second.payload_admission_usage().buffer_bytes, 0);
    assert_eq!(second.payload_admission_usage().acquisitions, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_account_concurrent_preparse_is_bounded_and_isolated() -> anyhow::Result<()> {
    let id = RostraIdSecretKey::generate().id();
    let account = crate::PayloadAccount::disabled(id);
    let mut db = Database::new_in_memory(id).await?;
    db.attach_payload_account(&account)?;
    configure(&db, config(1000, 1000, 5, 1000)).await?;
    drop(db);
    let other_id = RostraIdSecretKey::generate().id();
    let other_account = crate::PayloadAccount::disabled(other_id);
    let mut other_db = Database::new_in_memory(other_id).await?;
    other_db.attach_payload_account(&other_account)?;
    configure(&other_db, config(1000, 1000, 5, 1000)).await?;

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(9));
    let mut requests = vec![];
    for _ in 0..8 {
        let account = account.clone();
        let barrier = barrier.clone();
        requests.push(tokio::spawn(async move {
            let result = account.reserve_payload_allocation(600);
            barrier.wait().await;
            result
        }));
    }
    barrier.wait().await;
    // Requests retain their results while this account attaches concurrently.
    let mut reopened = Database::new_in_memory(id).await?;
    reopened.attach_payload_account(&account)?;
    assert_eq!(reopened.payload_admission_usage().buffer_bytes, 600);
    let independent = other_account.reserve_payload_allocation(1000)?.unwrap();
    assert_eq!(other_db.payload_admission_usage().buffer_bytes, 1000);
    let mut allocations = vec![];
    let mut paused = 0;
    for request in requests {
        match request.await? {
            Ok(Some(allocation)) => allocations.push(allocation),
            Err(PayloadAdmissionPause::InFlightBytes) => paused += 1,
            other => panic!("unexpected preparse result: {other:?}"),
        }
    }
    assert_eq!(allocations.len(), 1);
    assert_eq!(paused, 7);
    drop(allocations);
    assert_eq!(reopened.payload_admission_usage().buffer_bytes, 0);
    assert_eq!(other_db.payload_admission_usage().buffer_bytes, 1000);
    drop(independent);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_provisional_growth_transfer_and_late_release() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    assert!(db.reserve_payload_allocation(u64::MAX)?.is_none());
    ready(&db).await?;
    let event = content(RostraIdSecretKey::generate(), 1, "provisional");
    let n = u64::from(event.content_len());
    configure(&db, config(n, n, 2, 3 * n)).await?;
    let mut allocation = db.reserve_payload_allocation(0)?.unwrap();
    allocation.try_grow(2 * n)?;
    assert_eq!(
        allocation.try_grow(4 * n),
        Err(PayloadAdmissionPause::InFlightBytes)
    );
    assert_eq!(db.payload_admission_usage().buffer_bytes, 2 * n);
    let conversion = db.reserve_payload_allocation(n)?.unwrap();
    assert!(matches!(
        db.reserve_payload_allocation(0),
        Err(PayloadAdmissionPause::InFlightCount)
    ));
    drop(conversion);
    let reservation = reserved(db.reserve_payload(&event.event).await?);
    let buffer = allocation.bind_to_reservation(&reservation)?;
    assert_eq!(
        allocation.try_grow(0),
        Err(PayloadAdmissionPause::ReservationExpired)
    );
    assert!(matches!(
        allocation.bind_to_reservation(&reservation),
        Err(PayloadAdmissionPause::ReservationExpired)
    ));
    drop(allocation);
    assert_eq!(db.payload_admission_usage().buffers, 1);
    assert_eq!(db.payload_admission_usage().buffer_bytes, 2 * n);
    assert_eq!(
        db.try_process_admitted_event_content(&event, Some(&buffer))
            .await?,
        PayloadIngestOutcome::Processed
    );
    assert_eq!(db.payload_admission_usage().acquisitions, 0);
    assert_eq!(db.payload_admission_usage().buffer_bytes, 2 * n);
    drop(buffer);
    assert_eq!(db.payload_admission_usage().buffer_bytes, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_provisional_bad_binding_keeps_capacity_owned() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let other = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    ready(&db).await?;
    ready(&other).await?;
    let author = RostraIdSecretKey::generate();
    let event = content(author, 1, "binding");
    let n = u64::from(event.content_len());
    configure(&db, config(n, n, 2, n)).await?;
    configure(&other, config(n, n, 2, n)).await?;
    let reservation = reserved(db.reserve_payload(&event.event).await?);
    let mut foreign = other.reserve_payload_allocation(n)?.unwrap();
    assert!(matches!(
        foreign.bind_to_reservation(&reservation),
        Err(PayloadAdmissionPause::ReservationExpired)
    ));
    assert_eq!(other.payload_admission_usage().buffer_bytes, n);
    let mut short = db.reserve_payload_allocation(n - 1)?.unwrap();
    assert!(matches!(
        short.bind_to_reservation(&reservation),
        Err(PayloadAdmissionPause::ReservationExpired)
    ));
    assert_eq!(db.payload_admission_usage().buffer_bytes, n - 1);
    short.try_grow(n)?;
    let deletion = Event::builder_raw_content()
        .author(author.id())
        .kind(EventKind::NULL)
        .delete(event.event_id().to_short())
        .build();
    db.try_process_event(&VerifiedEvent::verify_signed(
        author.id(),
        deletion.signed_by(author),
    )?)
    .await?;
    assert!(matches!(
        short.bind_to_reservation(&reservation),
        Err(PayloadAdmissionPause::ReservationExpired)
    ));
    assert_eq!(db.payload_admission_usage().buffer_bytes, n);
    drop(short);
    drop(foreign);
    assert_eq!(db.payload_admission_usage().buffers, 0);
    assert_eq!(other.payload_admission_usage().buffers, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_acquisition_reuses_store_and_stops_terminal_fetches() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let author = RostraIdSecretKey::generate();
    let first = content(author, 1, "same bytes");
    let second = content(author, 2, "same bytes");
    db.try_process_event_with_content(&first).await?;
    assert!(matches!(
        db.prepare_payload_acquisition_detailed(&second.event)
            .await?,
        PayloadAcquisitionPreparation::Materialized
    ));
    assert!(
        !db.is_event_content_missing(second.event_id().to_short())
            .await
    );
    ready(&db).await?;
    let n = u64::from(first.content_len());
    configure(&db, config(4 * n, 4 * n, 2, 2 * n)).await?;
    let third = content(author, 3, "same bytes");
    let occupied = db.reserve_payload_allocation(n)?.unwrap();
    assert!(matches!(
        db.prepare_payload_acquisition(&third.event).await?,
        PayloadReservationOutcome::Deferred(PayloadAdmissionPause::InFlightCount)
    ));
    let missing = db.peek_next_missing_content().await.unwrap();
    assert_eq!(missing.event_id, third.event_id().to_short());
    assert_eq!(missing.fetch_attempt_count, 0);
    drop(occupied);
    assert!(matches!(
        db.prepare_payload_acquisition(&third.event).await?,
        PayloadReservationOutcome::Unneeded
    ));
    assert!(db.peek_next_missing_content().await.is_none());
    assert_eq!(db.payload_admission_usage().buffers, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_deferred_retry_does_not_count_peer_failure_or_starve_front() -> anyhow::Result<()>
{
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = content(RostraIdSecretKey::generate(), 1, "one");
    let b = content(RostraIdSecretKey::generate(), 1, "two");
    db.try_process_event(&a.event).await?;
    db.try_process_event(&b.event).await?;
    let first = db.peek_next_missing_content().await.unwrap();
    let later = first.scheduled_time.saturating_add_secs(30);
    db.defer_content_fetch(first.event_id, first.scheduled_time, later)
        .await?;
    let next = db.peek_next_missing_content().await.unwrap();
    assert_ne!(first.event_id, next.event_id);
    assert_eq!(next.fetch_attempt_count, 0);
    db.defer_content_fetch(
        first.event_id,
        first.scheduled_time,
        later.saturating_add_secs(30),
    )
    .await?;
    let crate::EventContentState::Missing {
        fetch_attempt_count,
        last_fetch_attempt,
        next_fetch_attempt,
    } = db.get_event_content_state(first.event_id).await.unwrap()
    else {
        panic!("Temporary pressure must keep Missing");
    };
    assert_eq!(fetch_attempt_count, 0);
    assert_eq!(last_fetch_attempt, None);
    assert_eq!(next_fetch_attempt, later);
    for event in [&a, &b] {
        db.try_process_event_with_content(event).await?;
    }
    db.defer_content_fetch(first.event_id, later, later.saturating_add_secs(30))
        .await?;
    assert!(db.peek_next_missing_content().await.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_disabled_and_explicit_config_validation() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let event = content(RostraIdSecretKey::generate(), 1, "disabled");
    assert!(matches!(
        db.reserve_payload(&event.event).await?,
        PayloadReservationOutcome::Disabled
    ));
    db.try_process_event_with_content(&event).await?;
    assert_eq!(db.payload_admission_usage().acquisitions, 0);
    assert_eq!(
        PayloadAdmissionConfig::experimental_low_water(u64::MAX),
        16_602_069_666_338_596_453
    );
    assert!(
        PayloadAdmissionConfig::new(PayloadAdmissionLimits {
            database_bytes: NonZeroU64::new(1).unwrap(),
            author_bytes: NonZeroU64::new(1).unwrap(),
            overrides: BTreeMap::new(),
            in_flight_count: NonZeroUsize::new(4097).unwrap(),
            in_flight_bytes: NonZeroU64::new(1).unwrap(),
        })
        .is_none()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_counts_logical_once_and_every_racing_buffer() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    ready(&db).await?;
    let event = content(RostraIdSecretKey::generate(), 1, "racing peer payload");
    let n = u64::from(event.content_len());
    configure(&db, config(n, n, 4, 3 * n)).await?;
    let lease = reserved(db.reserve_payload(&event.event).await?);
    assert!(matches!(
        db.reserve_payload(&event.event).await?,
        PayloadReservationOutcome::Deferred(PayloadAdmissionPause::AlreadyReserved)
    ));
    let one = lease.try_acquire_buffer().unwrap();
    let two = lease.try_acquire_buffer().unwrap();
    let three = lease.try_acquire_buffer().unwrap();
    assert_eq!(
        lease.try_acquire_buffer().unwrap_err(),
        PayloadAdmissionPause::InFlightBytes
    );
    assert_eq!(db.payload_admission_usage().logical_reserved_bytes, n);
    assert_eq!(db.payload_admission_usage().buffer_bytes, 3 * n);
    let aborted: crate::DbResult<()> = db
        .write_with(|tx| {
            db.process_event_content_with_buffer_tx(&event, Timestamp::now(), tx, Some(&one))?;
            Err(DbError::Overflow)
        })
        .await;
    assert!(matches!(aborted, Err(DbError::Overflow)));
    assert_eq!(db.payload_admission_usage().logical_reserved_bytes, n);
    assert_eq!(
        db.get_payload_usage().await?.unwrap().logical_current_bytes,
        0
    );
    assert_eq!(
        db.try_process_admitted_event_content(&event, Some(&one))
            .await?,
        PayloadIngestOutcome::Processed
    );
    assert_eq!(db.payload_admission_usage().logical_reserved_bytes, 0);
    assert_eq!(db.payload_admission_usage().buffer_bytes, 3 * n);
    assert_eq!(
        lease.try_acquire_buffer().unwrap_err(),
        PayloadAdmissionPause::ReservationExpired
    );
    drop((one, two, three, lease));
    assert_eq!(db.payload_admission_usage().buffer_bytes, 0);
    assert_eq!(
        db.get_payload_usage().await?.unwrap().logical_current_bytes,
        n
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_temporary_pause_preserves_header_and_legacy_atomicity() -> anyhow::Result<()> {
    let local = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(local.id()).await?;
    let event = content(local, 1, "local publication is protected, not exempt");
    configure(&db, config(1, 1, 4, 1024)).await?;
    assert_eq!(
        db.try_process_admitted_event_content(&event, None).await?,
        PayloadIngestOutcome::Deferred(PayloadAdmissionPause::AccountingNotReady)
    );
    ready(&db).await?;
    assert_eq!(
        db.try_process_admitted_event_content(&event, None).await?,
        PayloadIngestOutcome::Deferred(PayloadAdmissionPause::AuthorCapacity)
    );
    let other = content(local, 2, "legacy raw signed API");
    assert!(matches!(
        db.try_process_event_with_content(&other).await,
        Err(DbError::PayloadAdmissionPaused {
            reason: PayloadAdmissionPause::AuthorCapacity
        })
    ));
    assert!(db.get_event(other.event_id()).await.is_none());
    assert!(db.get_event(event.event_id()).await.is_some());
    assert!(db.get_event_content(event.event_id()).await.is_none());
    db.read_with(|tx| {
        assert!(
            tx.open_table(&crate::events_content_missing::TABLE)?
                .get(&(Timestamp::ZERO, event.event_id().to_short()))?
                .is_some()
        );
        Ok(())
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_global_author_override_and_concurrent_writers() -> anyhow::Result<()> {
    let db =
        std::sync::Arc::new(Database::new_in_memory(RostraIdSecretKey::generate().id()).await?);
    ready(&db).await?;
    let a = RostraIdSecretKey::generate();
    let b = RostraIdSecretKey::generate();
    let one = content(a, 1, "one");
    let two = content(b, 1, "two");
    let n = u64::from(one.content_len());
    assert_eq!(one.content_len(), two.content_len());
    // The config uses full account overrides and strict ceilings, not shares.
    let limits = PayloadAdmissionConfig::new(PayloadAdmissionLimits {
        database_bytes: NonZeroU64::new(n).unwrap(),
        author_bytes: NonZeroU64::new(1).unwrap(),
        overrides: [
            (a.id(), NonZeroU64::new(2 * n).unwrap()),
            (b.id(), NonZeroU64::new(2 * n).unwrap()),
        ]
        .into(),
        in_flight_count: NonZeroUsize::new(4).unwrap(),
        in_flight_bytes: NonZeroU64::new(4 * n).unwrap(),
    })
    .unwrap();
    configure(&db, limits).await?;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for event in [one, two] {
        let db = db.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            db.try_process_admitted_event_content(&event, None).await
        }));
    }
    let mut processed = 0;
    let mut paused = 0;
    for task in tasks {
        match task.await?? {
            PayloadIngestOutcome::Processed => processed += 1,
            PayloadIngestOutcome::Deferred(PayloadAdmissionPause::DatabaseCapacity) => paused += 1,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((processed, paused), (1, 1));
    assert_eq!(
        db.get_payload_usage().await?.unwrap().logical_current_bytes,
        n
    );
    assert_eq!(db.payload_admission_usage().logical_reserved_bytes, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_cancellation_abort_and_capacity_wakeup() -> anyhow::Result<()> {
    let db =
        std::sync::Arc::new(Database::new_in_memory(RostraIdSecretKey::generate().id()).await?);
    ready(&db).await?;
    let event = content(RostraIdSecretKey::generate(), 1, "cancel");
    let n = u64::from(event.content_len());
    configure(&db, config(n, n, 1, n)).await?;
    let (started, rx) = tokio::sync::oneshot::channel();
    let task_db = db.clone();
    let event_header = event.event;
    let task = tokio::spawn(async move {
        let lease = reserved(task_db.reserve_payload(&event_header).await.unwrap());
        let _buffer = lease.try_acquire_buffer().unwrap();
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    rx.await?;
    assert_eq!(db.payload_admission_usage().buffer_bytes, n);
    let notified = db.payload_admission_changed();
    tokio::pin!(notified);
    notified.as_mut().enable();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(1), notified).await?;
    assert_eq!(db.payload_admission_usage().acquisitions, 0);
    assert_eq!(db.payload_admission_usage().buffer_bytes, 0);
    let aborted: crate::DbResult<()> = db
        .write_with(|tx| {
            let _lease = reserved(db.reserve_payload_tx(tx, &event.event)?);
            Err(DbError::Overflow)
        })
        .await;
    assert!(matches!(aborted, Err(DbError::Overflow)));
    assert_eq!(db.payload_admission_usage().acquisitions, 0);
    let lease = reserved(db.reserve_payload(&event.event).await?);
    let buffer = lease.try_acquire_buffer().unwrap();
    drop(lease);
    assert_eq!(db.payload_admission_usage().acquisitions, 1);
    drop(buffer);
    assert_eq!(db.payload_admission_usage().acquisitions, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_hash_reuse_pauses_then_materializes_without_fetch() -> anyhow::Result<()> {
    let author = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let one = content(author, 1, "shared hash");
    let two = content(author, 2, "shared hash");
    let n = u64::from(one.content_len());
    db.try_process_event_content(&one).await?;
    db.try_process_event(&two.event).await?;
    ready(&db).await?;
    db.read_with(|tx| {
        assert!(
            tx.open_table(&crate::events_content_missing::TABLE)?
                .get(&(Timestamp::ZERO, two.event_id().to_short()))?
                .is_some()
        );
        Ok(())
    })
    .await?;
    configure(&db, config(n, n, 4, 4 * n)).await?;
    assert_eq!(
        db.try_materialize_stored_payload(two.event_id().to_short())
            .await?,
        PayloadIngestOutcome::Deferred(PayloadAdmissionPause::AuthorCapacity)
    );
    db.read_with(|tx| {
        assert!(
            tx.open_table(&crate::events_content_missing::TABLE)?
                .get(&(Timestamp::ZERO, two.event_id().to_short()))?
                .is_some()
        );
        Ok(())
    })
    .await?;
    assert_eq!(
        db.get_payload_usage().await?.unwrap().logical_current_bytes,
        n
    );
    configure(&db, config(2 * n, 2 * n, 4, 4 * n)).await?;
    assert_eq!(
        db.try_materialize_stored_payload(two.event_id().to_short())
            .await?,
        PayloadIngestOutcome::Processed
    );
    let usage = db.get_payload_usage().await?.unwrap();
    assert_eq!(usage.logical_current_bytes, 2 * n);
    assert_eq!(usage.unique_stored_bytes, n);
    assert!(!db.is_event_content_missing(two.event_id().to_short()).await);
    assert_eq!(
        db.try_materialize_stored_payload(two.event_id().to_short())
            .await?,
        PayloadIngestOutcome::Unchanged
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_state_change_releases_logical_not_buffers_and_late_delivery_is_terminal()
-> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    ready(&db).await?;
    let event = content(RostraIdSecretKey::generate(), 1, "terminal");
    let n = u64::from(event.content_len());
    configure(&db, config(n, n, 4, 4 * n)).await?;
    let lease = reserved(db.reserve_payload(&event.event).await?);
    let buffer = lease.try_acquire_buffer().unwrap();
    let request = crate::QuotaPruneRequest {
        id: event.event_id().to_short(),
        target: crate::QuotaPruneTarget::Missing,
        reason: crate::QuotaPruneReason::GlobalQuota,
        policy: rostra_core::retention::RetentionPolicy::new(1, 1, 0, 0, 1, 10).unwrap(),
        clock: crate::RetentionClock::Trusted(Timestamp::now()),
    };
    assert!(matches!(
        db.prune_quota_payload(request).await?,
        crate::QuotaPruneOutcome::Pruned { .. }
    ));
    assert_eq!(db.payload_admission_usage().logical_reserved_bytes, 0);
    assert_eq!(db.payload_admission_usage().buffer_bytes, n);
    assert_eq!(
        db.try_process_admitted_event_content(&event, Some(&buffer))
            .await?,
        PayloadIngestOutcome::Unchanged
    );
    assert!(db.get_event_content(event.event_id()).await.is_none());
    assert_eq!(
        lease.try_acquire_buffer().unwrap_err(),
        PayloadAdmissionPause::ReservationExpired
    );
    drop((lease, buffer));
    assert_eq!(db.payload_admission_usage().buffer_bytes, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_foreign_and_stale_buffers_do_not_authorize_materialization() -> anyhow::Result<()>
{
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let other = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let event = content(RostraIdSecretKey::generate(), 1, "foreign");
    let n = u64::from(event.content_len());
    for db in [&db, &other] {
        ready(db).await?;
        configure(db, config(n, n, 4, 4 * n)).await?;
    }
    let lease = reserved(db.reserve_payload(&event.event).await?);
    let buffer = lease.try_acquire_buffer().unwrap();
    assert_eq!(
        other
            .try_process_admitted_event_content(&event, Some(&buffer))
            .await?,
        PayloadIngestOutcome::Deferred(PayloadAdmissionPause::ReservationExpired)
    );
    configure(&db, config(n, n, 4, 4 * n)).await?;
    let replacement = reserved(db.reserve_payload(&event.event).await?);
    assert_eq!(
        db.try_process_admitted_event_content(&event, Some(&buffer))
            .await?,
        PayloadIngestOutcome::Deferred(PayloadAdmissionPause::ReservationExpired)
    );
    drop((buffer, lease));
    assert_eq!(db.payload_admission_usage().acquisitions, 1);
    drop(replacement);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_pending_reservations_count_against_both_caps() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    ready(&db).await?;
    let author = RostraIdSecretKey::generate();
    let one = content(author, 1, "same size");
    let two = content(author, 2, "same size");
    let three = content(RostraIdSecretKey::generate(), 1, "same size");
    let n = u64::from(one.content_len());
    configure(&db, config(n, n, 4, 4 * n)).await?;
    let lease = reserved(db.reserve_payload(&one.event).await?);
    assert!(matches!(
        db.reserve_payload(&two.event).await?,
        PayloadReservationOutcome::Deferred(PayloadAdmissionPause::AuthorCapacity)
    ));
    assert!(matches!(
        db.reserve_payload(&three.event).await?,
        PayloadReservationOutcome::Deferred(PayloadAdmissionPause::DatabaseCapacity)
    ));
    assert_eq!(
        db.get_payload_usage().await?.unwrap().logical_current_bytes,
        0
    );
    drop(lease);
    let lease = reserved(db.reserve_payload(&three.event).await?);
    drop(lease);
    configure(&db, config(4 * n, 4 * n, 1, 4 * n)).await?;
    let lease = reserved(db.reserve_payload(&one.event).await?);
    assert!(matches!(
        db.reserve_payload(&two.event).await?,
        PayloadReservationOutcome::Deferred(PayloadAdmissionPause::InFlightCount)
    ));
    let buffer = lease.try_acquire_buffer().unwrap();
    assert_eq!(
        lease.try_acquire_buffer().unwrap_err(),
        PayloadAdmissionPause::InFlightCount
    );
    drop((buffer, lease));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_invalid_is_distinct_and_protected_kinds_are_not_exempt() -> anyhow::Result<()> {
    let author = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(author.id()).await?;
    ready(&db).await?;
    for kind in [EventKind::SOCIAL_POST, EventKind::from(65535)] {
        let bytes = rostra_core::event::EventContentRaw::new(vec![0xff; 20]);
        let signed = Event::builder_raw_content()
            .author(author.id())
            .timestamp(Timestamp::from(1).to_offset_date_time().unwrap())
            .kind(kind)
            .content(&bytes)
            .build()
            .signed_by(author);
        let event = VerifiedEventContent::verify(
            VerifiedEvent::verify_signed(author.id(), signed).unwrap(),
            bytes,
        )
        .unwrap();
        configure(&db, config(1, 1, 4, 1024)).await?;
        assert_eq!(
            db.try_process_admitted_event_content(&event, None).await?,
            PayloadIngestOutcome::Deferred(PayloadAdmissionPause::AuthorCapacity)
        );
        configure(&db, config(1024, 1024, 4, 1024)).await?;
        let lease = reserved(db.reserve_payload(&event.event).await?);
        let buffer = lease.try_acquire_buffer().unwrap();
        let outcome = db
            .try_process_admitted_event_content(&event, Some(&buffer))
            .await?;
        if kind == EventKind::SOCIAL_POST {
            assert_eq!(outcome, PayloadIngestOutcome::Invalid);
        }
        assert_eq!(db.payload_admission_usage().logical_reserved_bytes, 0);
        assert_eq!(
            db.payload_admission_usage().buffer_bytes,
            u64::from(event.content_len())
        );
        drop((buffer, lease));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn detailed_preparation_does_not_count_shared_invalid_content() -> anyhow::Result<()> {
    let author = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(author.id()).await?;
    let bytes = rostra_core::event::EventContentRaw::new(vec![0xff; 20]);
    let build = |kind, timestamp| {
        let signed = Event::builder_raw_content()
            .author(author.id())
            .timestamp(Timestamp::from(timestamp).to_offset_date_time().unwrap())
            .kind(kind)
            .content(&bytes)
            .build()
            .signed_by(author);
        VerifiedEventContent::verify(
            VerifiedEvent::verify_signed(author.id(), signed).unwrap(),
            bytes.clone(),
        )
        .unwrap()
    };
    let stored = build(EventKind::from(65535), 1);
    let invalid = build(EventKind::SOCIAL_POST, 2);
    db.try_process_event_content(&stored).await?;

    assert!(matches!(
        db.prepare_payload_acquisition_detailed(&invalid.event)
            .await?,
        PayloadAcquisitionPreparation::Satisfied
    ));
    assert!(
        db.get_event_content(invalid.event_id().to_short())
            .await
            .is_none()
    );
    assert!(
        !db.is_event_content_missing(invalid.event_id().to_short())
            .await
    );
    assert!(matches!(
        db.prepare_payload_acquisition(&invalid.event).await?,
        PayloadReservationOutcome::Unneeded
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn envelopes_schedule_shared_hash_work_in_disabled_and_enforce_modes() -> anyhow::Result<()> {
    let author = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let first = content(author, 1, "already in hash store");
    let second = content(author, 2, "already in hash store");
    let third = content(author, 3, "already in hash store");
    db.try_process_event_content(&first).await?;
    db.try_process_event(&second.event).await?;
    assert!(
        db.get_social_post(second.event_id().to_short())
            .await
            .is_none()
    );
    let scheduled = db.peek_next_missing_content().await.unwrap();
    assert_eq!(scheduled.event_id, second.event_id().to_short());
    assert_eq!(scheduled.scheduled_time, Timestamp::ZERO);
    assert_eq!(scheduled.fetch_attempt_count, 0);
    assert!(matches!(
        db.prepare_payload_acquisition_detailed(&second.event)
            .await?,
        PayloadAcquisitionPreparation::Materialized
    ));
    assert!(
        db.get_social_post(second.event_id().to_short())
            .await
            .is_some()
    );
    assert!(db.peek_next_missing_content().await.is_none());
    assert!(matches!(
        db.prepare_payload_acquisition_detailed(&second.event)
            .await?,
        PayloadAcquisitionPreparation::Satisfied
    ));
    ready(&db).await?;
    let n = u64::from(first.content_len());
    configure(&db, config(3 * n, 3 * n, 4, 4 * n)).await?;
    db.try_process_event(&third.event).await?;
    db.read_with(|tx| {
        assert!(
            tx.open_table(&crate::events_content_missing::TABLE)?
                .get(&(Timestamp::ZERO, third.event_id().to_short()))?
                .is_some()
        );
        Ok(())
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_parent_envelope_schedules_shared_hash_work() -> anyhow::Result<()> {
    let author = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let stored = content(author, 1, "shared missing parent");
    let parent = content(author, 2, "shared missing parent");
    let child_content = SocialPost::new_text("child".to_owned(), None, Default::default())
        .serialize_cbor()
        .unwrap();
    let child = Event::builder_raw_content()
        .author(author.id())
        .timestamp(Timestamp::from(3).to_offset_date_time().unwrap())
        .kind(EventKind::SOCIAL_POST)
        .parent_prev(parent.event_id().to_short())
        .content(&child_content)
        .build()
        .signed_by(author);
    let child = VerifiedEvent::verify_signed(author.id(), child).unwrap();
    db.try_process_event_content(&stored).await?;
    db.try_process_event(&child).await?;
    let (outcome, _) = db.try_process_event(&parent.event).await?;
    assert!(matches!(
        outcome,
        crate::InsertEventOutcome::Inserted {
            was_missing: true,
            ..
        }
    ));
    db.read_with(|tx| {
        let schedule = tx.open_table(&crate::events_content_missing::TABLE)?;
        assert!(
            schedule
                .get(&(Timestamp::ZERO, parent.event_id().to_short()))?
                .is_some()
        );
        Ok(())
    })
    .await?;
    assert!(
        db.get_social_post(parent.event_id().to_short())
            .await
            .is_none()
    );
    assert!(matches!(
        db.prepare_payload_acquisition(&parent.event).await?,
        PayloadReservationOutcome::Unneeded
    ));
    assert!(
        db.get_social_post(parent.event_id().to_short())
            .await
            .is_some()
    );
    assert!(matches!(
        db.prepare_payload_acquisition(&parent.event).await?,
        PayloadReservationOutcome::Unneeded
    ));
    let receipts = db
        .read_with(|tx| {
            let table = tx.open_table(&crate::social_posts_by_received_at::TABLE)?;
            let mut count = 0;
            for entry in table.range(..)? {
                let (_, event_id) = entry?;
                if event_id.value() == parent.event_id().to_short() {
                    count += 1;
                }
            }
            Ok(count)
        })
        .await?;
    assert_eq!(receipts, 1);
    db.read_with(|tx| {
        assert!(
            tx.open_table(&crate::events_content_missing::TABLE)?
                .get(&(Timestamp::ZERO, parent.event_id().to_short()))?
                .is_none()
        );
        Ok(())
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn envelope_redelivery_restores_current_retry_schedule() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let event = content(RostraIdSecretKey::generate(), 1, "retry metadata");
    db.try_process_event(&event.event).await?;
    let attempted_at = Timestamp::from(10);
    let retry_at = Timestamp::from(20);
    db.record_failed_content_fetch(
        event.event_id().to_short(),
        Timestamp::ZERO,
        attempted_at,
        retry_at,
    )
    .await;
    db.write_with(|tx| {
        tx.open_table(&crate::events_content_missing::TABLE)?
            .remove(&(retry_at, event.event_id().to_short()))?;
        Ok(())
    })
    .await?;
    db.try_process_event(&event.event).await?;
    assert!(matches!(
        db.get_event_content_state(event.event_id().to_short()).await,
        Some(crate::EventContentState::Missing {
            last_fetch_attempt: Some(last),
            fetch_attempt_count: 1,
            next_fetch_attempt,
        }) if last == attempted_at && next_fetch_attempt == retry_at
    ));
    db.read_with(|tx| {
        assert!(
            tx.open_table(&crate::events_content_missing::TABLE)?
                .get(&(retry_at, event.event_id().to_short()))?
                .is_some()
        );
        Ok(())
    })
    .await?;
    Ok(())
}
