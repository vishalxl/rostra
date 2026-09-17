use super::*;

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn can_send_a_message() {
    let mut tx = Sender::new();

    let mut rx = tx.subscribe(10);

    assert_eq!(tx.send(8), 1);

    assert_eq!(rx.recv().await, Ok(8));
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn can_detect_tx_drop() {
    let mut tx = Sender::new();

    let mut rx = tx.subscribe(10);

    assert_eq!(tx.send(8), 1);

    assert_eq!(rx.recv().await, Ok(8));

    drop(tx);

    assert_eq!(rx.recv().await, Err(RecvError::Closed));
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn dedups_items_single_rx() {
    let mut tx = Sender::new();

    let mut rx = tx.subscribe(10);

    assert_eq!(tx.send(8), 1);
    assert_eq!(tx.send(8), 1);
    assert_eq!(tx.send(9), 1);

    assert_eq!(rx.recv().await, Ok(8));
    assert_eq!(rx.recv().await, Ok(9));
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn works_with_multiple() {
    let mut tx = Sender::new();

    let mut rx1 = tx.subscribe(10);
    let mut rx2 = tx.subscribe(10);
    let mut rx3 = tx.subscribe(10);

    assert_eq!(tx.send(8), 3);

    assert_eq!(rx1.recv().await, Ok(8));
    assert_eq!(rx2.recv().await, Ok(8));
    assert_eq!(rx3.recv().await, Ok(8));
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn dedups_items_with_multiple() {
    let mut tx = Sender::new();

    let mut rx1 = tx.subscribe(10);
    let mut rx2 = tx.subscribe(10);
    let mut rx3 = tx.subscribe(10);

    assert_eq!(tx.send(8), 3);

    assert_eq!(rx1.recv().await, Ok(8));

    assert_eq!(tx.send(8), 3);
    assert_eq!(tx.send(9), 3);

    assert_eq!(rx1.recv().await, Ok(8));
    assert_eq!(rx1.recv().await, Ok(9));
    assert_eq!(rx2.recv().await, Ok(8));
    assert_eq!(rx2.recv().await, Ok(9));
    assert_eq!(rx3.recv().await, Ok(8));
    assert_eq!(rx3.recv().await, Ok(9));
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn can_detect_rx_drop() {
    let mut tx = Sender::new();

    let mut rx1 = tx.subscribe(10);
    let rx2 = tx.subscribe(10);

    assert_eq!(tx.send(8), 2);

    assert_eq!(rx1.recv().await, Ok(8));
    drop(rx1);
    drop(rx2);

    assert_eq!(tx.send(9), 0);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn removes_closed_channel_on_duplicate() {
    let mut tx = Sender::new();
    let rx = tx.subscribe(1);

    assert_eq!(tx.send(8), 1);
    drop(rx);

    assert_eq!(tx.send(8), 0);
    assert!(tx.channels.lock().expect("Locking failed").is_empty());
    assert_eq!(tx.send(8), 0);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn removes_only_closed_channel_on_duplicate() {
    let mut tx = Sender::new();
    let mut live_rx = tx.subscribe(1);
    let dead_rx = tx.subscribe(1);

    assert_eq!(tx.send(8), 2);
    drop(dead_rx);

    assert_eq!(tx.send(8), 1);
    assert_eq!(tx.channels.lock().expect("Locking failed").len(), 1);
    assert_eq!(live_rx.recv().await, Ok(8));
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn receiver_clone_keeps_duplicate_channel_connected() {
    let mut tx = Sender::new();
    let rx = tx.subscribe(1);
    let mut rx_clone = rx.clone();

    assert_eq!(tx.send(8), 1);
    drop(rx);

    assert_eq!(tx.send(8), 1);
    assert_eq!(rx_clone.recv().await, Ok(8));

    assert_eq!(tx.send(8), 1);
    drop(rx_clone);

    assert_eq!(tx.send(8), 0);
    assert!(tx.channels.lock().expect("Locking failed").is_empty());
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn removes_closed_duplicate_queue() {
    let mut tx = Sender::new();
    let rx = tx.subscribe(1);
    let weak_inner = Arc::downgrade(&rx.inner);

    assert_eq!(tx.send(8), 1);
    drop(rx);
    assert!(weak_inner.upgrade().is_some());

    assert_eq!(tx.send(8), 0);
    assert!(weak_inner.upgrade().is_none());
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn load_balancing_works() {
    let mut tx = Sender::new();

    let mut rx1 = tx.subscribe(10);
    let mut rx1_clone = rx1.clone();
    let mut rx2 = tx.subscribe(10);
    let mut rx2_clone = rx2.clone();
    let mut rx3 = tx.subscribe(10);
    let mut rx3_clone = rx3.clone();

    assert_eq!(tx.send(8), 3);

    assert_eq!(rx1.recv().await, Ok(8));

    assert_eq!(tx.send(8), 3);
    assert_eq!(tx.send(9), 3);

    assert_eq!(rx1.recv().await, Ok(8));
    assert_eq!(rx1_clone.recv().await, Ok(9));
    assert_eq!(rx2_clone.recv().await, Ok(8));
    assert_eq!(rx2.recv().await, Ok(9));
    assert_eq!(rx3_clone.recv().await, Ok(8));
    assert_eq!(rx3.recv().await, Ok(9));
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn can_detect_lagging() {
    let mut tx = Sender::new();

    let mut rx = tx.subscribe(1);

    assert_eq!(tx.send(8), 1);
    assert_eq!(tx.send(9), 0);

    assert_eq!(rx.recv().await, Ok(8));
    assert_eq!(rx.recv().await, Err(RecvError::Lagging));
    assert_eq!(tx.send(10), 1);
    assert_eq!(rx.recv().await, Ok(10));
}
