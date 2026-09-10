use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use rostra_core::event::{Event, EventContentRaw, EventExt as _, EventKind, VerifiedEvent};
use rostra_core::id::RostraIdSecretKey;
use rostra_p2p::connection::{Connection, GetEventContentResponse, RpcId};
use rostra_p2p_api::ROSTRA_P2P_V0_ALPN;

#[derive(Debug)]
struct Guard(Arc<AtomicUsize>);

impl Guard {
    fn new(live: &Arc<AtomicUsize>) -> Self {
        live.fetch_add(1, SeqCst);
        Self(live.clone())
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, SeqCst);
    }
}

async fn exercise_reads(stall: bool) {
    let lookup = iroh::address_lookup::memory::MemoryLookup::new();
    let server = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .alpns(vec![ROSTRA_P2P_V0_ALPN.to_vec()])
        .address_lookup(lookup.clone())
        .bind()
        .await
        .unwrap();
    lookup.add_endpoint_info(server.addr());
    let client = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .alpns(vec![ROSTRA_P2P_V0_ALPN.to_vec()])
        .address_lookup(lookup)
        .bind()
        .await
        .unwrap();
    let raw = EventContentRaw::new(vec![42; 32768]);
    let author = RostraIdSecretKey::generate();
    let event = Event::builder_raw_content()
        .author(author.id())
        .kind(EventKind::NULL)
        .content(&raw)
        .build();
    let event = VerifiedEvent::verify_signed(author.id(), event.signed_by(author)).unwrap();
    let (started, mut received) = tokio::sync::mpsc::channel(4);
    let server_endpoint = server.clone();
    let server_task = tokio::spawn(async move {
        let incoming = server_endpoint.accept().await.unwrap();
        let connection = incoming.accept().unwrap().await.unwrap();
        let mut streams = Vec::new();
        for _ in 0..if stall { 4 } else { 1 } {
            let (mut send, mut recv) = connection.accept_bi().await.unwrap();
            let (rpc, _) = Connection::read_request_raw(&mut recv).await.unwrap();
            assert_eq!(rpc, RpcId::GET_EVENT_CONTENT);
            Connection::write_success_return_code(&mut send)
                .await
                .unwrap();
            Connection::write_message(&mut send, &GetEventContentResponse(true))
                .await
                .unwrap();
            if !stall {
                Connection::write_bao_content(&mut send, raw.as_ref(), event.content_hash())
                    .await
                    .unwrap();
                send.finish().unwrap();
            }
            streams.push((send, recv));
            started.send(()).await.unwrap();
        }
        std::future::pending::<()>().await;
        drop(streams);
    });
    let conn = Connection::from(
        client
            .connect(server.id(), ROSTRA_P2P_V0_ALPN)
            .await
            .unwrap(),
    );
    let live = Arc::new(AtomicUsize::new(0));
    if stall {
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let conn = conn.clone();
            let guards = (Guard::new(&live), Guard::new(&live));
            tasks.push(tokio::spawn(async move {
                conn.get_event_content_with_guard(event, guards).await
            }));
        }
        for _ in 0..4 {
            tokio::time::timeout(Duration::from_secs(5), received.recv())
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(live.load(SeqCst), 8);
        for task in tasks {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        assert_eq!(live.load(SeqCst), 0);
    } else {
        let guards = (Guard::new(&live), Guard::new(&live));
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            conn.get_event_content_with_guard(event, guards),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(
            live.load(SeqCst),
            2,
            "winning result must retain both caller guards"
        );
        assert_eq!(result.0.content_len(), 32768);
        drop(result);
        assert_eq!(live.load(SeqCst), 0);
    }
    server_task.abort();
    assert!(server_task.await.unwrap_err().is_cancelled());
    client.close().await;
    server.close().await;
}

#[tokio::test]
async fn winning_read_returns_its_capacity_guards() {
    exercise_reads(false).await;
}

#[tokio::test]
async fn four_cancelled_reads_release_all_eight_guards() {
    exercise_reads(true).await;
}
