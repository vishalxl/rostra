use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::sync::{OnceCell, oneshot};

use super::{CLEANUP_INTERVAL, ConnectionCache};

#[tokio::test]
async fn pending_cell_survives_cleanup_and_coalesces_initialization() {
    let cache = ConnectionCache::new();
    let cell = Arc::new(OnceCell::new());
    let mut cells = HashMap::from([(1, Arc::clone(&cell))]);
    let initialization_count = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();

    let first_cell = Arc::clone(&cell);
    let first_count = Arc::clone(&initialization_count);
    let first = tokio::spawn(async move {
        *first_cell
            .get_or_try_init(|| async move {
                first_count.fetch_add(1, Ordering::SeqCst);
                started_tx.send(()).unwrap();
                release_rx.await.unwrap();
                Ok::<_, Infallible>(7)
            })
            .await
            .unwrap()
    });
    started_rx.await.unwrap();

    assert_eq!(cache.maybe_cleanup_cells(&mut cells, |_| true), 0);
    let retained = Arc::clone(cells.get(&1).unwrap());
    assert!(Arc::ptr_eq(&cell, &retained));

    let second_count = Arc::clone(&initialization_count);
    let second = retained.get_or_try_init(|| async move {
        second_count.fetch_add(1, Ordering::SeqCst);
        Ok::<_, Infallible>(99)
    });
    tokio::pin!(second);
    assert!(futures::poll!(second.as_mut()).is_pending());

    release_tx.send(()).unwrap();
    assert_eq!(first.await.unwrap(), 7);
    assert_eq!(*second.await.unwrap(), 7);
    assert_eq!(initialization_count.load(Ordering::SeqCst), 1);
    assert_eq!(cells.get(&1).unwrap().get(), Some(&7));
}

#[tokio::test]
async fn cancelled_initializer_leaves_empty_cell_for_later_cleanup() {
    let cache = ConnectionCache::new();
    let cell = Arc::new(OnceCell::<()>::new());
    let initializing_cell = Arc::clone(&cell);
    let mut cells = HashMap::from([(1, cell)]);
    let (started_tx, started_rx) = oneshot::channel();

    let initializer = tokio::spawn(async move {
        let _ = initializing_cell
            .get_or_try_init(|| async move {
                started_tx.send(()).unwrap();
                std::future::pending::<Result<(), Infallible>>().await
            })
            .await;
    });
    started_rx.await.unwrap();

    assert_eq!(cache.maybe_cleanup_cells(&mut cells, |_| true), 0);
    assert!(cells.contains_key(&1));

    initializer.abort();
    assert!(initializer.await.unwrap_err().is_cancelled());
    let retrying_cell = Arc::clone(cells.get(&1).unwrap());
    assert!(Arc::ptr_eq(cells.get(&1).unwrap(), &retrying_cell));
    assert_eq!(
        retrying_cell
            .get_or_try_init(|| async { Err::<(), _>(()) })
            .await,
        Err(())
    );
    assert!(retrying_cell.get().is_none());
    drop(retrying_cell);

    for _ in 0..CLEANUP_INTERVAL - 1 {
        assert_eq!(cache.maybe_cleanup_cells(&mut cells, |_| true), 0);
        assert!(cells.contains_key(&1));
    }
    assert_eq!(cache.maybe_cleanup_cells(&mut cells, |_| true), 1);
    assert!(!cells.contains_key(&1));
}

#[test]
fn empty_cell_remains_while_shared_and_is_removed_after_release() {
    let cache = ConnectionCache::new();
    let cell = Arc::new(OnceCell::<()>::new());
    let waiter = Arc::clone(&cell);
    let mut cells = HashMap::from([(1, cell)]);

    assert_eq!(cache.maybe_cleanup_cells(&mut cells, |_| true), 0);
    assert!(cells.contains_key(&1));

    drop(waiter);
    for _ in 0..CLEANUP_INTERVAL - 1 {
        assert_eq!(cache.maybe_cleanup_cells(&mut cells, |_| true), 0);
        assert!(cells.contains_key(&1));
    }
    assert_eq!(cache.maybe_cleanup_cells(&mut cells, |_| true), 1);
    assert!(!cells.contains_key(&1));
}

#[test]
fn initialized_open_cell_survives_and_closed_cell_is_removed() {
    let cache = ConnectionCache::new();
    let open = Arc::new(AtomicBool::new(true));
    let open_cell = Arc::new(OnceCell::new());
    open_cell.set(Arc::clone(&open)).unwrap();
    let external_cell_owner = Arc::clone(&open_cell);
    let external_connection = Arc::clone(open_cell.get().unwrap());
    let mut cells = HashMap::from([(1, open_cell)]);

    assert_eq!(
        cache.maybe_cleanup_cells(&mut cells, |connection| connection.load(Ordering::SeqCst)),
        0
    );
    assert!(cells.contains_key(&1));

    open.store(false, Ordering::SeqCst);
    for _ in 0..CLEANUP_INTERVAL - 1 {
        cache.maybe_cleanup_cells(&mut cells, |connection| connection.load(Ordering::SeqCst));
    }
    assert_eq!(
        cache.maybe_cleanup_cells(&mut cells, |connection| connection.load(Ordering::SeqCst)),
        1
    );
    assert!(!cells.contains_key(&1));
    assert!(!external_cell_owner.get().unwrap().load(Ordering::SeqCst));
    assert!(!external_connection.load(Ordering::SeqCst));
}

#[test]
fn completion_after_shared_detection_remains_discoverable() {
    let cache = ConnectionCache::new();
    let cell = Arc::new(OnceCell::new());
    let completing_owner = Arc::clone(&cell);
    let mut cells = HashMap::from([(1, cell)]);
    let (complete_tx, complete_rx) = std::sync::mpsc::channel();
    let (completed_tx, completed_rx) = std::sync::mpsc::channel();
    let completion = std::thread::spawn(move || {
        complete_rx.recv().unwrap();
        completing_owner.set(true).unwrap();
        drop(completing_owner);
        completed_tx.send(()).unwrap();
    });

    assert_eq!(
        cache.maybe_cleanup_cells_after_shared(
            &mut cells,
            |is_open| *is_open,
            |_| {
                complete_tx.send(()).unwrap();
                completed_rx.recv().unwrap();
            }
        ),
        0
    );
    completion.join().unwrap();
    assert_eq!(cells.get(&1).unwrap().get(), Some(&true));
}
