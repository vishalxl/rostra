use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use rostra_client_db::Database;
use rostra_core::event::{VerifiedEvent, VerifiedEventContent};
use rostra_core::id::{RostraIdSecretKey, ToShort as _};

use super::Client;

async fn local_client(secret: RostraIdSecretKey, active: bool) -> Arc<Client> {
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .bind()
        .await
        .unwrap();
    let client = Client::builder(secret.id())
        .db(Database::new_in_memory(secret.id()).await.unwrap())
        .iroh_endpoint(endpoint)
        .start_background_tasks(false)
        .start_request_handler(false)
        .build()
        .await
        .unwrap();
    // Establish local authority without starting discovery/signing tasks. These
    // fixtures exercise publication and replication solely through local DBs.
    client.active.store(active, SeqCst);
    client
}

#[tokio::test(flavor = "multi_thread")]
async fn dm_activation_once_drains_preexisting_work_and_shutdown_joins() {
    use rostra_core::event::{Event, EventContentRaw, EventKind};
    let secret = RostraIdSecretKey::generate();
    let sender = RostraIdSecretKey::generate();
    let client = local_client(secret, false).await;
    assert_eq!(client.task_handles.len(), 0);
    assert!(client.db.dm_local_installation().await.unwrap().is_none());
    let epoch = client
        .db
        .dm_maintain_local_now()
        .await
        .unwrap()
        .epoch
        .unwrap();
    let body =
        rostra_dm::MessageBody::new(sender.id(), secret.id(), "before activation".to_owned())
            .unwrap();
    let content = EventContentRaw::new(
        rostra_dm::encrypt(
            &body,
            &[rostra_dm::PublicKey::from_bytes(epoch.public_key).unwrap()],
        )
        .unwrap(),
    );
    let signed = Event::builder_raw_content()
        .author(sender.id())
        .kind(EventKind::DIRECT_MESSAGE)
        .content(&content)
        .build()
        .signed_by(sender);
    client
        .db
        .try_process_event_with_content(
            &VerifiedEventContent::verify(
                VerifiedEvent::verify_signed(sender.id(), signed).unwrap(),
                content,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    // Consume the old notification; startup must inspect the durable queue.
    client.db.dm_pending_notify().notified().await;
    assert!(
        client
            .db
            .dm_history_with(sender.id(), None, 64)
            .await
            .unwrap()
            .is_empty()
    );
    let (first, second) = tokio::join!(client.unlock_active(secret), client.unlock_active(secret));
    first.unwrap();
    second.unwrap();
    client.unlock_active(secret).await.unwrap();
    assert_eq!(client.task_handles.len(), 3);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let history = client
                .db
                .dm_history_with(sender.id(), None, 64)
                .await
                .unwrap();
            if !history.is_empty() {
                assert_eq!(history[0].text, "before activation");
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let weak = Arc::downgrade(&client);
    let completion = Arc::clone(&client.task_handles);
    drop(client);
    tokio::time::timeout(std::time::Duration::from_secs(5), completion.terminated())
        .await
        .unwrap();
    assert!(weak.upgrade().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn dm_nonfull_client_never_starts_message_worker() {
    let secret = RostraIdSecretKey::generate();
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .bind()
        .await
        .unwrap();
    // Omitting an explicit database creates an ephemeral non-full client.
    let client = Client::builder(secret.id())
        .iroh_endpoint(endpoint)
        .start_background_tasks(false)
        .start_request_handler(false)
        .build()
        .await
        .unwrap();
    assert!(!client.is_mode_full);
    assert_eq!(client.task_handles.len(), 0);
    client.unlock_active(secret).await.unwrap();
    assert_eq!(client.task_handles.len(), 2);
    assert!(client.db.dm_local_installation().await.unwrap().is_none());
    assert!(client.require_dm_authority(secret).is_err());
}

async fn replicate_event(source: &Client, target: &Client, event: VerifiedEvent) {
    let content = source
        .db
        .get_event_content(event.event_id.to_short())
        .await
        .unwrap();
    let verified = VerifiedEventContent::verify(event, content).unwrap();
    target
        .db
        .try_process_event_with_content(&verified)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn dm_clients_send_receive_and_keep_sender_history_without_sender_slot() {
    let alice_key = RostraIdSecretKey::generate();
    let bob_key = RostraIdSecretKey::generate();
    let alice = local_client(alice_key, true).await;
    let bob = local_client(bob_key, true).await;
    alice.maintain_direct_messages(alice_key).await.unwrap();
    bob.maintain_direct_messages(bob_key).await.unwrap();
    let bob_head = bob.db.get_self_current_head().await.unwrap();
    let record = bob.db.get_event(bob_head).await.unwrap();
    let announcement = VerifiedEvent::verify_signed(bob_key.id(), record.signed).unwrap();
    replicate_event(&bob, &alice, announcement).await;
    let sent = alice
        .send_direct_message(alice_key, bob_key.id(), "hello Bob".to_owned())
        .await
        .unwrap();
    let local = alice
        .db
        .dm_history_with(bob_key.id(), None, 64)
        .await
        .unwrap();
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].text, "hello Bob");
    replicate_event(&alice, &bob, sent).await;
    assert!(
        bob.db
            .dm_history_with(alice_key.id(), None, 64)
            .await
            .unwrap()
            .is_empty()
    );
    bob.db.dm_process_pending_now().await.unwrap();
    let received = bob
        .db
        .dm_history_with(alice_key.id(), None, 64)
        .await
        .unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].message_id, local[0].message_id);
    assert_eq!(received[0].text, "hello Bob");
    // Replay the exact signed event, not a new encryption.
    replicate_event(&alice, &bob, sent).await;
    bob.db.dm_process_pending_now().await.unwrap();
    assert_eq!(
        bob.db
            .dm_history_with(alice_key.id(), None, 64)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dm_missing_recipient_and_read_only_fail_without_events() {
    let secret = RostraIdSecretKey::generate();
    let peer = RostraIdSecretKey::generate().id();
    let client = local_client(secret, false).await;
    assert!(
        client
            .send_direct_message(secret, peer, "denied".to_owned())
            .await
            .is_err()
    );
    assert!(client.db.get_self_current_head().await.is_none());
    client.active.store(true, SeqCst);
    assert!(
        client
            .send_direct_message(secret, peer, "no key".to_owned())
            .await
            .is_err()
    );
    assert!(client.db.get_self_current_head().await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn dm_worker_is_owned_and_joined_without_retaining_the_client() {
    let secret = RostraIdSecretKey::generate();
    let client = local_client(secret, true).await;
    let weak = Arc::downgrade(&client);
    let completion = Arc::clone(&client.task_handles);
    client.start_direct_message_worker(secret);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while client.db.get_self_current_head().await.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(client);
    tokio::time::timeout(std::time::Duration::from_secs(5), completion.terminated())
        .await
        .unwrap();
    assert!(weak.upgrade().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn dm_idle_worker_wakes_for_committed_ciphertext() {
    let alice_key = RostraIdSecretKey::generate();
    let bob_key = RostraIdSecretKey::generate();
    let alice = local_client(alice_key, true).await;
    let bob = local_client(bob_key, true).await;
    bob.maintain_direct_messages(bob_key).await.unwrap();
    let record = bob
        .db
        .get_event(bob.db.get_self_current_head().await.unwrap())
        .await
        .unwrap();
    replicate_event(
        &bob,
        &alice,
        VerifiedEvent::verify_signed(bob_key.id(), record.signed).unwrap(),
    )
    .await;
    bob.start_direct_message_worker(bob_key);
    // Let its initial empty queue check finish. There is no maintenance due
    // during this test; only the committed queue notification wakes it.
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let sent = alice
        .send_direct_message(alice_key, bob_key.id(), "wake".to_owned())
        .await
        .unwrap();
    replicate_event(&alice, &bob, sent).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if !bob
                .db
                .dm_history_with(alice_key.id(), None, 1)
                .await
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
