//! Session-specific authorization and independent synchronizer tokens.

use axum::extract::FromRequestParts;
use axum::http::{StatusCode, request};
use axum::response::Response;
use rostra_core::id::RostraIdSecretKey;
use tower_sessions::Session;

use super::error_page;
use crate::SharedState;
use crate::routes::unlock::session::UserSession;

const CSRF_KEY: &str = "direct_messages_csrf";

/// Full unlocked request authority, never inferred from another session.
pub(crate) struct MessageSession {
    /// Identity and exact in-memory secret lookup key for this request.
    pub user: UserSession,
    /// This session's signing authority, never rendered or persisted.
    pub secret: RostraIdSecretKey,
    /// Persistent non-secret session metadata for synchronizer tokens.
    pub session: Session,
    /// The exact runtime authorized by extraction; never re-resolve by account.
    client: rostra_client::ClientHandle,
}

impl FromRequestParts<SharedState> for MessageSession {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut request::Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let denied = || {
            error_page(
                StatusCode::FORBIDDEN,
                "Unlock this session with your recovery phrase to use private messages.",
            )
        };
        let user = UserSession::from_request_parts(parts, state)
            .await
            .map_err(|_| denied())?;
        let secret = state.id_secret(user.session_token()).ok_or_else(denied)?;
        let handle = state.client(user.id()).await.map_err(|_| denied())?;
        handle
            .client_ref()
            .map_err(|_| denied())?
            .require_dm_authority(secret)
            .map_err(|_| denied())?;
        let session = Session::from_request_parts(parts, state)
            .await
            .map_err(|_| denied())?;
        Ok(Self {
            user,
            secret,
            session,
            client: handle,
        })
    }
}

impl MessageSession {
    /// Upgrade only the originally authorized runtime and recheck its
    /// authority.
    ///
    /// If eviction drops it, the request fails rather than selecting a newly
    /// loaded runtime for the same account. The returned reference keeps that
    /// checked runtime alive through the operation.
    pub fn client(&self) -> Option<rostra_client::ClientRef<'_>> {
        let client = self.client.app_ref_opt()?;
        client.require_dm_authority(self.secret).ok()?;
        Some(client)
    }

    /// Return a random non-credential token bound to this browser session.
    pub async fn csrf(&self) -> Result<String, Response> {
        let failure = || error_page(StatusCode::INTERNAL_SERVER_ERROR, "Session storage failed.");
        if let Some(token) = self
            .session
            .get::<String>(CSRF_KEY)
            .await
            .map_err(|_| failure())?
        {
            return Ok(token);
        }
        let token = data_encoding::HEXLOWER.encode(&rand::random::<[u8; 32]>());
        self.session
            .insert(CSRF_KEY, &token)
            .await
            .map_err(|_| failure())?;
        Ok(token)
    }

    /// Reject missing, malformed, or foreign tokens before any mutation.
    pub async fn check_csrf(&self, supplied: &str) -> Result<(), Response> {
        let expected = self.session.get::<String>(CSRF_KEY).await.map_err(|_| {
            error_page(StatusCode::INTERNAL_SERVER_ERROR, "Session storage failed.")
        })?;
        if supplied.len() == 64 && expected.as_deref() == Some(supplied) {
            return Ok(());
        }
        Err(error_page(
            StatusCode::FORBIDDEN,
            "This form has expired or belongs to another session. Reload the page and try again.",
        ))
    }

    /// Opaque exact-session key for account-local durable read markers.
    pub fn read_key(&self) -> [u8; 16] {
        self.user.session_token().to_le_bytes()
    }
}
