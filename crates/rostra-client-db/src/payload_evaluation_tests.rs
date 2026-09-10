//! Opt-in, disk-backed, newly generated data only. No path or network inputs.

use std::collections::BTreeMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::time::{Duration, Instant};

use rostra_core::Timestamp;
use rostra_core::event::content_kind::{EventContentKind as _, SocialPost};
use rostra_core::event::{Event, EventKind, VerifiedEvent, VerifiedEventContent};
use rostra_core::id::{RostraIdSecretKey, ToShort as _};
use rostra_core::retention::RetentionPolicy;

use crate::payload_runtime::{PayloadRuntime, RuntimeCursor, RuntimeTurn};
use crate::{Database, PayloadAdmissionConfig, PayloadAdmissionLimits, PayloadRuntimeLimits};

fn post(n: usize) -> VerifiedEventContent {
    let author = RostraIdSecretKey::from_bytes([1 + (n % 32) as u8; 32]);
    let size = if n.is_multiple_of(64) {
        256 * 1024
    } else if n.is_multiple_of(8) {
        16 * 1024
    } else {
        1024
    };
    let content = SocialPost::new_text(
        format!("{}{}", n / 2, "x".repeat(size)),
        None,
        Default::default(),
    )
    .serialize_cbor()
    .unwrap();
    let signed = Event::builder_raw_content()
        .author(author.id())
        .kind(EventKind::SOCIAL_POST)
        .timestamp(Timestamp::from(1 + n as u64).to_offset_date_time().unwrap())
        .content(&content)
        .build()
        .signed_by(author);
    VerifiedEventContent::assume_verified(
        VerifiedEvent::verify_signed(author.id(), signed).unwrap(),
        content,
    )
}

fn timing(label: &str, times: &mut [u128]) {
    times.sort_unstable();
    println!(
        "{label}: samples={} p50_us={} p95_us={} p99_us={} max_us={}",
        times.len(),
        times[times.len() / 2],
        times[times.len() * 95 / 100],
        times[times.len() * 99 / 100],
        times[times.len() - 1]
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "disposable indexed performance experiment; run explicitly in release"]
async fn disposable_payload_index_evaluation() -> anyhow::Result<()> {
    evaluate(8192, 900).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "bounded disposable progress diagnosis"]
async fn disposable_payload_progress_diagnosis() -> anyhow::Result<()> {
    evaluate(128, 15).await
}

async fn evaluate(events_count: usize, runtime_seconds: u64) -> anyhow::Result<()> {
    let policy = RetentionPolicy::new(1024, 30 * 86400, 32768, 65536, 64, 0).unwrap();
    let holder = RostraIdSecretKey::from_bytes([200; 32]).id();
    // Signing/serialization is outside the timed transactions; identical inputs
    // go through the real ingestion boundary in both arms.
    let events: Vec<_> = (0..events_count).map(post).collect();
    for indexed in [false, true] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("disposable.redb");
        let mut db = Database::open(&path, holder).await?;
        while !db
            .rebuild_payload_accounting(NonZeroUsize::new(256).unwrap())
            .await?
            .ready
        {}
        if indexed {
            db.configure_retention_index(policy).await?;
            while !db
                .rebuild_retention_index(NonZeroUsize::new(256).unwrap())
                .await?
                .unwrap()
                .ready
            {}
        }
        let mut ingestion = Vec::new();
        for event in &events {
            let start = Instant::now();
            db.try_process_event_with_content(event).await?;
            ingestion.push(start.elapsed().as_micros());
        }
        timing(&format!("ingest indexed={indexed}"), &mut ingestion);
        let before = db.get_payload_usage().await?.unwrap();
        println!(
            "loaded indexed={indexed} events={events_count} authors=32 usage={before:?} file_bytes={}",
            std::fs::metadata(&path)?.len()
        );
        if !indexed {
            continue;
        }
        // Rebuild a different generation over the populated index. Each timed
        // public call is one real bounded writer transaction, including commit.
        let rebuilt = RetentionPolicy::new(1024, 30 * 86400, 32768, 65536, 8, 0).unwrap();
        db.configure_retention_index(rebuilt).await?;
        let mut transactions = Vec::new();
        let mut visited = 0;
        loop {
            let start = Instant::now();
            let progress = db
                .rebuild_retention_index(NonZeroUsize::new(64).unwrap())
                .await?
                .unwrap();
            transactions.push(start.elapsed().as_micros());
            visited += progress.visited;
            if progress.ready {
                break;
            }
        }
        timing("index_rebuild_transaction_64rows", &mut transactions);
        println!("index_rebuild_visited={visited}");
        let mut nominations = Vec::new();
        loop {
            let start = Instant::now();
            let progress = db
                .rebuild_quota_payload_nominations(NonZeroUsize::new(64).unwrap())
                .await?;
            nominations.push(start.elapsed().as_micros());
            if progress.ready {
                break;
            }
        }
        timing("nomination_rebuild_transaction_64rows", &mut nominations);
        let now = Timestamp::now();
        let mut promotion = Vec::new();
        let mut promoted = 0;
        loop {
            let start = Instant::now();
            let progress = db
                .promote_retention_grace(
                    crate::RetentionClock::Trusted(now),
                    NonZeroUsize::new(64).unwrap(),
                )
                .await?
                .unwrap();
            promotion.push(start.elapsed().as_micros());
            promoted += progress.visited;
            if progress.visited < 64 {
                break;
            }
        }
        anyhow::ensure!(promoted == events_count);
        timing("grace_promotion_transaction_64rows", &mut promotion);
        println!("grace_promoted={promoted}");
        let config = PayloadAdmissionConfig::new(PayloadAdmissionLimits {
            database_bytes: NonZeroU64::new(before.logical_current_bytes / 2).unwrap(),
            author_bytes: NonZeroU64::new(before.logical_current_bytes).unwrap(),
            overrides: BTreeMap::new(),
            in_flight_count: NonZeroUsize::new(8).unwrap(),
            in_flight_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
        })
        .unwrap();
        let generation = db.retention_index_progress().await?.unwrap().generation;
        db.payload_runtime = Some(
            PayloadRuntime::new(
                generation,
                config.identity(),
                PayloadRuntimeLimits {
                    operations: NonZeroUsize::new(64).unwrap(),
                    bytes: 1024 * 1024,
                    gc_bytes: 1024 * 1024,
                    time: Duration::from_millis(10),
                },
            )
            .unwrap(),
        );
        db.payload_admission.state.lock().unwrap().config = Some(config);
        let runtime = db.payload_runtime.as_ref().unwrap();
        let mut cursor = RuntimeCursor::default();
        let mut turns = Vec::new();
        let mut settled = false;
        let mut recovery_waits = 0;
        let maintenance_start = Instant::now();
        for turn in 0..100_000 {
            if maintenance_start.elapsed() >= Duration::from_secs(runtime_seconds) {
                break;
            }
            let start = Instant::now();
            let result = runtime.turn(&db, &mut cursor).await?;
            turns.push(start.elapsed().as_micros());
            if turn % 1000 == 999 {
                println!(
                    "maintenance_progress turns={} usage={:?}",
                    turn + 1,
                    db.get_payload_usage().await?
                );
            }
            if result == RuntimeTurn::Wait {
                // Wait is a recovery boundary, not an assertion of target
                // attainment: a successful prune invalidates advisory sweeps.
                let usage = db.get_payload_usage().await?.unwrap();
                if usage.logical_current_bytes <= before.logical_current_bytes / 2 * 9 / 10
                    && gc_backlog(&db).await? == 0
                {
                    settled = true;
                    break;
                }
                recovery_waits += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        timing("maintenance_turn_64ops_1MiB_10ms", &mut turns);
        let after = db.get_payload_usage().await?.unwrap();
        println!(
            "maintenance_finished settled={settled} elapsed_ms={} usage={after:?}",
            maintenance_start.elapsed().as_millis()
        );
        println!("runtime_cursor={cursor:?} recovery_waits={recovery_waits}");
        anyhow::ensure!(
            settled,
            "fixture did not reach target within 100000 turns / cooperative 900 seconds"
        );
        anyhow::ensure!(after.logical_current_bytes <= before.logical_current_bytes / 2 * 9 / 10);
        println!(
            "remaining_gc_nominations={} recovery_waits={recovery_waits}",
            gc_backlog(&db).await?
        );
        let mut retry_events = 0;
        let retry_sample = (events_count / 64).clamp(1, 32);
        let mut suppressed_bytes = 0u64;
        for event in &events {
            if db
                .get_event_content_state(event.event.event_id.to_short())
                .await
                != Some(crate::EventContentState::Pruned)
            {
                continue;
            }
            for _ in 0..3 {
                anyhow::ensure!(matches!(
                    db.prepare_payload_acquisition(&event.event).await?,
                    crate::PayloadReservationOutcome::Unneeded
                ));
                suppressed_bytes += event.content.as_ref().unwrap().len() as u64;
            }
            retry_events += 1;
            if retry_events == retry_sample {
                break;
            }
        }
        anyhow::ensure!(retry_events == retry_sample);
        anyhow::ensure!(after == db.get_payload_usage().await?.unwrap());
        anyhow::ensure!(db.payload_admission_observation().buffers == 0);
        println!(
            "terminal_retry_events={retry_events} attempts={} suppressed_declared_bytes={suppressed_bytes} remaining_guarded_buffers=0",
            retry_events * 3
        );
        println!(
            "settled usage={after:?} logical_removed={} unique_removed={} file_bytes={}",
            before.logical_current_bytes - after.logical_current_bytes,
            before.unique_stored_bytes - after.unique_stored_bytes,
            std::fs::metadata(&path)?.len()
        );
    }
    Ok(())
}

async fn gc_backlog(db: &Database) -> crate::DbResult<usize> {
    db.read_with(|tx| {
        Ok(tx
            .open_table(&crate::content_quota_gc::TABLE)?
            .range(..)?
            .count())
    })
    .await
}
