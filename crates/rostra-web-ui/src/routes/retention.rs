//! Session-scoped, read-only presentation of existing bounded observations.

use axum::extract::State;
use axum::response::Response;
use maud::{Markup, html};
use rostra_client_db::DryRunReport;

use super::unlock::session::UserSession;
use super::{Maud, recovery};
use crate::SharedState;
use crate::error::{ReadOnlyModeSnafu, RequestResult};

#[cfg(test)]
mod tests;

/// Render diagnostics only for the authenticated storing account, never a query
/// ID.
pub async fn get_retention(
    state: State<SharedState>,
    session: UserSession,
) -> RequestResult<Response> {
    state
        .id_secret(session.session_token())
        .ok_or_else(|| ReadOnlyModeSnafu.build())?;
    let client = state.client(session.id()).await?;
    let client = client.client_ref()?;
    let db = client.db();
    let mode = db.payload_retention_mode();
    let report = db.payload_retention_forecast();
    let sampled_at = rostra_core::Timestamp::now();
    let usage = db.get_payload_usage().await;
    let admission = db.payload_admission_observation();
    let content = html! {
        h2 { "Payload retention" }
        p { "Storing account: " (session.id()) ". Startup mode: " (mode) "." }
        p { "Read-only diagnostics. Reload observes the last worker report; it does not run maintenance." }
        p { "Usage sampled starting at Unix seconds " (sampled_at.as_u64())
            "; accounting and guarded counters are separate observations, not one atomic snapshot." }
        @match usage {
            Ok(Some(usage)) => {
                p { "Logical retained: " (usage.logical_current_bytes)
                    " bytes; unique content store: " (usage.unique_stored_bytes) " bytes." }
            },
            Ok(None) => { p { "Usage unavailable: accounting is not ready." } },
            Err(_) => { p { "Usage unavailable: database accounting read failed." } },
        }
        p { "Logical reserved: " (admission.logical_reserved_bytes)
            " bytes; guarded buffer capacity: " (admission.buffer_bytes)
            " bytes; pending demand intent: " (admission.pending_demand_bytes)
            " bytes (including entries awaiting routine expiry). These are distinct, not whole-process RAM; "
            "Disabled/DryRun do not track aggregate acquisition buffers." }
        (render_forecast(report.as_ref()))
    };
    let navbar = state.render_settings_navbar(&session, "events").await?;
    let page = state
        .render_settings_page(&session, navbar, "Payload retention", content)
        .await?;
    Ok(recovery::sensitive_response(Maud(page)))
}

/// Present only complete projections; absent and incomplete reports are
/// explicit.
fn render_forecast(report: Option<&DryRunReport>) -> Markup {
    html! {
        p {
            "Physical database size and reclamation: unknown. Unique-store savings and GC backlog "
            "are not projected. Shared, Missing and protected references can pin values; "
            "logical victims do not promise disk space. Whole values and transactions are indivisible."
        }
        @if let Some(report) = report {
            p { "As of Unix seconds " (report.as_of.as_u64())
                "; may already be stale. Completeness: " (format!("{:?}", report.status)) "." }
            p { "Snapshot limits: " (report.limits.events) " headers, "
                (report.limits.authors) " authors, " (report.limits.logical_bytes)
                " logical bytes, " (report.limits.time.as_millis()) " ms (cooperative). Visited: "
                (report.visited) "." }
            p { "Forecast global high water: " (report.database_high_water)
                " logical bytes; triggered low water is 90%. No future demand, reservations, "
                "arrivals or existing Enforce hysteresis are simulated." }
            @if let Some(usage) = report.observed_usage {
                p { "Observed logical retained: " (usage.logical_current_bytes)
                    " bytes; unique content store: " (usage.unique_stored_bytes) " bytes." }
            } @else {
                p { "Observed logical retained and unique content-store usage unavailable: accounting not ready." }
            }
            p { "Guarded logical reserved (sampled before snapshot): "
                (report.observed_guarded_admission.logical_reserved_bytes)
                " bytes. DryRun admission is Disabled: zero guarded ownership does not mean zero buffers or RAM." }
            @if let Some(projection) = &report.projection {
                p { "Proposed logical removal: " (projection.logical_victim_bytes)
                    " bytes; remaining: " (projection.logical_remaining_bytes)
                    " bytes; protected: " (projection.protected_logical_bytes)
                    " bytes. Unmet author targets: " (projection.unmet_authors)
                    "; unmet global target: " (projection.unmet_global) "." }
                details {
                    summary { "Author forecast high waters" }
                    @for (author, bytes) in &projection.author_high_waters {
                        p { (author) ": " (bytes) " logical bytes" }
                    }
                }
                p { "Every proposed victim below is unprotected in this snapshot: nonlocal SocialPost, "
                    "known nonfuture origins, expired materialization grace, nonzero length. "
                    "Distance bonus is shown as its capped logarithmic age credit (seconds), not replication probability." }
                table {
                    thead { tr {
                        th { "Event / author" } th { "Reason" } th { "Logical bytes" }
                        th { "Effective age (seconds)" } th { "Distance age credit (seconds)" }
                    } }
                    tbody {
                        @for (detail, (_, reason)) in projection.victim_details.iter().zip(&projection.victims) {
                            tr {
                                td { (detail.event) br; (detail.author) }
                                td { (format!("{reason:?}")) }
                                td { (detail.bytes) }
                                td { (detail.age_seconds) }
                                td { (detail.distance_credit_ticks / (1i128 << 32)) }
                            }
                        }
                    }
                }
            } @else {
                p { "No projection. Bounds or unready accounting can keep this snapshot incomplete forever; "
                    "no prefix victims or accumulated savings are shown." }
            }
        } @else {
            p { "No DryRun report available. Disabled and Enforce do not produce forecasts; "
                "a DryRun worker may not yet have published one. This is not evidence of zero usage or successful maintenance." }
        }
    }
}
