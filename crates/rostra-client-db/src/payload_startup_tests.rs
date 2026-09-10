use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use rostra_core::Timestamp;
use rostra_core::event::content_kind::{EventContentKind as _, SocialPost};
use rostra_core::event::{Event, EventExt as _, EventKind, VerifiedEvent, VerifiedEventContent};
use rostra_core::id::{RostraIdSecretKey, ToShort as _};
use rostra_core::retention::RetentionPolicy;

use crate::{
    Database, DbError, DryRunLimits, PayloadAccount, PayloadAccountAttachError,
    PayloadAdmissionConfig, PayloadAdmissionLimits, PayloadIngestOutcome, PayloadRetentionConfig,
    PayloadRuntimeLimits,
};

fn admission(bytes: u64) -> PayloadAdmissionConfig {
    PayloadAdmissionConfig::new(PayloadAdmissionLimits {
        database_bytes: NonZeroU64::new(bytes).unwrap(),
        author_bytes: NonZeroU64::new(bytes).unwrap(),
        overrides: Default::default(),
        in_flight_count: NonZeroUsize::new(8).unwrap(),
        in_flight_bytes: NonZeroU64::new(100_000).unwrap(),
    })
    .unwrap()
}

fn limits() -> PayloadRuntimeLimits {
    PayloadRuntimeLimits {
        operations: NonZeroUsize::new(32).unwrap(),
        bytes: 100_000,
        gc_bytes: 100_000,
        time: Duration::from_millis(20),
    }
}

fn policy() -> RetentionPolicy {
    RetentionPolicy::new(1, 1, 0, 0, 1, 0).unwrap()
}

fn post(author: RostraIdSecretKey, time: u64, text: &str) -> VerifiedEventContent {
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

#[tokio::test(flavor = "multi_thread")]
async fn startup_enforce_initializes_absent_indexes_prunes_and_preserves_terminal_ingress()
-> anyhow::Result<()> {
    let holder = RostraIdSecretKey::generate().id();
    let author = RostraIdSecretKey::generate();
    let mut db = Database::new_in_memory(holder).await?;
    let oldest = post(author, 1, "oldest");
    let newest = post(author, 2, "newest");
    let bytes = u64::from(newest.content_len());
    db.try_process_event_with_content(&oldest).await?;
    db.try_process_event_with_content(&newest).await?;
    let account = PayloadAccount::configured(
        holder,
        PayloadRetentionConfig::Enforce {
            policy: policy(),
            admission: admission(bytes + 1),
            limits: limits(),
        },
    )?;
    db.attach_payload_account(&account)?;
    assert!(db.has_payload_retention_runtime());
    assert_eq!(
        db.attach_payload_account(&PayloadAccount::disabled(holder)),
        Err(PayloadAccountAttachError::DatabaseAlreadyAttached)
    );
    let db = Arc::new(db);
    let owned = db.clone();
    let worker = tokio::spawn(async move { owned.run_payload_retention().await });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                db.get_event_content_state(oldest.event_id().to_short())
                    .await,
                Some(crate::EventContentState::Pruned)
            ) {
                break;
            }
            assert!(
                !worker.is_finished(),
                "configured worker exited before maintenance"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert_eq!(
        db.try_process_admitted_event_content(&oldest, None).await?,
        PayloadIngestOutcome::Unchanged
    );
    assert!(matches!(
        db.prepare_payload_acquisition(&oldest.event).await?,
        crate::PayloadReservationOutcome::Unneeded
    ));
    assert_eq!(db.payload_admission_usage().buffers, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_dry_run_is_disabled_admission_and_observation_only() -> anyhow::Result<()> {
    let holder = RostraIdSecretKey::generate().id();
    let author = RostraIdSecretKey::generate();
    let mut db = Database::new_in_memory(holder).await?;
    let retained = post(author, 1, "retained");
    db.try_process_event_with_content(&retained).await?;
    let before = db.get_payload_usage().await?;
    let account = PayloadAccount::configured(
        holder,
        PayloadRetentionConfig::DryRun {
            policy: policy(),
            admission: admission(1),
            limits: DryRunLimits {
                events: NonZeroUsize::new(100).unwrap(),
                authors: NonZeroUsize::new(100).unwrap(),
                logical_bytes: 100_000,
                time: Duration::from_secs(1),
            },
        },
    )?;
    assert!(account.reserve_payload_allocation(u64::MAX)?.is_none());
    db.attach_payload_account(&account)?;
    let db = Arc::new(db);
    let owned = db.clone();
    let worker = tokio::spawn(async move { owned.run_payload_retention().await });
    tokio::time::timeout(Duration::from_secs(10), async {
        while db.payload_retention_forecast().is_none() {
            assert!(!worker.is_finished());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert_eq!(db.get_payload_usage().await?, before);
    assert!(
        db.get_event_content(retained.event_id().to_short())
            .await
            .is_some()
    );
    let incoming = post(author, 2, "above forecast ceiling");
    assert!(matches!(
        db.prepare_payload_acquisition(&incoming.event).await?,
        crate::PayloadReservationOutcome::Disabled
    ));
    assert_eq!(
        db.try_process_admitted_event_content(&incoming, None)
            .await?,
        PayloadIngestOutcome::Processed
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_external_preallocated_db_inputs_never_bypass_logical_admission()
-> anyhow::Result<()> {
    let holder = RostraIdSecretKey::generate().id();
    let mut db = Database::new_in_memory(holder).await?;
    db.attach_payload_account(&PayloadAccount::configured(
        holder,
        PayloadRetentionConfig::Enforce {
            policy: policy(),
            admission: admission(1),
            limits: limits(),
        },
    )?)?;
    while !db
        .rebuild_payload_accounting(NonZeroUsize::new(32).unwrap())
        .await?
        .ready
    {}
    let content = post(
        RostraIdSecretKey::generate(),
        1,
        "already allocated outside DB",
    );
    assert!(matches!(
        db.try_process_event_with_content(&content).await,
        Err(DbError::PayloadAdmissionPaused { .. })
    ));
    assert!(matches!(
        db.try_process_event_content(&content).await,
        Err(DbError::PayloadAdmissionPaused { .. })
    ));
    assert!(matches!(
        db.try_process_admitted_event_content(&content, None)
            .await?,
        PayloadIngestOutcome::Deferred(_)
    ));
    assert!(
        db.get_event_content(content.event_id().to_short())
            .await
            .is_none()
    );
    assert_eq!(db.payload_admission_usage().buffers, 0);
    Ok(())
}

#[test]
fn startup_invalid_allowances_fail_before_account_publication() {
    let id = RostraIdSecretKey::generate().id();
    for bad in [
        PayloadRuntimeLimits {
            bytes: 0,
            ..limits()
        },
        PayloadRuntimeLimits {
            gc_bytes: 0,
            ..limits()
        },
        PayloadRuntimeLimits {
            time: Duration::ZERO,
            ..limits()
        },
        PayloadRuntimeLimits {
            operations: NonZeroUsize::new(4097).unwrap(),
            ..limits()
        },
    ] {
        assert!(matches!(
            PayloadAccount::configured(
                id,
                PayloadRetentionConfig::Enforce {
                    policy: policy(),
                    admission: admission(1),
                    limits: bad,
                }
            ),
            Err(PayloadAccountAttachError::InvalidConfiguration)
        ));
    }
}
