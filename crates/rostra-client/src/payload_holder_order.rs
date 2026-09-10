use std::cmp::Ordering;
use std::collections::BTreeSet;

use rostra_core::EventId;
use rostra_core::id::RostraId;
use rostra_core::retention::RetentionDistance;

/// Deduplicate and rank existing payload-holder candidates.
///
/// Known preferred holders stay first in the supplied preference order.
/// Remaining accounts use exact full-ID retention distance with the full
/// holder identity as a deterministic tie-breaker.
pub(crate) fn rank_payload_holders(
    event_id: EventId,
    candidates: impl IntoIterator<Item = RostraId>,
    preferred: impl IntoIterator<Item = RostraId>,
) -> Vec<RostraId> {
    let candidates = candidates.into_iter().collect::<BTreeSet<_>>();
    let mut ranked = Vec::with_capacity(candidates.len());
    let mut emitted = BTreeSet::new();

    for holder in preferred {
        if candidates.contains(&holder) && emitted.insert(holder) {
            ranked.push(holder);
        }
    }

    let mut remaining = candidates
        .into_iter()
        .filter(|holder| !emitted.contains(holder))
        .map(|holder| (RetentionDistance::new(event_id, holder), holder))
        .collect::<Vec<_>>();
    remaining.sort_unstable_by(|a, b| compare_distance_then_holder(*a, *b));
    ranked.extend(remaining.into_iter().map(|(_, holder)| holder));
    ranked
}

fn compare_distance_then_holder<D: Ord>(a: (D, RostraId), b: (D, RostraId)) -> Ordering {
    a.cmp(&b)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use rostra_core::EventId;

    use super::*;

    fn event(byte: u8) -> EventId {
        EventId::from_bytes([byte; 32])
    }

    fn holder(byte: u8) -> RostraId {
        RostraId::from_bytes([byte; 32])
    }

    struct Guard(Rc<Cell<u8>>);

    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.set(self.0.get() - 1);
        }
    }

    #[test]
    fn ranking_deduplicates_and_uses_full_identity_for_deterministic_ties() {
        let event_id = event(9);
        let a = holder(1);
        let b = holder(2);
        let expected = if (RetentionDistance::new(event_id, a), a)
            < (RetentionDistance::new(event_id, b), b)
        {
            vec![a, b]
        } else {
            vec![b, a]
        };

        assert_eq!(rank_payload_holders(event_id, [a, b, a, b], []), expected);
        assert_eq!(
            compare_distance_then_holder((0_u8, b), (0_u8, a)),
            b.cmp(&a)
        );
    }

    #[test]
    fn known_preferences_precede_distance_without_inventing_candidates() {
        let event_id = event(7);
        let preferred = holder(4);
        let absent = holder(5);
        let other = holder(6);

        let ranked =
            rank_payload_holders(event_id, [other, preferred], [absent, preferred, preferred]);
        assert_eq!(ranked[0], preferred);
        assert_eq!(ranked.len(), 2);
        assert!(ranked.contains(&other));
    }

    #[tokio::test]
    async fn tight_capacity_eventually_reaches_a_farther_successful_holder() {
        let event_id = event(3);
        let holders = (1..=8).map(holder).collect::<Vec<_>>();
        let ranked = rank_payload_holders(event_id, holders, []);
        let farther = *ranked.last().expect("holders");
        let live = Rc::new(Cell::new(0_u8));

        let found = crate::payload_read_race::race_payload_reads(
            &ranked,
            || {
                if live.get() == 1 {
                    Err(())
                } else {
                    live.set(1);
                    Ok(Guard(live.clone()))
                }
            },
            |candidate, guard| async move {
                let _guard = guard;
                (candidate == farther).then_some(candidate)
            },
        )
        .await
        .expect("one slot remains usable");

        assert_eq!(found, Some(farther));
    }
}
