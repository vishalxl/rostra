use std::sync::Arc;

use rostra_client_db::Database;
use rostra_core::id::{RostraId, RostraIdSecretKey};

use super::task_owner::ClientTaskOwner;
use crate::PkarrClient;

/// Internal initialization inputs; shared storage is not a public builder path.
pub(super) struct ClientInit {
    /// Abort ownership exists before initialization can fail or start tasks.
    pub(super) task_owner: ClientTaskOwner,
    /// Account whose runtime is being constructed.
    pub(super) id: RostraId,
    /// Whether to accept peer requests.
    pub(super) start_request_handler: bool,
    /// Whether full-client maintenance tasks should start.
    pub(super) start_background_tasks: bool,
    /// Storage owned by this runtime or safely recovered by its manager.
    pub(super) db: Option<Arc<Database>>,
    /// Optional signing authority.
    pub(super) secret: Option<RostraIdSecretKey>,
    /// Explicit permission for direct IP transport.
    pub(super) public_mode: bool,
    /// Optional endpoint recovered after old request handlers have terminated.
    pub(super) iroh_endpoint: Option<iroh::Endpoint>,
    /// Shared identity discovery service.
    pub(super) pkarr_client: Option<Arc<PkarrClient>>,
}
