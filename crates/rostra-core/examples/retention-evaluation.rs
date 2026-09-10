//! Offline deterministic experiment; synthetic demand and hash sharing, not a
//! network model.

use std::collections::{BTreeMap, BTreeSet};

use rostra_core::id::RostraId;
use rostra_core::retention::simulation::{Budget, Candidate, simulate};
use rostra_core::retention::{RetentionDistance, RetentionPolicy};
use rostra_core::{EventId, Timestamp};

const DAY: u64 = 86400;
const COUNT: usize = 4096;

fn bytes(domain: &str, n: usize) -> [u8; 32] {
    *blake3::hash(format!("{domain}:{n}").as_bytes()).as_bytes()
}

fn holder(n: usize) -> RostraId {
    // Devices zero and one intentionally share an account, budget and follow set.
    RostraId::from_bytes(bytes("holder", n.saturating_sub(1)))
}

fn main() {
    println!("fixture=v1 events={COUNT} devices=12 accounts=11 authors=32 seed=domain-counter");
    println!(
        "policy,clock,retained_logical,network_events,last_copy_loss,rare_loss,online_success,unordered_attempts,ordered_attempts,evicted_logical,redownload_without_terminal,suppressed_retry_bytes,unique_removed,unique_pinned,unmet_devices,same_account_equal"
    );
    for (name, alpha, beta, cap) in [
        ("oldest", 0, 0, 1),
        ("age-size", 32768, 0, 1),
        ("distance-8", 32768, 65536, 8),
        ("distance-64", 32768, 65536, 64),
        ("distance-strong", 65536, 131072, 64),
    ] {
        let policy =
            RetentionPolicy::new(1024, 30 * DAY as u32, alpha, beta, cap, DAY as u32).unwrap();
        for (clock, now) in [
            ("normal", 400 * DAY),
            ("backward", 340 * DAY),
            ("forward", 800 * DAY),
        ] {
            run(name, clock, now, policy);
        }
    }
}

fn run(name: &str, clock: &str, now: u64, policy: RetentionPolicy) {
    let candidates: Vec<_> = (0..COUNT)
        .map(|n| {
            let author = if n % 257 == 0 { 31 } else { (n / 64) % 31 };
            let mut event = EventId::from_bytes(bytes("event", n));
            // A bounded 16-ID grinding sample targets holder zero for every 97th event.
            if n % 97 == 0 {
                event = (0..16)
                    .map(|trial| EventId::from_bytes(bytes("grind", n * 16 + trial)))
                    .min_by_key(|id| RetentionDistance::new(*id, holder(0)))
                    .unwrap();
            }
            let signed = if n % 101 == 0 {
                900 * DAY
            } else {
                (n % 360) as u64 * DAY
            };
            let received = (360 + n / 256) as u64 * DAY;
            Candidate {
                event,
                author: RostraId::from_bytes(bytes("author", author)),
                content_len: if n % 64 == 0 {
                    256 * 1024
                } else if n % 8 == 0 {
                    16 * 1024
                } else {
                    1024
                },
                effective_timestamp: RetentionPolicy::effective_timestamp(
                    signed.into(),
                    received.into(),
                ),
                first_materialized: (n % 113 != 0).then_some(received.into()),
                protected: n % 127 == 0 || received > now,
            }
        })
        .collect();
    let mut retained = Vec::new();
    let mut observed = BTreeSet::new();
    let mut evicted_bytes = 0u64;
    let mut retries = 0u64;
    let mut unique_removed = 0u64;
    let mut unique_pinned = 0u64;
    let mut unmet = 0;
    let mut total = 0u128;
    for device in 0usize..12 {
        let account = device.saturating_sub(1);
        let input: Vec<_> = candidates
            .iter()
            .enumerate()
            .filter(|(n, _)| {
                let author = if n % 257 == 0 { 31 } else { (n / 64) % 31 };
                if author == 31 {
                    account < 2
                } else {
                    (author + account) % 4 != 0
                }
            })
            .map(|(_, c)| *c)
            .collect();
        observed.extend(input.iter().map(|c| c.event));
        let high = (4 + account % 3 * 2) * 1024 * 1024;
        let result = simulate(
            policy,
            holder(device),
            Timestamp::from(now),
            &input,
            Budget::new(2 * 1024 * 1024, 1800 * 1024).unwrap(),
            Budget::new(high as u64, high as u64 * 9 / 10).unwrap(),
        )
        .unwrap();
        let victims: BTreeSet<_> = result.evicted.into_iter().collect();
        let kept: BTreeSet<_> = input
            .iter()
            .filter(|c| !victims.contains(&c.event))
            .map(|c| c.event)
            .collect();
        total += result.retained_bytes;
        unmet += usize::from(result.unmet_global || !result.unmet_authors.is_empty());
        // Synthetic shared hash groups preserve size class: every other block of 64
        // shares with its predecessor. This is not event-ID deduplication.
        let hash = |n: usize| (n / 128, n % 64);
        let index: BTreeMap<_, _> = candidates
            .iter()
            .enumerate()
            .map(|(n, c)| (c.event, n))
            .collect();
        let live_hashes: BTreeSet<_> = kept.iter().map(|id| hash(index[id])).collect();
        let mut removed_hashes = BTreeSet::new();
        for c in &input {
            if victims.contains(&c.event) {
                evicted_bytes += u64::from(c.content_len);
                // Exactly three replay/duplicate requests per victim: terminal state
                // suppresses all; a hypothetical stateless fetcher downloads each.
                retries += 3 * u64::from(c.content_len);
                let h = hash(index[&c.event]);
                if removed_hashes.insert(h) {
                    if live_hashes.contains(&h) {
                        unique_pinned += u64::from(c.content_len);
                    } else {
                        unique_removed += u64::from(c.content_len);
                    }
                }
            }
        }
        retained.push(kept);
    }
    let union: BTreeSet<_> = retained.iter().flatten().copied().collect();
    let lost = observed.difference(&union).count();
    let rare_loss = candidates
        .iter()
        .enumerate()
        .filter(|(n, c)| n % 257 == 0 && !union.contains(&c.event))
        .count();
    // Isolate loss of the duplicated account from loss of the entire rare follow
    // set: account one remains online in the partial-outage scenario.
    let rare = |c: &&Candidate| c.author == RostraId::from_bytes(bytes("author", 31));
    let rare_partial_hits = candidates
        .iter()
        .filter(rare)
        .filter(|c| retained[2].contains(&c.event))
        .count();
    let rare_full_outage_hits = candidates
        .iter()
        .filter(rare)
        .filter(|c| (3..12).any(|d| retained[d].contains(&c.event)))
        .count();
    eprintln!("rare_availability,{name},{clock},duplicated_account_outage,{rare_partial_hits},16");
    eprintln!(
        "rare_availability,{name},{clock},entire_follow_set_outage,{rare_full_outage_hits},16"
    );
    let mut success = 0usize;
    let mut unordered = 0usize;
    let mut ordered = 0usize;
    // Correlated outage removes accounts zero and one, including both devices of
    // zero. One candidate per online account, identical discovery lists for
    // both orders.
    for c in &candidates {
        let peers: Vec<_> = (3..12).collect();
        let attempts = |peers: &[usize]| {
            peers
                .iter()
                .position(|d| retained[*d].contains(&c.event))
                .map_or(peers.len(), |n| n + 1)
        };
        success += usize::from(peers.iter().any(|d| retained[*d].contains(&c.event)));
        unordered += attempts(&peers);
        let mut ranked = peers;
        ranked.sort_by_key(|d| RetentionDistance::new(c.event, holder(*d)));
        ordered += attempts(&ranked);
    }
    println!(
        "{name},{clock},{total},{},{lost},{rare_loss},{success},{unordered},{ordered},{evicted_bytes},{retries},{retries},{unique_removed},{unique_pinned},{unmet},{}",
        union.len(),
        retained[0] == retained[1]
    );
    // Distribution is separate, machine-readable, and includes empty author bins.
    for author in 0..32 {
        let id = RostraId::from_bytes(bytes("author", author));
        let kept: Vec<_> = candidates
            .iter()
            .filter(|c| c.author == id && union.contains(&c.event))
            .collect();
        eprintln!(
            "author,{name},{clock},{author},{},{}",
            kept.len(),
            kept.iter().map(|c| u64::from(c.content_len)).sum::<u64>()
        );
    }
    for (label, predicate) in [
        (
            "small",
            (|c: &Candidate| c.content_len == 1024) as fn(&Candidate) -> bool,
        ),
        ("medium", |c: &Candidate| c.content_len == 16 * 1024),
        ("large", |c: &Candidate| c.content_len == 256 * 1024),
        ("old", |c: &Candidate| {
            c.effective_timestamp.as_u64() < 180 * DAY
        }),
        ("recent", |c: &Candidate| {
            c.effective_timestamp.as_u64() >= 180 * DAY
        }),
    ] {
        let kept: Vec<_> = candidates
            .iter()
            .filter(|c| predicate(c) && union.contains(&c.event))
            .collect();
        eprintln!(
            "distribution,{name},{clock},{label},{},{}",
            kept.len(),
            kept.iter().map(|c| u64::from(c.content_len)).sum::<u64>()
        );
    }
}
