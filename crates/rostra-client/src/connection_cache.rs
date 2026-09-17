use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::stream::{self, StreamExt as _};
use rostra_core::ShortEventId;
use rostra_core::event::{EventExt as _, VerifiedEvent};
use rostra_core::id::{RostraId, ToShort as _};
use rostra_p2p::Connection;
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, trace};

use crate::error::ConnectResult;
use crate::net::ClientNetworking;

const LOG_TARGET: &str = "rostra-client::connection-cache";
const CLEANUP_INTERVAL: u64 = 64;

type LazySharedConnection = Arc<OnceCell<Connection>>;

#[derive(Clone)]
pub struct ConnectionCache {
    connections: Arc<Mutex<HashMap<RostraId, LazySharedConnection>>>,
    access_count: Arc<AtomicU64>,
}

impl Default for ConnectionCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionCache {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
            access_count: Arc::new(AtomicU64::new(0)),
        }
    }

    fn maybe_cleanup_closed(&self, connections: &mut HashMap<RostraId, LazySharedConnection>) {
        let removed = self.maybe_cleanup_cells(connections, |connection| !connection.is_closed());
        if 0 < removed {
            trace!(
                target: LOG_TARGET,
                removed,
                remaining = connections.len(),
                "Removed closed or abandoned connections from cache"
            );
        }
    }

    fn maybe_cleanup_cells<K, T>(
        &self,
        cells: &mut HashMap<K, Arc<OnceCell<T>>>,
        is_open: impl Fn(&T) -> bool,
    ) -> usize
    where
        K: Eq + Hash,
    {
        self.maybe_cleanup_cells_after_shared(cells, is_open, |_| {})
    }

    fn maybe_cleanup_cells_after_shared<K, T>(
        &self,
        cells: &mut HashMap<K, Arc<OnceCell<T>>>,
        is_open: impl Fn(&T) -> bool,
        mut after_shared_detected: impl FnMut(&K),
    ) -> usize
    where
        K: Eq + Hash,
    {
        let access_count = self.access_count.fetch_add(1, Ordering::Relaxed);
        if !access_count.is_multiple_of(CLEANUP_INTERVAL) {
            return 0;
        }

        let before = cells.len();
        cells.retain(|key, cell| {
            if let Some(cell) = Arc::get_mut(cell) {
                cell.get().is_some_and(&is_open)
            } else {
                after_shared_detected(key);
                cell.get().is_none_or(&is_open)
            }
        });
        before.saturating_sub(cells.len())
    }

    pub async fn get_or_connect(
        &self,
        networking: &ClientNetworking,
        id: RostraId,
    ) -> ConnectResult<Connection> {
        let mut pool_lock = self.connections.lock().await;
        self.maybe_cleanup_closed(&mut pool_lock);

        let entry_arc = pool_lock
            .entry(id)
            .and_modify(|entry_arc| {
                // Check if existing connection is disconnected and remove it
                if let Some(existing_conn) = entry_arc.get()
                    && existing_conn.is_closed() {
                        trace!(target: LOG_TARGET, %id, "Existing connection is disconnected, removing from pool");
                        *entry_arc = Arc::new(OnceCell::new());
                    }
            })
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone();

        // Drop the pool lock so other connections can work in parallel
        drop(pool_lock);

        let result = entry_arc
            .get_or_try_init(|| async {
                trace!(target: LOG_TARGET, %id, "Creating new connection");
                match networking.connect_uncached(id).await {
                    Ok(conn) => {
                        debug!(target: LOG_TARGET, %id, endpoint_id = %conn.remote_id().fmt_short(), "Connection successful");
                        Ok(conn)
                    }
                    Err(err) => {
                        trace!(target: LOG_TARGET, %id, err = %err, "Connection failed");
                        Err(err)
                    }
                }
            })
            .await;

        result.cloned()
    }

    async fn is_connected(&self, id: RostraId) -> bool {
        self.connections
            .lock()
            .await
            .get(&id)
            .and_then(|connection| connection.get())
            .is_some_and(|connection| !connection.is_closed())
    }

    /// Try to fetch an event from multiple peers with some parallelism.
    ///
    /// Returns `Some(event)` from the first peer that has it, or `None`.
    pub async fn get_event_from_peers(
        &self,
        networking: &ClientNetworking,
        peers: &[RostraId],
        author_id: RostraId,
        event_id: ShortEventId,
    ) -> Option<VerifiedEvent> {
        let result = futures_lite::StreamExt::find_map(
            &mut stream::iter(peers.iter().copied())
                .map(|peer_id| {
                    let cache = self.clone();
                    async move {
                        let conn = cache.get_or_connect(networking, peer_id).await.ok()?;
                        match conn.get_event(author_id, event_id).await {
                            Ok(Some(event)) => Some(event),
                            Ok(None) => {
                                debug!(
                                    target: LOG_TARGET,
                                    peer_id = %peer_id.to_short(),
                                    event_id = %event_id.to_short(),
                                    "Event not found on peer"
                                );
                                None
                            }
                            Err(_err) => {
                                debug!(
                                    target: LOG_TARGET,
                                    peer_id = %peer_id.to_short(),
                                    event_id = %event_id.to_short(),
                                    "Failed to fetch event from peer"
                                );
                                None
                            }
                        }
                    }
                })
                .buffer_unordered(4),
            |result| result,
        )
        .await;

        if result.is_none() {
            debug!(
                target: LOG_TARGET,
                event_id = %event_id.to_short(),
                "Event not found on any peer"
            );
        }

        result
    }

    /// Try to fetch event content from multiple peers with some parallelism.
    ///
    /// Reuse the shared store first, then reserve each racing read
    /// independently. Returns true when no further acquisition is needed or
    /// ingestion succeeded. Capacity refusal is a typed temporary DB error,
    /// never a peer failure.
    pub async fn fetch_event_content_from_peers(
        &self,
        networking: &ClientNetworking,
        peers: &[RostraId],
        event: VerifiedEvent,
        db: &rostra_client_db::Database,
    ) -> rostra_client_db::DbResult<bool> {
        use rostra_client_db::{DbError, PayloadReservationOutcome};

        let reservation = match db.prepare_payload_acquisition(&event).await? {
            PayloadReservationOutcome::Disabled => None,
            PayloadReservationOutcome::Reserved(reservation) => Some(reservation),
            PayloadReservationOutcome::Unneeded => return Ok(true),
            PayloadReservationOutcome::Deferred(reason) => {
                return Err(DbError::PayloadAdmissionPaused { reason });
            }
        };
        let preferred = self
            .is_connected(event.author())
            .await
            .then_some(event.author());
        let peers = crate::payload_holder_order::rank_payload_holders(
            event.event_id,
            peers.iter().copied(),
            preferred,
        );
        let result = crate::payload_read_race::race_payload_reads(
            &peers,
            || {
                let reservation = reservation.as_ref();
                let buffer = reservation.map(|r| r.try_acquire_buffer()).transpose()?;
                let conversion = reservation.map(|r| r.try_acquire_buffer()).transpose()?;
                Ok((buffer, conversion))
            },
            |peer_id, (buffer, conversion)| {
                let cache = self.clone();
                async move {
                    crate::task::outbound_deadline::within(
                        crate::task::outbound_deadline::PEER_OPERATION_DEADLINE,
                        async move {
                            let conn = cache.get_or_connect(networking, peer_id).await.ok()?;
                            match conn
                                .get_event_content_with_guard(event, (buffer, conversion))
                                .await
                            {
                                Ok(Some((content, (buffer, conversion)))) => {
                                    drop(conversion);
                                    Some(crate::acquired_payload::AcquiredPayload {
                                        content,
                                        buffer,
                                    })
                                }
                                Ok(None) => {
                                    debug!(
                                        target: LOG_TARGET,
                                        peer_id = %peer_id.to_short(),
                                        event_id = %event.event_id.to_short(),
                                        "Peer does not have content"
                                    );
                                    None
                                }
                                Err(_err) => {
                                    debug!(
                                        target: LOG_TARGET,
                                        peer_id = %peer_id.to_short(),
                                        event_id = %event.event_id.to_short(),
                                        "Failed to fetch content from peer"
                                    );
                                    None
                                }
                            }
                        },
                    )
                    .await
                    .ok()
                    .flatten()
                }
            },
        )
        .await
        .map_err(|reason| DbError::PayloadAdmissionPaused { reason })?;

        if let Some(payload) = result {
            payload.ingest(db).await?;
            return Ok(true);
        }
        {
            debug!(
                target: LOG_TARGET,
                event_id = %event.event_id.to_short(),
                "Event content not found from any peer"
            );
        }

        Ok(false)
    }
}

#[cfg(test)]
mod tests;
