//! Public device metadata and explicit signed lifecycle actions.

use axum::Form;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use maud::html;
use serde::Deserialize;

use super::session::MessageSession;
use super::{
    MessagePageSection, MessageResult, access_error, error_page, page, publication_error,
    storage_error, take_page,
};
use crate::routes::fragment;

/// Exclusive own-device page cursor.
#[derive(Default, Deserialize)]
pub(crate) struct DevicesQuery {
    /// Hex-encoded device identifier of the last displayed row.
    after: Option<String>,
}

fn device_id(value: &str) -> Result<[u8; 16], &'static str> {
    data_encoding::HEXLOWER
        .decode(value.as_bytes())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or("Invalid message-device identifier.")
}

fn deadline(seconds: u64) -> String {
    crate::util::time::format_timestamp_iso(rostra_core::Timestamp::from(seconds))
}

/// Show public device state, immutable deadlines, and lifecycle forms.
pub(crate) async fn get_settings(
    session: MessageSession,
    Query(query): Query<DevicesQuery>,
) -> MessageResult {
    let client = session.client().ok_or_else(access_error)?;
    let local = client
        .db()
        .dm_local_installation()
        .await
        .map_err(storage_error)?;
    let devices = client
        .db()
        .dm_own_devices(
            query
                .after
                .as_deref()
                .map(device_id)
                .transpose()
                .map_err(|message| error_page(StatusCode::BAD_REQUEST, message))?,
            33,
        )
        .await
        .map_err(storage_error)?;
    let csrf = session.csrf().await?;
    let now = rostra_core::Timestamp::now().as_u64();
    let (devices, has_more) = take_page(devices);
    let next = has_more.then(|| {
        format!(
            "/settings/messages?after={}",
            data_encoding::HEXLOWER.encode(&devices.last().expect("nonempty page").0)
        )
    });
    Ok(page(
        "Message devices",
        Some(MessagePageSection::Devices),
        html! {
            p { "Each installation has independent random message keys. Your recovery phrase does not recover old message keys or this installation's history." }
            p { "New local keys are eligible for sending for 7 days, then receive-only for 28 more days. Each key keeps its original deletion deadline. Local plaintext history currently has no automatic expiry." }
            @if let Some(local) = &local {
                p ."m-directMessages__identity" {
                    "This installation: " code { (data_encoding::HEXLOWER.encode(&local.device_id)) }
                    @if local.retired { " (permanently retired)" }
                }
            } @else {
                p { "This installation has not enrolled a message device yet." }
            }
            @if local.as_ref().is_none_or(|local| local.retired) {
                form method="post" action="/settings/messages" {
                    input type="hidden" name="csrf" value=(&csrf);
                    input type="hidden" name="action" value="reenroll";
                    (fragment::button("m-directMessages__enrollButton", "Enroll")
                        .aria_label("Enroll a new message device").call())
                }
            }
            p { "Retirement is permanent for a device ID. It stops future selection after senders learn it; it cannot revoke existing ciphertext or wipe history. This installation stops sending when retired, but its old keys can still receive until their original deadlines. Enroll again to use a fresh device ID." }
            @if devices.is_empty() { p { "No device announcements are known yet." } }
            ul ."m-directMessages__devices" {
                @for (device, device_state) in &devices {
                    li {
                        h2 ."m-directMessages__identity" {
                            code { (data_encoding::HEXLOWER.encode(device)) }
                            @if local.as_ref().is_some_and(|local| local.device_id == *device) { " · This installation" }
                        }
                        @if device_state.retired() {
                            p { "Permanently retired" }
                        } @else if let Some((published, _, epoch)) = device_state.latest() {
                            p {
                                @if epoch.eligible(now, published) { "Eligible for new messages" }
                                @else { "Not eligible for new messages" }
                            }
                            dl {
                                dt { "Send from" } dd { (deadline(epoch.send_from)) }
                                dt { "Send until" } dd { (deadline(epoch.send_until)) }
                                dt { "Declared receive-key deletion deadline" } dd { (deadline(epoch.decrypt_until)) }
                            }
                        }
                        @if !device_state.retired() {
                            a href=(format!("/settings/messages/retire/{}", data_encoding::HEXLOWER.encode(device))) {
                                "Retire this device…"
                            }
                        }
                    }
                }
            }
            p { "These are the latest public announcements, not proof that another device erased its keys. Older local receive-only keys keep their original deadlines." }
            @if let Some(next) = next { a href=(next) { "More devices" } }
        },
    ))
}

/// Confirm the exact irreversible retirement through an ordinary HTML page.
pub(crate) async fn get_retirement(
    session: MessageSession,
    Path(device): Path<String>,
) -> MessageResult {
    let id = device_id(&device).map_err(|message| error_page(StatusCode::BAD_REQUEST, message))?;
    let client = session.client().ok_or_else(access_error)?;
    let local = client
        .db()
        .dm_local_installation()
        .await
        .map_err(storage_error)?;
    let csrf = session.csrf().await?;
    Ok(page(
        "Confirm device retirement",
        Some(MessagePageSection::Devices),
        html! {
            p ."m-directMessages__identity" { "Device: " code { (data_encoding::HEXLOWER.encode(&id)) } }
            @if local.is_some_and(|local| local.device_id == id) {
                p { "This is the current installation. Retiring it stops new sends until you explicitly enroll a new device ID." }
            }
            p { "Retirement is permanent for this device ID. It stops future selection once senders learn it, but does not revoke existing ciphertext or erase history. Old receive-only keys retain their original deadlines." }
            form method="post" action="/settings/messages" {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="action" value="retire";
                input type="hidden" name="device" value=(data_encoding::HEXLOWER.encode(&id));
                (fragment::button("m-directMessages__retireButton", "Retire")
                    .aria_label("Permanently retire this device").call())
            }
            a href="/settings/messages" { "Cancel" }
        },
    ))
}

/// Explicit device lifecycle action, protected by a session synchronizer token.
#[derive(Deserialize)]
pub(crate) struct DeviceForm {
    /// Independent session-bound CSRF token.
    csrf: String,
    /// Valid lifecycle transitions carry only their required input.
    #[serde(flatten)]
    action: DeviceAction,
}

/// The two supported lifecycle transitions have distinct required fields.
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
enum DeviceAction {
    /// Permanently retire an own-account public device ID.
    Retire {
        /// Canonical hexadecimal public device identifier.
        device: String,
    },
    /// Explicitly create a fresh local identity after retirement.
    Reenroll {},
}

/// Sign a retirement or explicitly enroll a new local device, then redirect.
pub(crate) async fn post_settings(
    session: MessageSession,
    Form(form): Form<DeviceForm>,
) -> MessageResult {
    session.check_csrf(&form.csrf).await?;
    let client = session.client().ok_or_else(access_error)?;
    match form.action {
        DeviceAction::Retire { device } => {
            let device = device_id(&device)
                .map_err(|message| error_page(StatusCode::BAD_REQUEST, message))?;
            client
                .retire_direct_message_device(session.secret, device)
                .await
                .map_err(|error| {
                    let (status, message) = publication_error(error, "retire");
                    error_page(status, message)
                })?;
        }
        DeviceAction::Reenroll {} => {
            client
                .reenroll_direct_messages(session.secret)
                .await
                .map_err(|error| {
                    let (status, message) = publication_error(error, "reenroll");
                    error_page(status, message)
                })?;
        }
    }
    Ok(Redirect::to("/settings/messages").into_response())
}
