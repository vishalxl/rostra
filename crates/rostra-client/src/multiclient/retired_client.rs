use std::sync::{Arc, Weak};

use rostra_client_db::Database;

use crate::Client;
use crate::client::tasks::ClientTasks;
use crate::net::ClientNetworking;

/// Discoverable ownership after the strong client LRU entry is removed.
pub(super) struct RetiredClient {
    /// Reuse the complete runtime while a request still owns it.
    pub(super) client: Weak<Client>,
    /// Reuse live storage; cleanup atomically unwraps sole ownership before
    /// closing it, so a concurrent weak upgrade cannot race a fresh open.
    pub(super) database: Arc<Database>,
    /// Reuse the endpoint while old networking operations are winding down.
    pub(super) networking: Weak<ClientNetworking>,
    /// Old tasks must actually terminate before a DB-only runtime rebuild.
    pub(super) tasks: Option<Arc<ClientTasks>>,
}
