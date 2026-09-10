use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::ops::Bound::{Excluded, Unbounded};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use rostra_client_db::{Database, DbError, PayloadAccount, PayloadAccountAttachError};
use rostra_core::id::RostraId;
use rostra_util_error::FmtCompact as _;
use snafu::{ResultExt as _, Snafu};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::client::task_owner::ClientTaskOwner;
use crate::error::InitError;
use crate::{Client, ClientHandle, LOG_TARGET, PkarrClient};

mod retired_client;
#[cfg(test)]
mod tests;

use self::retired_client::RetiredClient;

#[derive(Debug, Snafu)]
pub enum MultiClientError {
    #[snafu(display("Failed to initialize client"))]
    ClientInit { source: InitError },
    #[snafu(display("Failed to open client database"))]
    Database { source: DbError },
    #[snafu(transparent)]
    Io { source: io::Error },
    /// Account identity, configuration or exclusive attachment rejected
    /// startup.
    #[snafu(display("Failed to attach payload account: {source}"))]
    PayloadAccount { source: PayloadAccountAttachError },
    /// Duplicate identity entries cannot silently replace startup ownership.
    #[snafu(display("Duplicate payload account: {id}"))]
    DuplicatePayloadAccount { id: RostraId },
    /// A manager-owned initialization task failed before returning its result.
    #[snafu(display("Client initialization task failed: {source}"))]
    LoadTask { source: tokio::task::JoinError },
    /// Blocking storage-handle closure failed before a cold load could proceed.
    #[snafu(display("Retired database close task failed: {source}"))]
    DatabaseCloseTask { source: tokio::task::JoinError },
}

pub type MultiClientResult<T> = std::result::Result<T, MultiClientError>;
struct ClientInfo {
    client: Arc<Client>,
    last_used: Instant,
}

#[derive(Clone)]
pub struct MultiClient {
    data_dir: PathBuf,
    inner: Arc<tokio::sync::RwLock<HashMap<RostraId, ClientInfo>>>,
    max_clients: usize,
    usage_queue: Arc<tokio::sync::RwLock<VecDeque<RostraId>>>,
    /// When true, allows direct IP connections (exposes IP address).
    /// When false (default), uses relay-only mode for privacy.
    public_mode: bool,
    /// Shared pkarr client reused across all Rostra client instances.
    pkarr_client: Arc<PkarrClient>,
    /// Immutable account policy handles, available without opening databases.
    payload_accounts: Arc<HashMap<RostraId, PayloadAccount>>,
    /// Serializes the existing open/build/eviction/publication sequence.
    load_lock: Arc<tokio::sync::Mutex<()>>,
    /// Retired clients are weak; storage stays discoverable until quiescent
    /// cleanup on a later cold load or periodic reaper pass.
    retired: Arc<RwLock<BTreeMap<RostraId, RetiredClient>>>,
    /// Exclusive ordered reaper cursor prevents live entries starving cleanup.
    retired_cursor: Arc<std::sync::Mutex<Option<RostraId>>>,
    /// At most one weak-manager task reaps retired storage after final release.
    retired_cleanup_running: Arc<AtomicBool>,
    /// Disposable test barrier after attachment and before client publication.
    #[cfg(test)]
    load_pause: Arc<std::sync::Mutex<Option<Arc<tests::LoadPause>>>>,
    /// One-shot failure injection inside construction after task startup.
    #[cfg(test)]
    build_hook: Arc<std::sync::Mutex<Option<crate::client::task_owner::BuildHook>>>,
}

impl MultiClient {
    pub fn new(
        data_dir: PathBuf,
        max_clients: usize,
        public_mode: bool,
        pkarr_client: Arc<PkarrClient>,
    ) -> Self {
        Self {
            data_dir,
            inner: Arc::new(RwLock::new(Default::default())),
            max_clients: max_clients.max(1), // Ensure at least 1 client
            usage_queue: Arc::new(RwLock::new(VecDeque::new())),
            public_mode,
            pkarr_client,
            payload_accounts: Arc::default(),
            load_lock: Arc::default(),
            retired: Arc::default(),
            retired_cursor: Arc::default(),
            retired_cleanup_running: Arc::default(),
            #[cfg(test)]
            load_pause: Arc::default(),
            #[cfg(test)]
            build_hook: Arc::default(),
        }
    }

    /// Construct a manager with immutable, explicitly provisioned account
    /// ledgers.
    ///
    /// Configuration is installed before the manager is published. There is no
    /// hot reload: disabled acquisitions cannot be retroactively charged.
    pub fn new_with_payload_accounts(
        data_dir: PathBuf,
        max_clients: usize,
        public_mode: bool,
        pkarr_client: Arc<PkarrClient>,
        accounts: impl IntoIterator<Item = PayloadAccount>,
    ) -> MultiClientResult<Self> {
        let mut payload_accounts = HashMap::new();
        for account in accounts {
            let id = account.id();
            if payload_accounts.insert(id, account).is_some() {
                return DuplicatePayloadAccountSnafu { id }.fail();
            }
        }
        Ok(Self {
            payload_accounts: Arc::new(payload_accounts),
            ..Self::new(data_dir, max_clients, public_mode, pkarr_client)
        })
    }

    /// Return configured preparse ownership without loading or creating
    /// storage.
    ///
    /// Unconfigured identities remain disabled and do not allocate registry
    /// rows from attacker-controlled HTTP paths.
    pub fn payload_account(&self, id: RostraId) -> Option<PayloadAccount> {
        self.payload_accounts.get(&id).cloned()
    }
}

impl MultiClient {
    /// Load or reuse an identity runtime, preserving live ownership across LRU
    /// eviction.
    ///
    /// A cold miss starts manager-owned initialization, including database
    /// creation/opening, migration, optional compaction and networking setup.
    /// Cancelling this caller does not cancel that initialization: it completes
    /// publication or failure and then releases its manager ownership. A live
    /// caller receives the initialization result (or `LoadTask` if its task
    /// panicked); a cancelled sole caller receives nothing. Failed-build
    /// storage remains discoverable for reuse or periodic cleanup. Other
    /// callers retry or reuse under the serialized load boundary, not the
    /// cancelled caller's abandoned result.
    pub async fn load(&self, id: RostraId) -> MultiClientResult<Arc<Client>> {
        if let Some(client) = self.loaded_client(id).await {
            return Ok(client);
        }
        // Initialization retains its manager until publication, even if this
        // request is cancelled while storage or networking is being opened.
        let manager = self.clone();
        tokio::spawn(async move { manager.load_inner(id).await })
            .await
            .context(LoadTaskSnafu)?
    }

    /// Serialize cold loads and keep every surviving ownership path
    /// discoverable.
    async fn load_inner(&self, id: RostraId) -> MultiClientResult<Arc<Client>> {
        let _load = self.load_lock.lock().await;
        // A preceding loader may have published this account while we waited.
        if let Some(client) = self.loaded_client(id).await {
            return Ok(client);
        }

        // Client not loaded, need to load it
        let load_start = Instant::now();

        let (client, database, networking, tasks) = {
            let mut retired = self.retired.write().await;
            let mut cursor = *self.retired_cursor.lock().unwrap();
            Self::reap_retired(&mut retired, &mut cursor).await?;
            *self.retired_cursor.lock().unwrap() = cursor;
            retired
                .get(&id)
                .map(|entry| {
                    (
                        entry.client.upgrade(),
                        Some(entry.database.clone()),
                        entry.networking.upgrade(),
                        entry.tasks.clone(),
                    )
                })
                .unwrap_or_default()
        };
        let client = if let Some(client) = client {
            client
        } else {
            if let Some(tasks) = tasks {
                #[cfg(test)]
                if let Some(pause) = self.load_pause.lock().unwrap().as_ref() {
                    pause.waiting_teardown.notify_one();
                }
                tasks.terminated().await;
            }
            let db = if let Some(db) = database {
                db
            } else {
                Arc::new(self.open_database(id, load_start).await?)
            };
            #[cfg(test)]
            {
                let pause = self.load_pause.lock().unwrap().take();
                if let Some(pause) = pause {
                    pause.opened.notify_one();
                    pause.resume.notified().await;
                }
            }
            let task_owner = ClientTaskOwner::default();
            #[cfg(test)]
            let task_owner = {
                let mut task_owner = task_owner;
                *task_owner.after_start.get_mut().unwrap() = self.build_hook.lock().unwrap().take();
                task_owner
            };
            // Retain completion before construction can start any tasks. The
            // unique owner aborts on partial-build failure as well as Client drop.
            self.retired.write().await.insert(
                id,
                RetiredClient {
                    client: Default::default(),
                    database: db.clone(),
                    networking: Default::default(),
                    tasks: Some(task_owner.clone()),
                },
            );
            self.start_retired_cleanup();
            Client::from_shared_database(
                id,
                db,
                self.public_mode,
                self.pkarr_client.clone(),
                networking.map(|networking| networking.endpoint.clone()),
                task_owner,
            )
            .await
            .context(ClientInitSnafu)?
        };
        debug!(target: LOG_TARGET, id = %id, elapsed_ms = %load_start.elapsed().as_millis(), "Client built or reused");

        self.maybe_evict_clients().await?;
        self.inner.write().await.insert(
            id,
            ClientInfo {
                client: client.clone(),
                last_used: Instant::now(),
            },
        );
        self.retired.write().await.remove(&id);
        self.update_usage_queue(id).await;
        Ok(client)
    }

    /// Open fresh storage only after all earlier database ownership has ended.
    async fn open_database(
        &self,
        id: RostraId,
        load_start: Instant,
    ) -> MultiClientResult<Database> {
        let db_path = Database::mk_db_path(&self.data_dir, id).await?;
        let compact = db_path.exists();
        let mut db = Database::open(&db_path, id).await.context(DatabaseSnafu)?;
        debug!(target: LOG_TARGET, id = %id, elapsed_ms = %load_start.elapsed().as_millis(), "Database opened");

        if compact {
            if let Err(err) = db.compact().await {
                warn!(
                    target: LOG_TARGET,
                    err = %err.fmt_compact(),
                    path=%db_path.display(),
                    "Failed to compact database"
                );
            }
            debug!(target: LOG_TARGET, id = %id, elapsed_ms = %load_start.elapsed().as_millis(), "Database compacted");
        }

        if let Some(account) = self.payload_accounts.get(&id) {
            db.attach_payload_account(account)
                .context(PayloadAccountSnafu)?;
        }

        Ok(db)
    }

    /// Close sole-owned retired databases while holding the load lock.
    ///
    /// Atomic unwrap excludes future weak upgrades; dropping the database here
    /// finishes file closure before a cold load can try to open it again.
    /// Visits at most 32 entries and checks a 10-ms cooperative budget between
    /// entries. One database close is indivisible and may exceed that budget.
    async fn reap_retired(
        retired: &mut BTreeMap<RostraId, RetiredClient>,
        cursor: &mut Option<RostraId>,
    ) -> MultiClientResult<()> {
        let started = Instant::now();
        let keys: Vec<_> = retired
            .range((cursor.map_or(Unbounded, Excluded), Unbounded))
            .take(32)
            .map(|(id, _)| *id)
            .collect();
        if keys.is_empty() {
            *cursor = None;
            return Ok(());
        }
        for id in keys {
            let Some(entry) = retired.remove(&id) else {
                continue;
            };
            if entry
                .tasks
                .as_ref()
                .is_some_and(|tasks| tasks.terminated().now_or_never().is_none())
            {
                retired.insert(id, entry);
            } else {
                let RetiredClient {
                    client,
                    database,
                    networking,
                    tasks,
                } = entry;
                match Arc::try_unwrap(database) {
                    Ok(database) => tokio::task::spawn_blocking(move || drop(database))
                        .await
                        .context(DatabaseCloseTaskSnafu)?,
                    Err(database) => {
                        retired.insert(
                            id,
                            RetiredClient {
                                client,
                                database,
                                networking,
                                tasks,
                            },
                        );
                    }
                }
            }
            *cursor = Some(id);
            if started.elapsed() >= Duration::from_millis(10) {
                break;
            }
        }
        Ok(())
    }

    /// Reap released storage without keeping the manager alive while idle.
    fn start_retired_cleanup(&self) {
        if self.retired_cleanup_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let retired = Arc::downgrade(&self.retired);
        let load_lock = Arc::downgrade(&self.load_lock);
        let running = self.retired_cleanup_running.clone();
        let cursor = self.retired_cursor.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let (Some(retired), Some(load_lock)) = (retired.upgrade(), load_lock.upgrade())
                else {
                    running.store(false, Ordering::Release);
                    return;
                };
                let _load = load_lock.lock().await;
                let mut retired = retired.write().await;
                let mut next_cursor = *cursor.lock().unwrap();
                if let Err(err) = Self::reap_retired(&mut retired, &mut next_cursor).await {
                    warn!(target: LOG_TARGET, err = %err.fmt_compact(), "Retired database cleanup failed");
                    running.store(false, Ordering::Release);
                    return;
                }
                *cursor.lock().unwrap() = next_cursor;
                if retired.is_empty() {
                    // Still under load_lock: a new retire cannot miss restarting
                    // cleanup between this flag reset and task completion.
                    running.store(false, Ordering::Release);
                    return;
                }
            }
        });
    }

    // Helper method to update the usage queue
    async fn update_usage_queue(&self, id: RostraId) {
        let mut queue = self.usage_queue.write().await;

        // Remove the ID if it's already in the queue
        if let Some(pos) = queue.iter().position(|&x| x == id) {
            queue.remove(pos);
        }

        // Add the ID to the front of the queue
        queue.push_front(id);
    }

    // Helper method to evict clients if we're over the limit
    async fn maybe_evict_clients(&self) -> MultiClientResult<()> {
        let mut write = self.inner.write().await;

        // If we're under the limit, no need to evict
        if write.len() < self.max_clients {
            return Ok(());
        }

        // Get the least recently used clients from the queue
        let to_evict = {
            let queue = self.usage_queue.read().await;

            // Get the IDs of clients to evict (from the back of the queue)
            let num_to_evict = write.len() + 1 - self.max_clients;
            queue
                .iter()
                .rev()
                .take(num_to_evict)
                .cloned()
                .collect::<Vec<_>>()
        };

        // Evict the clients
        for id in to_evict {
            if let Some(info) = write.remove(&id) {
                self.retired.write().await.insert(
                    id,
                    RetiredClient {
                        client: Arc::downgrade(&info.client),
                        database: info.client.db().clone(),
                        networking: Arc::downgrade(info.client.networking()),
                        tasks: Some(info.client.task_handles.clone()),
                    },
                );
                self.start_retired_cleanup();
                info!(
                    target: LOG_TARGET,
                    id = %id,
                    "Evicted client due to max_clients limit"
                );
            }
        }

        // Update the usage queue
        {
            let mut queue = self.usage_queue.write().await;
            queue.retain(|id| write.contains_key(id));
        }

        Ok(())
    }

    pub async fn get(&self, id: RostraId) -> Option<ClientHandle> {
        self.loaded_client(id).await.map(|client| client.handle())
    }

    /// Touch an existing client without waiting for unrelated lazy loading.
    async fn loaded_client(&self, id: RostraId) -> Option<Arc<Client>> {
        let mut write = self.inner.write().await;
        if let Some(client_info) = write.get_mut(&id) {
            // Update last used time
            client_info.last_used = Instant::now();

            // Update usage queue
            self.update_usage_queue(id).await;

            Some(client_info.client.clone())
        } else {
            None
        }
    }
}
