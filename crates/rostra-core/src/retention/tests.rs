use super::simulation::{Budget, Candidate, simulate};
use super::{RetentionDistance, RetentionPolicy};
use crate::id::RostraId;
use crate::{EventId, Timestamp};

fn event(n: u32) -> EventId {
    let mut bytes = [0; 32];
    bytes[28..].copy_from_slice(&n.to_be_bytes());
    EventId::from_bytes(bytes)
}

fn holder(n: u8) -> RostraId {
    RostraId::from_bytes([n; 32])
}

#[test]
fn validation_and_encoding() {
    let policy = RetentionPolicy::experimental();
    assert_eq!(RetentionPolicy::from_bytes(policy.to_bytes()), Some(policy));
    assert_eq!(
        policy.to_bytes(),
        [
            0, 0, 0, 1, 0, 0, 4, 0, 0, 39, 141, 0, 0, 0, 128, 0, 0, 1, 0, 0, 0, 0, 0, 64, 0, 1, 81,
            128,
        ]
    );
    let mut unknown = policy.to_bytes();
    unknown[3] = 2;
    assert_eq!(RetentionPolicy::from_bytes(unknown), None);
    assert!(RetentionPolicy::new(0, 1, 0, 0, 1, 0).is_none());
    assert!(RetentionPolicy::new(1, 0, 0, 0, 1, 0).is_none());
    assert!(RetentionPolicy::new(1, 1, u32::MAX, u32::MAX, u32::MAX, 0).is_some());
    assert!(RetentionPolicy::new(1, 1, 0, 0, 0, 0).is_none());
    assert!(RetentionPolicy::new(1, 1, 0, 0, 1, u32::MAX).is_some());
    assert!(Budget::new(0, 0).is_none());
    assert!(Budget::new(10, 10).is_none());
    assert!(Budget::new(10, 11).is_none());
}

#[test]
fn timestamp_and_grace_boundaries() {
    let policy = RetentionPolicy::experimental();
    assert_eq!(
        RetentionPolicy::effective_timestamp(200.into(), 100.into()),
        Timestamp::from(100)
    );
    assert_eq!(
        RetentionPolicy::effective_timestamp(50.into(), 100.into()),
        Timestamp::from(50)
    );
    assert!(!policy.grace_elapsed(None, Timestamp::MAX));
    assert!(!policy.grace_elapsed(Some(100.into()), 99.into()));
    assert!(!policy.grace_elapsed(Some(100.into()), 86499.into()));
    assert!(policy.grace_elapsed(Some(100.into()), 86500.into()));
    assert!(!policy.grace_elapsed(Some(Timestamp::MAX), Timestamp::MAX));
}

#[test]
fn monotonicity_caps_and_extreme_arithmetic() {
    let policy = RetentionPolicy::experimental();
    let key = |distance, len, time| policy.key_at_distance(event(0), distance, len, time);
    let baseline = key(u64::MAX, 1024, 100.into());
    assert_eq!(baseline, key(u64::MAX, 0, 100.into()));
    let mut previous = baseline;
    for len in (1024..=1_000_000).step_by(37) {
        let next = key(u64::MAX, len, 100.into());
        assert!(next <= previous);
        previous = next;
    }
    let cap = key(0, 1024, 100.into());
    assert_eq!(cap, key(1_u64 << 58, 1024, 100.into()));
    previous = cap;
    for distance in (0..=65535_u64).map(|n| n << 48).chain([u64::MAX]) {
        let next = key(distance, 1024, 100.into());
        assert!(next <= previous);
        previous = next;
    }
    assert!(baseline < key(u64::MAX, 1024, 101.into()));
    let extreme =
        RetentionPolicy::new(1, u32::MAX, u32::MAX, u32::MAX, u32::MAX, u32::MAX).unwrap();
    let negative = extreme.key_at_distance(event(0), u64::MAX, u32::MAX, Timestamp::ZERO);
    let positive = extreme.key_at_distance(event(0), 0, 0, Timestamp::MAX);
    assert!(negative.ticks() < 0);
    assert!(negative.to_bytes() < positive.to_bytes());
    assert!(positive.ticks() > 0);
    let all_max =
        RetentionPolicy::new(u32::MAX, u32::MAX, u32::MAX, u32::MAX, u32::MAX, u32::MAX).unwrap();
    assert_eq!(
        RetentionPolicy::from_bytes(all_max.to_bytes()),
        Some(all_max)
    );
    let no_size_penalty = all_max.key_at_distance(event(0), 0, u32::MAX, Timestamp::MAX);
    assert_eq!(no_size_penalty, positive);
    let expiry = u64::from(u32::MAX) + 10;
    assert!(!all_max.grace_elapsed(Some(10.into()), (expiry - 1).into()));
    assert!(all_max.grace_elapsed(Some(10.into()), expiry.into()));
    assert!(!all_max.grace_elapsed(Some((u64::MAX - 1).into()), Timestamp::MAX));
}

#[test]
fn static_score_matches_floating_reference() {
    let policy = RetentionPolicy::experimental();
    let mut max_error = 0.0_f64;
    for distance in [0, 1, 1 << 58, 1 << 60, 1 << 63, u64::MAX] {
        for size in [0_u32, 1024, 1025, 65536, 1_000_000, u32::MAX] {
            let timestamp = 1_000_000.;
            let key = policy
                .key_at_distance(event(0), distance, size, (timestamp as u64).into())
                .ticks() as f64
                / 4294967296.;
            let s = f64::from(size.max(1024)) / 1024.;
            let d = distance as f64 / 18446744073709551616.;
            let bonus = 1. / d.max(1. / 64.);
            for now in [2_000_000., 20_000_000., 200_000_000.] {
                let dynamic = -(now - timestamp) / 2592000. - 0.5 * s.ln() + bonus.ln();
                let reconstructed = (key - now) / 2592000.;
                let error_seconds = ((dynamic - reconstructed) * 2592000.).abs();
                max_error = max_error.max(error_seconds);
                assert!(error_seconds < 0.02, "{error_seconds}");
            }
        }
    }
    println!("maximum sampled static/reference error: {max_error} seconds");
}

fn candidates() -> Vec<Candidate> {
    (0..1024)
        .map(|i| Candidate {
            event: event(i),
            author: holder((i % 4) as u8),
            content_len: 1024,
            effective_timestamp: Timestamp::from(1_000_000 + u64::from(i) * 3600),
            first_materialized: Some(Timestamp::ZERO),
            protected: false,
        })
        .collect()
}

#[test]
fn deterministic_pressure_and_account_correlation() {
    let policy = RetentionPolicy::experimental();
    let candidates = candidates();
    let run = |account, inputs: &[Candidate]| {
        simulate(
            policy,
            account,
            10_000_000.into(),
            inputs,
            Budget::new(300_000, 270_000).unwrap(),
            Budget::new(512_000, 460_800).unwrap(),
        )
        .unwrap()
    };
    let first = run(holder(0), &candidates);
    let mut reversed = candidates.clone();
    reversed.reverse();
    assert_eq!(first, run(holder(0), &reversed));
    // Transport identity is deliberately absent from the API: replicas of one
    // account, even using distinct endpoints, have exactly the same result.
    assert_eq!(first, run(holder(0), &candidates));
    let second = run(holder(1), &candidates);
    assert_ne!(first.evicted, second.evicted);
    assert_eq!(first.retained_bytes, 460_800);
    let overlap = first
        .evicted
        .iter()
        .filter(|id| second.evicted.contains(id))
        .count();
    println!(
        "two holders: evicted={}, common={overlap}",
        first.evicted.len()
    );
    assert!(!first.unmet_global);
    assert!(first.unmet_authors.is_empty());
    let mut duplicate = candidates.clone();
    duplicate.push(candidates[0]);
    assert!(
        simulate(
            policy,
            holder(0),
            Timestamp::MAX,
            &duplicate,
            Budget::new(1, 0).unwrap(),
            Budget::new(1, 0).unwrap()
        )
        .is_none()
    );
}

#[test]
fn pressure_protection_and_no_age_expiry() {
    let policy = RetentionPolicy::experimental();
    let mut candidates = candidates();
    let generous = Budget::new(u64::MAX, u64::MAX - 1).unwrap();
    let tiny = Budget::new(1, 0).unwrap();
    let run = |inputs: &[Candidate], author, global| {
        simulate(policy, holder(0), Timestamp::MAX, inputs, author, global).unwrap()
    };
    assert!(run(&candidates, generous, generous).evicted.is_empty());
    let emptied = run(&candidates, tiny, generous);
    assert_eq!(emptied.retained_bytes, 0);
    assert_eq!(emptied.evicted.len(), candidates.len());
    candidates[0].protected = true;
    candidates[1].first_materialized = None;
    candidates[2].first_materialized = Some(Timestamp::MAX);
    candidates[3].content_len = 0;
    let blocked = run(&candidates, tiny, tiny);
    assert_eq!(blocked.retained_bytes, 3 * 1024);
    assert_eq!(blocked.unmet_authors.len(), 3);
    assert!(blocked.unmet_global);
    assert!(!blocked.evicted.contains(&candidates[3].event));
    // Exactly high-water never triggers low-water cleanup.
    candidates[0].protected = false;
    let exact = Budget::new(1024, 0).unwrap();
    assert!(run(&candidates[..1], exact, exact).evicted.is_empty());
    let indivisible = Budget::new(1023, 512).unwrap();
    assert_eq!(
        run(&candidates[..1], generous, indivisible).retained_bytes,
        0
    );
}

#[test]
fn full_identifiers_and_golden_vectors() {
    let distance = RetentionDistance::new(event(0), holder(1));
    assert_ne!(distance, RetentionDistance::new(event(1), holder(1)));
    let mut changed_holder = [1; 32];
    changed_holder[31] = 2;
    assert_ne!(
        distance,
        RetentionDistance::new(event(0), RostraId::from_bytes(changed_holder))
    );
    assert_eq!(
        distance.to_bytes(),
        [
            222, 162, 23, 23, 11, 30, 181, 185, 42, 162, 157, 98, 104, 66, 67, 216, 161, 164, 252,
            180, 120, 67, 91, 205, 137, 71, 64, 193, 243, 24, 108, 90,
        ]
    );
    assert_eq!(
        RetentionPolicy::experimental()
            .key(event(0), holder(1), 1048576, 1_000_000.into())
            .to_bytes(),
        [
            127, 255, 255, 255, 255, 255, 255, 255, 255, 139, 181, 166, 153, 163, 124, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]
    );
}

#[test]
fn bounded_bonus_has_a_final_tail_cutoff() {
    let policy = RetentionPolicy::experimental();
    let old_best = policy.key_at_distance(event(0), 0, 1024, Timestamp::ZERO);
    let fresh_worst = policy.key_at_distance(event(1), u64::MAX, 1024, (125 * 86400).into());
    assert!(old_best < fresh_worst);
    // The cap removes further preference even at the exact zero-distance edge.
    assert_eq!(
        old_best,
        policy.key_at_distance(event(0), 1, 1024, Timestamp::ZERO)
    );
    let no_bonus = RetentionPolicy::new(1024, 2592000, 32768, 65536, 1, 0).unwrap();
    assert_eq!(
        no_bonus.key_at_distance(event(0), 0, 1024, Timestamp::ZERO),
        no_bonus.key_at_distance(event(0), u64::MAX, 1024, Timestamp::ZERO)
    );
}

#[test]
fn key_encoding_orders_signed_time_and_full_id_ties() {
    let policy = RetentionPolicy::new(1, 1, 0, 0, 1, 0).unwrap();
    let earlier = policy.key(event(1), holder(0), u32::MAX, Timestamp::ZERO);
    let later = policy.key(event(0), holder(0), 0, 1.into());
    let tied = policy.key(event(1), holder(0), 0, 1.into());
    assert!(earlier < later && later < tied);
    assert!(earlier.to_bytes() < later.to_bytes() && later.to_bytes() < tied.to_bytes());
    assert_eq!(
        &later.to_bytes()[..16],
        &[128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]
    );
    assert_eq!(&tied.to_bytes()[16..], event(1).as_slice());
    assert!(policy.grace_elapsed(Some(Timestamp::MAX), Timestamp::MAX));
    assert!(!policy.grace_elapsed(None, Timestamp::MAX));
}

#[test]
fn diagnostic_distance_credit_is_exact_key_contribution() {
    for beta in [0, 65536, u32::MAX] {
        for cap in [1, 8, 64, u32::MAX] {
            let policy = RetentionPolicy::new(1024, u32::MAX, 32768, beta, cap, 0).unwrap();
            let baseline = RetentionPolicy::new(1024, u32::MAX, 32768, 0, cap, 0).unwrap();
            for n in 0..32 {
                let credit = policy.distance_credit_ticks(event(n), holder(0));
                assert!(credit >= 0);
                assert_eq!(
                    policy
                        .key(event(n), holder(0), 100_000, Timestamp::MAX)
                        .ticks()
                        - baseline
                            .key(event(n), holder(0), 100_000, Timestamp::MAX)
                            .ticks(),
                    credit
                );
            }
        }
    }
}
