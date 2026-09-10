use std::collections::BTreeMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::time::{Duration, Instant};

use rostra_core::Timestamp;
use rostra_core::event::content_kind::{EventContentKind as _, SocialPost};
use rostra_core::event::{Event, EventExt as _, EventKind, VerifiedEvent, VerifiedEventContent};
use rostra_core::id::{RostraIdSecretKey, ToShort as _};
use rostra_core::retention::RetentionPolicy;

use crate::payload_demand::{DemandRegistration, DemandStep, PayloadDemand};
use crate::{
    Database, PayloadAdmissionConfig, PayloadAdmissionLimits, PayloadReservationOutcome,
    RetentionClock, RetentionGeneration,
};

fn limit(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn policy() -> RetentionPolicy {
    RetentionPolicy::new(1, 1, 0, 0, 1, 0).unwrap()
}

fn post(secret: RostraIdSecretKey, time: u64, text: &str) -> VerifiedEventContent {
    let raw = SocialPost::new_text(text.to_owned(), None, Default::default())
        .serialize_cbor()
        .unwrap();
    let signed = Event::builder_raw_content()
        .author(secret.id())
        .kind(EventKind::SOCIAL_POST)
        .timestamp(Timestamp::from(time).to_offset_date_time().unwrap())
        .content(&raw)
        .build()
        .signed_by(secret);
    VerifiedEventContent::assume_verified(
        VerifiedEvent::verify_signed(secret.id(), signed).unwrap(),
        raw,
    )
}

async fn ingest(
    db: &Database,
    post: &VerifiedEventContent,
    materialize: bool,
) -> anyhow::Result<()> {
    ingest_at(db, post, materialize, 100).await
}

async fn ingest_at(
    db: &Database,
    post: &VerifiedEventContent,
    materialize: bool,
    now: u64,
) -> anyhow::Result<()> {
    db.write_with(|tx| {
        db.process_event_tx(&post.event, Timestamp::from(now), tx)?;
        if materialize {
            db.process_event_content_tx(post, Timestamp::from(now), tx)?;
        }
        Ok(())
    })
    .await?;
    Ok(())
}

async fn one_row_step(
    db: &Database,
    generation: RetentionGeneration,
    now: u64,
    bytes: u64,
) -> anyhow::Result<DemandStep> {
    Ok(db
        .preempt_payload_demand_with(
            generation,
            || Timestamp::from(now),
            limit(1),
            bytes,
            Instant::now() + Duration::from_secs(5),
            || Ok(()),
        )
        .await?)
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_alternate_author_progresses_past_exhausted_ranked_plan() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let b = RostraIdSecretKey::generate();
    let high_victim = post(a, 95, "same");
    let high = post(a, 90, "same");
    let low_victim = post(b, 1, "same");
    let low = post(b, 50, "same");
    for event in [&high_victim, &low_victim] {
        ingest(&db, event, true).await?;
    }
    for event in [&high, &low] {
        ingest(&db, event, false).await?;
    }
    let bytes = u64::from(low.content_len());
    let generation = configure(&db, 10000, bytes + 1, 5, 10000).await?;
    let _high = demand(&db, &high, generation, 100).await?;
    let low_owner = demand(&db, &low, generation, 100).await?;
    assert_eq!(
        one_row_step(&db, generation, 100, bytes).await?,
        DemandStep::Continue
    );
    assert_eq!(
        one_row_step(&db, generation, 101, bytes).await?,
        DemandStep::Pruned {
            demand: low.event_id(),
            victim: low_victim.event_id(),
            bytes
        },
    );
    assert_eq!(
        step(&db, generation, 101).await?,
        DemandStep::Fits(low.event_id())
    );
    drop(low_owner);
    assert_eq!(
        step(&db, generation, 101).await?,
        DemandStep::NoVictim {
            retry_at: Timestamp::from(130),
        }
    );
    assert!(
        db.get_event_content(high_victim.event_id().to_short())
            .await
            .is_some()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_cursor_crosses_future_prefix_without_repeating_it() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let first = post(a, 1, "same");
    let second = post(a, 2, "same");
    let ready = post(a, 3, "same");
    let incoming = post(a, 89, "same");
    ingest_at(&db, &first, true, 95).await?;
    ingest_at(&db, &second, true, 96).await?;
    ingest_at(&db, &ready, true, 80).await?;
    ingest_at(&db, &incoming, false, 89).await?;
    let bytes = u64::from(ready.content_len());
    let generation = configure(&db, 3 * bytes + 1, 10000, 5, 10000).await?;
    let _owner = demand(&db, &incoming, generation, 90).await?;
    assert_eq!(
        one_row_step(&db, generation, 90, bytes).await?,
        DemandStep::Continue
    );
    assert_eq!(
        one_row_step(&db, generation, 91, bytes).await?,
        DemandStep::Continue
    );
    assert_eq!(
        one_row_step(&db, generation, 92, bytes).await?,
        DemandStep::Pruned {
            demand: incoming.event_id(),
            victim: ready.event_id(),
            bytes
        },
    );
    assert!(
        db.get_event_content(first.event_id().to_short())
            .await
            .is_some()
    );
    assert!(
        db.get_event_content(second.event_id().to_short())
            .await
            .is_some()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_cursor_restarts_at_skipped_eligibility_deadline() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let first = post(a, 1, "same");
    let later = post(a, 2, "same");
    let incoming = post(a, 89, "same");
    ingest_at(&db, &first, true, 95).await?;
    ingest_at(&db, &later, true, 80).await?;
    ingest_at(&db, &incoming, false, 89).await?;
    let bytes = u64::from(later.content_len());
    let generation = configure(&db, 2 * bytes + 1, 10000, 5, 10000).await?;
    let _owner = demand(&db, &incoming, generation, 90).await?;
    assert_eq!(
        one_row_step(&db, generation, 90, bytes).await?,
        DemandStep::Continue
    );
    assert_eq!(
        one_row_step(&db, generation, 95, bytes).await?,
        DemandStep::Pruned {
            demand: incoming.event_id(),
            victim: first.event_id(),
            bytes
        },
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_cursor_restarts_after_promotion_before_frontier() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let future = post(a, 1, "same");
    let later = post(a, 2, "same");
    let new_minimum = post(a, 0, "same");
    let incoming = post(a, 89, "same");
    ingest_at(&db, &future, true, 95).await?;
    ingest_at(&db, &later, true, 80).await?;
    ingest_at(&db, &incoming, false, 89).await?;
    let bytes = u64::from(later.content_len());
    let generation = configure(&db, 2 * bytes + 1, 10000, 5, 10000).await?;
    let _owner = demand(&db, &incoming, generation, 90).await?;
    assert_eq!(
        one_row_step(&db, generation, 90, bytes).await?,
        DemandStep::Continue
    );
    let config = db.payload_admission.state.lock().unwrap().config.take();
    ingest_at(&db, &new_minimum, true, 80).await?;
    db.payload_admission.state.lock().unwrap().config = config;
    assert_eq!(step(&db, generation, 90).await?, DemandStep::NotReady);
    db.promote_retention_grace(RetentionClock::Trusted(Timestamp::from(90)), limit(10))
        .await?;
    assert_eq!(
        one_row_step(&db, generation, 90, bytes).await?,
        DemandStep::Pruned {
            demand: incoming.event_id(),
            victim: new_minimum.event_id(),
            bytes
        },
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_exhausted_cursor_wakes_for_clock_and_rewinds_on_rollback() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let first = post(a, 1, "same");
    let second = post(a, 2, "same");
    let incoming = post(a, 79, "same");
    ingest_at(&db, &first, true, 95).await?;
    ingest_at(&db, &second, true, 96).await?;
    ingest_at(&db, &incoming, false, 79).await?;
    let bytes = u64::from(first.content_len());
    let generation = configure(&db, 2 * bytes + 1, 10000, 5, 10000).await?;
    let _owner = demand(&db, &incoming, generation, 80).await?;
    assert_eq!(
        one_row_step(&db, generation, 90, bytes).await?,
        DemandStep::Continue
    );
    let first_cursor = db.payload_admission.demands.lock().unwrap().entries[&incoming.event_id()]
        .scan
        .unwrap()
        .after;
    assert_eq!(
        one_row_step(&db, generation, 89, bytes).await?,
        DemandStep::Continue
    );
    assert_eq!(
        db.payload_admission.demands.lock().unwrap().entries[&incoming.event_id()]
            .scan
            .unwrap()
            .after,
        first_cursor
    );
    assert_eq!(
        one_row_step(&db, generation, 90, bytes).await?,
        DemandStep::Continue
    );
    for _ in 0..2 {
        assert_eq!(
            one_row_step(&db, generation, 90, bytes).await?,
            DemandStep::NoVictim {
                retry_at: Timestamp::from(95),
            }
        );
    }
    assert_eq!(
        one_row_step(&db, generation, 95, bytes).await?,
        DemandStep::Pruned {
            demand: incoming.event_id(),
            victim: first.event_id(),
            bytes
        },
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_partial_plan_keeps_priority_until_cancellation_or_expiry() -> anyhow::Result<()> {
    for cancel in [false, true] {
        let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
        let a = RostraIdSecretKey::generate();
        let first = post(a, 1, "same");
        let second = post(a, 2, "same");
        let incoming = post(a, 50, "a somewhat longer post");
        let higher = post(a, 90, "same");
        ingest(&db, &first, true).await?;
        ingest(&db, &second, true).await?;
        ingest(&db, &incoming, false).await?;
        let bytes = u64::from(first.content_len());
        assert!(bytes + 1 < u64::from(incoming.content_len()));
        assert!(u64::from(incoming.content_len()) <= 2 * bytes + 1);
        let generation = configure(&db, 2 * bytes + 1, 10000, 5, 10000).await?;
        let owner = demand(&db, &incoming, generation, 100).await?;
        assert_eq!(
            one_row_step(&db, generation, 100, bytes).await?,
            DemandStep::Pruned {
                demand: incoming.event_id(),
                victim: first.event_id(),
                bytes
            },
        );
        // Another plan fits now, but must not replace the partial plan.
        ingest(&db, &higher, false).await?;
        // Registration requires pressure, so temporarily reserve the remaining
        // gap, register, then release without changing candidate membership.
        let reservation = match db.reserve_payload(&higher.event).await? {
            PayloadReservationOutcome::Reserved(owner) => owner,
            other => panic!("{other:?}"),
        };
        let another = post(a, 91, "same");
        ingest(&db, &another, false).await?;
        let _another_owner = demand(&db, &another, generation, 101).await?;
        drop(reservation);
        if cancel {
            drop(owner);
            let _replacement = demand(&db, &incoming, generation, 101).await?;
            assert_eq!(
                step(&db, generation, 101).await?,
                DemandStep::Fits(another.event_id())
            );
        } else {
            assert_eq!(
                one_row_step(&db, generation, 101, bytes).await?,
                DemandStep::Pruned {
                    demand: incoming.event_id(),
                    victim: second.event_id(),
                    bytes
                },
            );
            assert_eq!(
                step(&db, generation, 101).await?,
                DemandStep::Fits(incoming.event_id())
            );
            assert_eq!(
                step(&db, generation, 130).await?,
                DemandStep::Fits(another.event_id())
            );
            drop(owner);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_byte_blocked_plan_yields_to_another_author_and_larger_budget() -> anyhow::Result<()>
{
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let b = RostraIdSecretKey::generate();
    let large = post(a, 1, "a much larger retained payload");
    let small = post(b, 2, "same");
    let high = post(a, 90, "same");
    let low = post(b, 80, "same");
    for event in [&large, &small] {
        ingest(&db, event, true).await?;
    }
    for event in [&high, &low] {
        ingest(&db, event, false).await?;
    }
    let bytes = u64::from(small.content_len());
    let large_bytes = u64::from(large.content_len());
    assert!(large_bytes > bytes);
    let generation = configure(&db, 10000, bytes + 1, 5, 10000).await?;
    let _high = demand(&db, &high, generation, 100).await?;
    let low_owner = demand(&db, &low, generation, 100).await?;
    assert_eq!(
        one_row_step(&db, generation, 100, bytes).await?,
        DemandStep::Continue
    );
    assert_eq!(
        one_row_step(&db, generation, 100, bytes).await?,
        DemandStep::Pruned {
            demand: low.event_id(),
            victim: small.event_id(),
            bytes
        },
    );
    assert_eq!(
        step(&db, generation, 100).await?,
        DemandStep::Fits(low.event_id())
    );
    drop(low_owner);
    for _ in 0..2 {
        assert_eq!(
            one_row_step(&db, generation, 100, bytes).await?,
            DemandStep::Bounded
        );
    }
    assert_eq!(
        one_row_step(&db, generation, 100, large_bytes).await?,
        DemandStep::Pruned {
            demand: high.event_id(),
            victim: large.event_id(),
            bytes: large_bytes
        },
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_cursor_ignores_noop_refresh_but_resets_after_aborted_mutation() -> anyhow::Result<()>
{
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let future = post(a, 1, "same");
    let later = post(a, 2, "same");
    let new_minimum = post(a, 0, "same");
    let incoming = post(a, 79, "same");
    ingest_at(&db, &future, true, 95).await?;
    ingest_at(&db, &later, true, 80).await?;
    ingest_at(&db, &incoming, false, 79).await?;
    let bytes = u64::from(later.content_len());
    let generation = configure(&db, 2 * bytes + 1, 10000, 5, 10000).await?;
    let _owner = demand(&db, &incoming, generation, 80).await?;
    assert_eq!(
        one_row_step(&db, generation, 90, bytes).await?,
        DemandStep::Continue
    );
    let revision = db
        .payload_admission
        .retention_revision
        .load(std::sync::atomic::Ordering::Relaxed);
    ingest_at(&db, &future, true, 90).await?;
    assert_eq!(
        db.payload_admission
            .retention_revision
            .load(std::sync::atomic::Ordering::Relaxed),
        revision
    );
    let config = db.payload_admission.state.lock().unwrap().config.take();
    let aborted: crate::DbResult<()> = db
        .write_with(|tx| {
            db.process_event_tx(&new_minimum.event, Timestamp::from(80), tx)?;
            db.process_event_content_tx(&new_minimum, Timestamp::from(80), tx)?;
            Err(crate::DbError::PayloadAccountingInvariant)
        })
        .await;
    db.payload_admission.state.lock().unwrap().config = config;
    assert!(aborted.is_err());
    assert!(
        db.payload_admission
            .retention_revision
            .load(std::sync::atomic::Ordering::Relaxed)
            > revision
    );
    assert!(
        db.get_event_content(new_minimum.event_id().to_short())
            .await
            .is_none()
    );
    // The aborted insert forces a conservative rewind, not a skipped prefix.
    assert_eq!(
        one_row_step(&db, generation, 90, bytes).await?,
        DemandStep::Continue
    );
    assert_eq!(
        one_row_step(&db, generation, 90, bytes).await?,
        DemandStep::Pruned {
            demand: incoming.event_id(),
            victim: later.event_id(),
            bytes
        },
    );
    Ok(())
}

async fn configure(
    db: &Database,
    global: u64,
    author: u64,
    count: usize,
    bytes: u64,
) -> anyhow::Result<RetentionGeneration> {
    let config = PayloadAdmissionConfig::new(PayloadAdmissionLimits {
        database_bytes: NonZeroU64::new(global).unwrap(),
        author_bytes: NonZeroU64::new(author).unwrap(),
        overrides: BTreeMap::new(),
        in_flight_count: limit(count),
        in_flight_bytes: NonZeroU64::new(bytes).unwrap(),
    })
    .unwrap();
    db.write_with(|_| {
        db.payload_admission.state.lock().unwrap().config = Some(config);
        Ok(())
    })
    .await?;
    while !db.rebuild_payload_accounting(limit(10)).await?.ready {}
    db.configure_retention_index(policy()).await?;
    while !db.rebuild_retention_index(limit(10)).await?.unwrap().ready {}
    while db
        .promote_retention_grace(RetentionClock::Trusted(Timestamp::from(100)), limit(10))
        .await?
        .unwrap()
        .visited
        == 10
    {}
    Ok(RetentionGeneration::new(policy(), db.self_id))
}

async fn demand(
    db: &Database,
    event: &VerifiedEventContent,
    generation: RetentionGeneration,
    now: u64,
) -> anyhow::Result<PayloadDemand> {
    match db
        .register_payload_demand_with(event.event_id(), generation, || Timestamp::from(now))
        .await?
    {
        DemandRegistration::Pending(owner) => Ok(owner),
        other => anyhow::bail!("expected pending, got {other:?}"),
    }
}

async fn step(
    db: &Database,
    generation: RetentionGeneration,
    now: u64,
) -> anyhow::Result<DemandStep> {
    Ok(db
        .preempt_payload_demand_with(
            generation,
            || Timestamp::from(now),
            limit(10),
            10000,
            Instant::now() + Duration::from_secs(5),
            || Ok(()),
        )
        .await?)
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_cap_minus_one_stops_at_one_live_plan_without_aggregate_eviction()
-> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let old = post(a, 1, "same");
    let old2 = post(a, 2, "same");
    let incoming = post(a, 4, "same");
    let second = post(RostraIdSecretKey::generate(), 3, "same");
    for event in [&old, &old2] {
        ingest(&db, event, true).await?;
    }
    for event in [&incoming, &second] {
        ingest(&db, event, false).await?;
    }
    let bytes = u64::from(old.content_len());
    let generation = configure(&db, 2 * bytes + 1, 10000, 5, 10000).await?;
    assert!(matches!(
        db.reserve_payload(&incoming.event).await?,
        PayloadReservationOutcome::Deferred(_)
    ));
    let _first = demand(&db, &incoming, generation, 100).await?;
    let _second = demand(&db, &second, generation, 100).await?;
    assert_eq!(
        step(&db, generation, 100).await?,
        DemandStep::Pruned {
            demand: incoming.event_id(),
            victim: old.event_id(),
            bytes,
        }
    );
    // Do not sum two demands and evict old2 while the first intent can now fit.
    assert_eq!(
        step(&db, generation, 100).await?,
        DemandStep::Fits(incoming.event_id())
    );
    let reservation = match db.reserve_payload(&incoming.event).await? {
        PayloadReservationOutcome::Reserved(owner) => owner,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        step(&db, generation, 100).await?,
        DemandStep::Pruned {
            demand: second.event_id(),
            victim: old2.event_id(),
            bytes,
        }
    );
    drop(reservation);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_deduplicates_expires_and_late_drop_cannot_cancel_replacement() -> anyhow::Result<()>
{
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let old = post(a, 1, "same");
    let incoming = post(a, 2, "same");
    ingest(&db, &old, true).await?;
    ingest(&db, &incoming, false).await?;
    let generation = configure(&db, u64::from(old.content_len()) + 1, 10000, 1, 10000).await?;
    let first = demand(&db, &incoming, generation, 100).await?;
    let duplicate = demand(&db, &incoming, generation, 120).await?;
    assert_eq!(
        db.payload_admission.demands.lock().unwrap().usage(),
        (1, u64::from(incoming.content_len()))
    );
    drop(first);
    assert_eq!(step(&db, generation, 130).await?, DemandStep::Idle);
    let replacement = demand(&db, &incoming, generation, 130).await?;
    drop(duplicate);
    assert_eq!(db.payload_admission.demands.lock().unwrap().usage().0, 1);
    drop(replacement);
    assert_eq!(step(&db, generation, 130).await?, DemandStep::Idle);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_bounds_do_not_charge_buffers_or_reservations() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let old = post(a, 1, "same");
    let incoming = post(a, 2, "same");
    let extra = post(a, 3, "same");
    ingest(&db, &old, true).await?;
    ingest(&db, &incoming, false).await?;
    ingest(&db, &extra, false).await?;
    let bytes = u64::from(old.content_len());
    for (count, budget) in [(1, 10000), (3, bytes)] {
        let generation = configure(&db, bytes + 1, 10000, count, budget).await?;
        let owner = demand(&db, &incoming, generation, 100).await?;
        assert!(matches!(
            db.register_payload_demand_with(extra.event_id(), generation, || Timestamp::from(100))
                .await?,
            DemandRegistration::Overloaded
        ));
        let state = db.payload_admission.state.lock().unwrap();
        assert_eq!(
            (state.events.len(), state.buffers, state.buffer_bytes),
            (0, 0, 0)
        );
        drop(state);
        drop(owner);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_policy_replacement_and_cancel_cannot_authorize_stale_prune() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let old = post(a, 1, "same");
    let incoming = post(a, 2, "same");
    ingest(&db, &old, true).await?;
    ingest(&db, &incoming, false).await?;
    let generation = configure(&db, u64::from(old.content_len()) + 1, 10000, 5, 10000).await?;
    let owner = demand(&db, &incoming, generation, 100).await?;
    db.configure_retention_index(RetentionPolicy::new(1, 2, 0, 0, 1, 0).unwrap())
        .await?;
    assert_eq!(step(&db, generation, 100).await?, DemandStep::Idle);
    let generation = configure(&db, u64::from(old.content_len()) + 1, 10000, 5, 10000).await?;
    let next = demand(&db, &incoming, generation, 100).await?;
    drop(owner);
    drop(next);
    assert_eq!(step(&db, generation, 100).await?, DemandStep::Idle);
    assert!(
        db.get_event_content(old.event_id().to_short())
            .await
            .is_some()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_author_first_never_uses_other_author_to_relieve_author_pressure()
-> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let b = RostraIdSecretKey::generate();
    let global_min = post(b, 1, "same");
    let author_min = post(a, 2, "same");
    let incoming = post(a, 3, "same");
    for event in [&global_min, &author_min] {
        ingest(&db, event, true).await?;
    }
    ingest(&db, &incoming, false).await?;
    let bytes = u64::from(author_min.content_len());
    let generation = configure(&db, 2 * bytes + 1, bytes + 1, 5, 10000).await?;
    let _owner = demand(&db, &incoming, generation, 100).await?;
    assert_eq!(
        step(&db, generation, 100).await?,
        DemandStep::Pruned {
            demand: incoming.event_id(),
            victim: author_min.event_id(),
            bytes,
        }
    );
    assert_eq!(
        step(&db, generation, 100).await?,
        DemandStep::Fits(incoming.event_id())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_no_victim_and_fixed_time_due_prefix_and_work_bounds() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let old = post(a, 2, "same");
    let incoming = post(a, 1, "same");
    ingest(&db, &old, true).await?;
    ingest(&db, &incoming, false).await?;
    let bytes = u64::from(old.content_len());
    let generation = configure(&db, bytes + 1, 10000, 5, 10000).await?;
    let owner = demand(&db, &incoming, generation, 100).await?;
    assert_eq!(
        step(&db, generation, 100).await?,
        DemandStep::NoVictim {
            retry_at: Timestamp::from(130),
        }
    );
    drop(owner);
    let newer = post(a, 3, "same");
    ingest(&db, &newer, false).await?;
    let _owner = demand(&db, &newer, generation, 100).await?;
    assert_eq!(
        db.preempt_payload_demand_with(
            generation,
            || Timestamp::from(100),
            limit(1),
            bytes - 1,
            Instant::now() + Duration::from_secs(5),
            || Ok(())
        )
        .await?,
        DemandStep::Bounded
    );
    assert_eq!(
        db.preempt_payload_demand_with(
            generation,
            || Timestamp::from(100),
            limit(1),
            bytes,
            Instant::now(),
            || Ok(())
        )
        .await?,
        DemandStep::Bounded
    );
    // A newly materialized row is due but unpromoted: backfill ready is not
    // sufficient even though an older promoted candidate exists.
    let lower = post(a, 0, "same");
    let config = db.payload_admission.state.lock().unwrap().config.take();
    ingest(&db, &lower, true).await?;
    db.payload_admission.state.lock().unwrap().config = config;
    assert_eq!(step(&db, generation, 100).await?, DemandStep::NotReady);
    db.promote_retention_grace(RetentionClock::Trusted(Timestamp::from(100)), limit(1))
        .await?;
    assert_eq!(
        step(&db, generation, 100).await?,
        DemandStep::Pruned {
            demand: newer.event_id(),
            victim: lower.event_id(),
            bytes,
        }
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_cancellation_and_reservation_release_serialize_with_prune() -> anyhow::Result<()> {
    let db =
        std::sync::Arc::new(Database::new_in_memory(RostraIdSecretKey::generate().id()).await?);
    let a = RostraIdSecretKey::generate();
    let b = RostraIdSecretKey::generate();
    let old = post(a, 1, "same");
    let incoming = post(a, 3, "same");
    let reserved = post(b, 2, "same");
    ingest(&db, &old, true).await?;
    ingest(&db, &incoming, false).await?;
    ingest(&db, &reserved, false).await?;
    let bytes = u64::from(old.content_len());
    let generation = configure(&db, 2 * bytes + 1, 10000, 5, 10000).await?;
    let reservation = match db.reserve_payload(&reserved.event).await? {
        PayloadReservationOutcome::Reserved(owner) => owner,
        other => panic!("{other:?}"),
    };
    let owner = demand(&db, &incoming, generation, 100).await?;
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let mut release_thread = None;
    let db2 = db.clone();
    let outcome = db
        .preempt_payload_demand_with(
            generation,
            || Timestamp::from(100),
            limit(10),
            10000,
            Instant::now() + Duration::from_secs(5),
            || {
                release_thread = Some(std::thread::spawn(move || {
                    // Removal-sensitive assertion: both owner Drop implementations
                    // must arbitrate against the mutex held through this reducer.
                    assert!(matches!(
                        db2.payload_admission.demands.try_lock(),
                        Err(std::sync::TryLockError::WouldBlock)
                    ));
                    started_tx.send(()).unwrap();
                    drop(reservation);
                    drop(owner);
                    finished_tx.send(()).unwrap();
                }));
                started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!(finished_rx.try_recv().is_err());
                Ok(())
            },
        )
        .await?;
    release_thread.unwrap().join().unwrap();
    finished_rx.recv_timeout(Duration::from_secs(5))?;
    assert_eq!(
        outcome,
        DemandStep::Pruned {
            demand: incoming.event_id(),
            victim: old.event_id(),
            bytes,
        }
    );
    assert_eq!(step(&db, generation, 100).await?, DemandStep::Idle);
    assert_eq!(db.payload_admission.state.lock().unwrap().events.len(), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_rechecks_reservation_release_config_and_backwards_time() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let old = post(a, 1, "same");
    let incoming = post(a, 3, "same");
    let reserved = post(a, 2, "same");
    ingest(&db, &old, true).await?;
    ingest(&db, &incoming, false).await?;
    ingest(&db, &reserved, false).await?;
    let bytes = u64::from(old.content_len());
    let generation = configure(&db, 2 * bytes + 1, 10000, 5, 10000).await?;
    let reservation = match db.reserve_payload(&reserved.event).await? {
        PayloadReservationOutcome::Reserved(owner) => owner,
        other => panic!("{other:?}"),
    };
    let _owner = demand(&db, &incoming, generation, 100).await?;
    drop(reservation);
    assert_eq!(
        step(&db, generation, 100).await?,
        DemandStep::Fits(incoming.event_id())
    );
    assert_eq!(step(&db, generation, 99).await?, DemandStep::Idle);

    let generation = configure(&db, bytes + 1, 10000, 5, 10000).await?;
    let _owner = demand(&db, &incoming, generation, 100).await?;
    db.write_with(|_| {
        db.payload_admission.state.lock().unwrap().config = None;
        Ok(())
    })
    .await?;
    assert_eq!(step(&db, generation, 100).await?, DemandStep::NotReady);
    assert_eq!(step(&db, generation, 100).await?, DemandStep::Idle);
    assert!(
        db.get_event_content(old.event_id().to_short())
            .await
            .is_some()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_protected_overload_and_impossible_payload_do_not_prune() -> anyhow::Result<()> {
    let holder = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(holder.id()).await?;
    let a = RostraIdSecretKey::generate();
    let protected = post(holder, 1, "same");
    let incoming = post(a, 2, "same");
    let oversized = post(
        a,
        3,
        "a payload larger than the whole configured logical capacity",
    );
    ingest(&db, &protected, true).await?;
    ingest(&db, &incoming, false).await?;
    ingest(&db, &oversized, false).await?;
    let bytes = u64::from(protected.content_len());
    let generation = configure(&db, bytes + 1, 10000, 5, 10000).await?;
    let _owner = demand(&db, &incoming, generation, 100).await?;
    for _ in 0..3 {
        assert_eq!(
            step(&db, generation, 100).await?,
            DemandStep::NoVictim {
                retry_at: Timestamp::from(130),
            }
        );
    }
    assert!(matches!(
        db.register_payload_demand_with(oversized.event_id(), generation, || Timestamp::from(100))
            .await?,
        DemandRegistration::Overloaded,
    ));
    assert!(
        db.get_event_content(protected.event_id().to_short())
            .await
            .is_some()
    );
    assert!(
        db.write_with(|tx| Database::payload_is_missing_tx(tx, oversized.event_id().to_short()))
            .await?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_clock_is_sampled_inside_writer_and_cancellation_boundary() -> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let old = post(a, 1, "same");
    let incoming = post(a, 2, "same");
    ingest(&db, &old, true).await?;
    ingest(&db, &incoming, false).await?;
    let generation = configure(&db, u64::from(old.content_len()) + 1, 10000, 5, 10000).await?;
    let locked_time = |time| {
        assert!(matches!(
            db.write_and_publish_lock.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        assert!(matches!(
            db.payload_admission.demands.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        Timestamp::from(time)
    };
    let owner = db
        .register_payload_demand_with(incoming.event_id(), generation, || locked_time(100))
        .await?;
    assert!(matches!(owner, DemandRegistration::Pending(_)));
    assert_eq!(
        db.preempt_payload_demand_with(
            generation,
            || locked_time(130),
            limit(10),
            10000,
            Instant::now() + Duration::from_secs(5),
            || panic!("expired demand must not prune"),
        )
        .await?,
        DemandStep::Idle
    );
    assert!(
        db.get_event_content(old.event_id().to_short())
            .await
            .is_some()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_checked_reducer_failure_rolls_back_and_preserves_live_intent() -> anyhow::Result<()>
{
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let old = post(a, 1, "same");
    let incoming = post(a, 2, "same");
    ingest(&db, &old, true).await?;
    ingest(&db, &incoming, false).await?;
    let bytes = u64::from(old.content_len());
    let generation = configure(&db, bytes + 1, 10000, 5, 10000).await?;
    let _owner = demand(&db, &incoming, generation, 100).await?;
    let usage = db.get_payload_usage().await?;
    let mut notifications = db.quota_pruned_subscribe();
    db.write_with(|tx| {
        let mut table = tx.open_table(&crate::ids_data_usage::TABLE)?;
        let mut author = table.get(&a.id())?.unwrap().value_try()?;
        author.current_content_size = 0;
        table.insert(&a.id(), &author)?;
        Ok(())
    })
    .await?;
    assert!(step(&db, generation, 100).await.is_err());
    assert_eq!(db.get_payload_usage().await?, usage);
    assert_eq!(db.payload_admission.demands.lock().unwrap().usage().0, 1);
    assert!(
        db.get_event_content(old.event_id().to_short())
            .await
            .is_some()
    );
    assert!(notifications.try_recv().is_err());
    db.write_with(|tx| {
        assert!(
            tx.open_table(&crate::events_quota_pruned::TABLE)?
                .get(&old.event_id().to_short())?
                .is_none()
        );
        let mut table = tx.open_table(&crate::ids_data_usage::TABLE)?;
        let mut author = table.get(&a.id())?.unwrap().value_try()?;
        author.current_content_size = bytes;
        table.insert(&a.id(), &author)?;
        Ok(())
    })
    .await?;
    assert_eq!(
        step(&db, generation, 100).await?,
        DemandStep::Pruned {
            demand: incoming.event_id(),
            victim: old.event_id(),
            bytes,
        }
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn demand_equal_budget_replacement_invalidates_without_structural_comparison()
-> anyhow::Result<()> {
    let db = Database::new_in_memory(RostraIdSecretKey::generate().id()).await?;
    let a = RostraIdSecretKey::generate();
    let old = post(a, 1, "same");
    let incoming = post(a, 2, "same");
    ingest(&db, &old, true).await?;
    ingest(&db, &incoming, false).await?;
    let bytes = u64::from(old.content_len());
    let generation = configure(&db, bytes + 1, 10000, 5, 10000).await?;
    let owner = db
        .register_payload_demand(incoming.event_id(), generation)
        .await?;
    assert!(matches!(owner, DemandRegistration::Pending(_)));
    let usage = db.payload_admission_usage();
    assert_eq!(
        (usage.pending_demands, usage.pending_demand_bytes),
        (1, bytes)
    );
    assert_eq!((usage.logical_reserved_bytes, usage.buffer_bytes), (0, 0));
    let old_identity = db
        .payload_admission
        .state
        .lock()
        .unwrap()
        .config
        .as_ref()
        .unwrap()
        .identity();
    configure(&db, bytes + 1, 10000, 5, 10000).await?;
    let new_identity = db
        .payload_admission
        .state
        .lock()
        .unwrap()
        .config
        .as_ref()
        .unwrap()
        .identity();
    assert!(!old_identity.ptr_eq(&new_identity));
    assert!(
        old_identity.upgrade().is_none(),
        "intent must not retain configuration"
    );
    assert_eq!(
        db.preempt_payload_demand(
            generation,
            limit(10),
            10000,
            Instant::now() + Duration::from_secs(5),
        )
        .await?,
        DemandStep::NotReady
    );
    assert_eq!(db.payload_admission_usage().pending_demands, 0);
    drop(owner);
    Ok(())
}
