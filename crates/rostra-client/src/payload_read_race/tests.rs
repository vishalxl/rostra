use std::cell::Cell;
use std::rc::Rc;

use super::race_payload_reads;

struct Slots(Rc<Cell<usize>>);

impl Drop for Slots {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 2);
    }
}

fn reserve(live: &Rc<Cell<usize>>, limit: usize) -> Result<Slots, ()> {
    if live.get() + 2 > limit {
        return Err(());
    }
    live.set(live.get() + 2);
    Ok(Slots(live.clone()))
}

#[tokio::test]
async fn tight_capacity_preserves_later_peer_after_first_has_no_payload() {
    let live = Rc::new(Cell::new(0));
    let queried = Rc::new(Cell::new(0));
    let winner = race_payload_reads(
        &[1, 2, 3],
        || reserve(&live, 2),
        |peer, guard| {
            queried.set(queried.get() + 1);
            async move {
                tokio::task::yield_now().await;
                (peer == 2).then_some((peer, guard))
            }
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(winner.0, 2);
    assert_eq!(queried.get(), 2);
    assert_eq!(
        live.get(),
        2,
        "winner remains owned, including through ingestion"
    );
    drop(winner);
    assert_eq!(live.get(), 0);
}

#[tokio::test]
async fn cancellation_releases_four_attempts_without_consuming_extra_peers() {
    let live = Rc::new(Cell::new(0));
    let queried = Rc::new(Cell::new(0));
    let mut race = Box::pin(race_payload_reads(
        &[1, 2, 3, 4, 5],
        || reserve(&live, 8),
        |peer, guard| {
            queried.set(queried.get() + 1);
            async move {
                std::future::pending::<()>().await;
                Some((peer, guard))
            }
        },
    ));
    assert!(futures::poll!(race.as_mut()).is_pending());
    assert_eq!(live.get(), 8);
    assert_eq!(queried.get(), 4);
    drop(race);
    assert_eq!(live.get(), 0);
    let result = race_payload_reads(&[1], || reserve(&live, 1), |_, _| async { Some(()) }).await;
    assert!(result.is_err());
    assert_eq!(live.get(), 0);
}

#[tokio::test]
async fn timed_out_first_holder_releases_tight_capacity_for_next_holder() {
    let live = Rc::new(Cell::new(0));
    let winner = race_payload_reads(
        &[1, 2],
        || reserve(&live, 2),
        |peer, guard| async move {
            crate::task::outbound_deadline::within(
                std::time::Duration::from_millis(10),
                async move {
                    if peer == 1 {
                        std::future::pending::<()>().await;
                    }
                    Some((peer, guard))
                },
            )
            .await
            .ok()
            .flatten()
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(winner.0, 2);
    assert_eq!(live.get(), 2);
    drop(winner);
    assert_eq!(live.get(), 0);
}
