//! Plain-text, HTML-first private-message workflows with session-only access.

mod session;
mod settings;
#[cfg(test)]
mod tests;

use axum::Form;
use axum::extract::{OriginalUri, Path, Query};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{DOCTYPE, Markup, html};
use rostra_core::ShortEventId;
use rostra_core::id::{RostraId, ToShort as _};
use serde::Deserialize;

use self::session::MessageSession;
pub(super) use self::settings::{get_retirement, get_settings, post_settings};
use super::url::{RostraPathId, redirect_to_canonical};
use super::{Maud, fragment, recovery};

type MessageResult = Result<Response, Response>;

/// Apply private-response controls, including errors and redirects.
pub(super) fn sensitive_response(body: impl IntoResponse) -> Response {
    let mut response = body.into_response();
    if response.status().is_client_error() || response.status().is_server_error() {
        let is_html = response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|value| value.as_bytes().starts_with(b"text/html"));
        if !is_html {
            let page = error_page(
                response.status(),
                match response.status() {
                    StatusCode::PAYLOAD_TOO_LARGE => {
                        "The submitted form is too large. Messages must be at most 16 KiB of UTF-8."
                    }
                    StatusCode::NOT_FOUND => "This private-message page does not exist.",
                    _ => {
                        "The private-message request could not be processed. Return to conversations and try again."
                    }
                },
            );
            response.headers_mut().remove(header::CONTENT_LENGTH);
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            *response.body_mut() = page.into_body();
        }
    }
    let mut response = recovery::sensitive_response(response);
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'self'; img-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Render a complete private page without scripts, embeds, or remote resources.
fn page(title: &str, content: Markup) -> Response {
    Maud(html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                meta name="color-scheme" content="light dark";
                meta name="robots" content="noindex";
                title { (title) " — Rostra" }
                link rel="stylesheet" href="/assets/style.css";
            }
            body ."o-body" {
                div ."o-pageLayout" {
                    nav ."o-navBar" aria-label="Private messages" {
                        div ."o-topNav" {
                            a ."o-topNav__item" href="/following" { "Back to timeline" }
                            a ."o-topNav__item" href="/messages" { "Conversations" }
                            a ."o-topNav__item" href="/settings/messages" { "Message devices" }
                            a ."o-topNav__item" href="/unlock" { "Unlock session" }
                        }
                    }
                    main ."o-mainBarTimeline m-directMessages" {
                        div ."o-mainBarTimeline__tabs" { h1 { (title) } }
                        div ."o-settingsContent" { (content) }
                    }
                }
            }
        }
    })
    .into_response()
}

fn error_page(status: StatusCode, message: &str) -> Response {
    let mut response = page(
        "Private messages",
        html! {
            p role="alert" { (message) }
            p { a href="/messages" { "Return to conversations" } }
        },
    );
    *response.status_mut() = status;
    response
}

fn storage_error(error: rostra_client_db::DbError) -> Response {
    tracing::error!(target: "rostra::direct_messages::http", operation = "read", error = %error,
        "Private-message storage operation failed");
    error_page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Private message storage is unavailable.",
    )
}

/// Preserve expected availability failures separately from internal failures.
fn publication_error(
    error: rostra_client::error::PostError,
    operation: &'static str,
) -> (StatusCode, &'static str) {
    use rostra_client::error::PostError;
    use rostra_client_db::DbError;
    match error {
        PostError::DirectMessageUnavailable
        | PostError::Storage {
            source: DbError::DmRecipientUnavailable,
        } => (
            StatusCode::BAD_REQUEST,
            "Sending is unavailable. Check this installation and the recipient's eligible devices. No message was queued.",
        ),
        PostError::Storage {
            source: DbError::PayloadAdmissionPaused { .. },
        } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Local storage admission is temporarily paused. Nothing was queued; retry after capacity is available.",
        ),
        error => {
            tracing::error!(target: "rostra::direct_messages::http", operation, error = %error,
                "Private-message publication failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The operation could not finish because of an internal or storage failure. Reload before retrying.",
            )
        }
    }
}

fn access_error() -> Response {
    error_page(
        StatusCode::FORBIDDEN,
        "This authorized client is no longer available. Unlock this session again.",
    )
}

/// Keep a rendered page only after probing one additional bounded row.
fn take_page<T>(mut rows: Vec<T>) -> (Vec<T>, bool) {
    let has_more = rows.len() > 32;
    rows.truncate(32);
    (rows, has_more)
}

pub(super) fn thread_url(peer: RostraId) -> String {
    format!("/messages/{}", peer.to_short())
}

fn peer_of(entry: &rostra_client_db::dm::HistoryEntry, own: RostraId) -> RostraId {
    if entry.sender == own {
        entry.recipient
    } else {
        entry.sender
    }
}

/// Exclusive conversation-list cursor in sorted participant-pair order.
#[derive(Default, Deserialize)]
pub(super) struct ConversationsQuery {
    /// First member of the last displayed pair.
    first: Option<RostraId>,
    /// Second member of the last displayed pair.
    second: Option<RostraId>,
}

/// List a bounded page of conversations without reading unrelated history.
pub(super) async fn get_messages(
    session: MessageSession,
    Query(query): Query<ConversationsQuery>,
) -> MessageResult {
    let client = session.client().ok_or_else(access_error)?;
    let entries = client
        .db()
        .dm_conversations(query.first.zip(query.second), 33)
        .await
        .map_err(storage_error)?;
    let (entries, has_more) = take_page(entries);
    let next = has_more.then(|| {
        let last = entries.last().expect("nonempty page");
        format!(
            "/messages?first={}&second={}",
            last.sender.min(last.recipient),
            last.sender.max(last.recipient)
        )
    });
    Ok(page(
        "Private messages",
        html! {
            p { "Messages are encrypted for selected devices. Local history is retained on this installation; it does not automatically appear on a new device." }
            form method="get" action="/messages/open" {
                label for="message-peer" { "Recipient's Rostra ID" }
                input id="message-peer" name="peer" type="text" required autocomplete="off";
                (fragment::button("m-directMessages__openButton", "Open").call())
            }
            @if entries.is_empty() { p { "No conversations on this installation yet." } }
            ul ."m-directMessages__conversations" {
                @for entry in &entries {
                    li {
                        a href=(thread_url(peer_of(entry, session.user.id()))) {
                            (peer_of(entry, session.user.id()).to_short())
                        }
                        p ."m-directMessages__text" {
                            (entry.text.chars().take(160).collect::<String>())
                            @if entry.text.chars().count() > 160 { "…" }
                        }
                    }
                }
            }
            @if let Some(next) = next { a href=(next) { "More conversations" } }
        },
    ))
}

/// Recipient submitted through an ordinary GET form.
#[derive(Deserialize)]
pub(super) struct OpenQuery {
    /// Full or locally resolvable short identity, never a secret.
    peer: RostraPathId,
}

/// Open only a resolvable identity so generated short URLs remain usable.
pub(super) async fn open_conversation(
    session: MessageSession,
    Query(query): Query<OpenQuery>,
) -> MessageResult {
    let client = session.client().ok_or_else(access_error)?;
    let peer = resolve_peer(query.peer, client.db()).await?;
    Ok(Redirect::to(&thread_url(peer)).into_response())
}

async fn resolve_peer(
    path: RostraPathId,
    db: &rostra_client_db::Database,
) -> Result<RostraId, Response> {
    let unavailable = || {
        error_page(
            StatusCode::NOT_FOUND,
            "This identity is not known on this installation. Obtain its events and an eligible message-device announcement before sending.",
        )
    };
    let peer = path.resolve(db).await.ok_or_else(unavailable)?;
    if db.get_known_identity(peer.to_short()).await != Some(peer) {
        return Err(unavailable());
    }
    Ok(peer)
}

/// Exclusive thread-history cursor.
#[derive(Default, Deserialize)]
pub(super) struct ThreadQuery {
    /// Signed timestamp of the oldest displayed message.
    before_time: Option<u64>,
    /// Event-ID tie breaker.
    before_event: Option<ShortEventId>,
}

/// Render retained history and an ordinary no-JavaScript composer.
pub(super) async fn get_thread(
    session: MessageSession,
    Path(path): Path<RostraPathId>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<ThreadQuery>,
) -> MessageResult {
    let client = session.client().ok_or_else(access_error)?;
    let peer = resolve_peer(path, client.db()).await?;
    if let Some(response) = redirect_to_canonical(&uri, thread_url(peer)) {
        return Ok(response);
    }
    render_thread(client.db(), &session, peer, query, "", None).await
}

async fn render_thread(
    db: &rostra_client_db::Database,
    session: &MessageSession,
    peer: RostraId,
    query: ThreadQuery,
    draft: &str,
    error: Option<(StatusCode, &str)>,
) -> MessageResult {
    let entries = db
        .dm_history_with(peer, query.before_time.zip(query.before_event), 33)
        .await
        .map_err(storage_error)?;
    let csrf = session.csrf().await?;
    let (entries, has_more) = take_page(entries);
    let next = has_more.then(|| {
        let oldest = entries.last().expect("nonempty page");
        format!(
            "{}?before_time={}&before_event={}",
            thread_url(peer),
            oldest.timestamp,
            oldest.event_id
        )
    });
    let unavailable = match db.dm_destinations_now(peer).await {
        Ok(_) => false,
        Err(rostra_client_db::DbError::DmRecipientUnavailable) => true,
        Err(error) => return Err(storage_error(error)),
    };
    let mut response = page(
        "Conversation",
        html! {
            p ."m-directMessages__identity" { "With " (peer) }
            p { "Only selected devices can decrypt new messages. There are no delivery or read receipts." }
            @if let Some((_, error)) = error { p role="alert" { (error) } }
            @if unavailable {
                p role="status" { "Sending is unavailable: this installation may be retired, or no eligible recipient device is known. No message will be queued." }
            }
            @if let Some(next) = next { a href=(next) { "Older messages" } }
            @if entries.is_empty() { p { "No messages on this installation yet." } }
            ol ."m-directMessages__history" {
                @for entry in entries.iter().rev() {
                    li ."m-directMessages__message" ."-outgoing"[entry.sender == session.user.id()] {
                        p {
                            strong { @if entry.sender == session.user.id() { "You" } @else { "Peer" } }
                            " · "
                            (crate::util::time::format_timestamp(rostra_core::Timestamp::from(entry.timestamp)))
                        }
                        p ."m-directMessages__text" { (&entry.text) }
                        @if entry.conflicted {
                            p role="status" { "A conflicting authenticated message reused this message ID. The first saved text is shown." }
                        }
                    }
                }
            }
            form method="post" action=(thread_url(peer)) {
                input type="hidden" name="csrf" value=(csrf);
                label for="message-text" { "Plain-text message (up to 16 KiB of UTF-8)" }
                textarea id="message-text" name="text" rows="5" required
                    maxlength="16384" autocomplete="off" { (draft) }
                (fragment::button("m-directMessages__sendButton", "Send").call())
            }
            p { a href=(thread_url(peer)) { "Refresh latest messages" } }
        },
    );
    if let Some((status, _)) = error {
        *response.status_mut() = status;
    }
    Ok(response)
}

/// Plain-text send form; never derive Debug for plaintext-bearing input.
#[derive(Deserialize)]
pub(super) struct SendForm {
    /// Independent session-bound CSRF token.
    csrf: String,
    /// User text, encoded only after all authorization checks.
    text: String,
}

/// Send once through an ordinary POST followed by a 303 redirect.
pub(super) async fn post_message(
    session: MessageSession,
    Path(path): Path<RostraPathId>,
    Form(form): Form<SendForm>,
) -> MessageResult {
    session.check_csrf(&form.csrf).await?;
    let client = session.client().ok_or_else(access_error)?;
    let peer = resolve_peer(path, client.db()).await?;
    let error = if form.text.is_empty() || form.text.len() > 16 * 1024 {
        Some((
            StatusCode::BAD_REQUEST,
            "Enter a nonempty message of at most 16 KiB of UTF-8. Nothing was sent.",
        ))
    } else {
        client
            .send_direct_message(session.secret, peer, form.text.clone())
            .await
            .err()
            .map(|error| publication_error(error, "send"))
    };
    if let Some(error) = error {
        return render_thread(
            client.db(),
            &session,
            peer,
            ThreadQuery::default(),
            &form.text,
            Some(error),
        )
        .await;
    }
    Ok(Redirect::to(&thread_url(peer)).into_response())
}
