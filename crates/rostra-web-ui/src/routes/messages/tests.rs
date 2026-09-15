use std::sync::{Arc, Mutex};

use axum::http::{StatusCode, header};
use rostra_client::error::PostError;
use rostra_client_db::DbError;
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

use super::{error_page, publication_error, sensitive_response, storage_error, take_page};

#[test]
fn default_identity_names_are_not_shown_as_message_display_names() {
    use rostra_core::id::ToShort as _;

    let id = rostra_core::id::RostraIdSecretKey::generate().id();
    for name in [
        None,
        Some(""),
        Some("  "),
        Some(id.to_short().to_string().as_str()),
    ] {
        assert_eq!(
            super::message_display_name(id, name),
            super::UNNAMED_PROFILE
        );
    }
    assert_eq!(super::message_display_name(id, Some(" Alice ")), "Alice");
    let other = rostra_core::id::RostraIdSecretKey::generate().id();
    for name in [
        id.to_string(),
        other.to_string(),
        other.to_short().to_string(),
    ] {
        assert_eq!(super::message_display_name(id, Some(&name)), name);
    }
}

#[test]
fn bounded_lookahead_only_offers_pages_with_another_row() {
    for count in [0, 1, 31, 32, 33, 64] {
        let (page, more) = take_page((0..count).collect::<Vec<_>>());
        assert_eq!(page.len(), count.min(32));
        assert_eq!(more, count > 32);
    }
}

/// Capture only structured test diagnostics, without a global subscriber.
struct Capture(Arc<Mutex<Vec<String>>>);

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields(Vec::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(format!(
            "{} {}",
            event.metadata().target(),
            fields.0.join(" ")
        ));
    }
}

/// Record field names and values for status/logging assertions.
struct Fields(Vec<String>);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push(format!("{}={value:?}", field.name()));
    }
}

#[tokio::test]
async fn injected_storage_failures_keep_private_html_and_structured_diagnostics() {
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    for operation in ["send", "retire", "reenroll"] {
        let (status, message) =
            tracing::subscriber::with_default(Capture(diagnostics.clone()), || {
                publication_error(
                    PostError::Storage {
                        source: DbError::Overflow,
                    },
                    operation,
                )
            });
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let response = sensitive_response(error_page(status, message));
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "identity");
        assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("<html"));
        assert!(!body.contains("Integer overflow"));
    }
    let response = tracing::subscriber::with_default(Capture(diagnostics.clone()), || {
        storage_error(DbError::Overflow)
    });
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    {
        let diagnostics = diagnostics.lock().unwrap();
        assert_eq!(diagnostics.len(), 4);
        for (entry, operation) in diagnostics
            .iter()
            .zip(["send", "retire", "reenroll", "read"])
        {
            assert!(entry.contains("rostra::direct_messages::http"));
            assert!(entry.contains(&format!("operation=\"{operation}\"")));
            assert!(entry.contains("error="));
            assert!(entry.contains("Integer overflow"));
            assert!(!entry.contains("text="));
            assert!(!entry.contains("csrf="));
        }
    }
    for error in [
        PostError::DirectMessageUnavailable,
        PostError::Storage {
            source: DbError::DmRecipientUnavailable,
        },
    ] {
        assert_eq!(publication_error(error, "send").0, StatusCode::BAD_REQUEST);
    }
    let (status, message) = publication_error(
        PostError::Storage {
            source: DbError::PayloadAdmissionPaused {
                reason: rostra_client_db::PayloadAdmissionPause::DatabaseCapacity,
            },
        },
        "send",
    );
    let response = sensitive_response(error_page(status, message));
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "no-store, private"
    );
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(std::str::from_utf8(&body).unwrap().contains("<html"));
    let internal = Arc::new(Mutex::new(Vec::new()));
    let (status, message) = tracing::subscriber::with_default(Capture(internal.clone()), || {
        publication_error(
            PostError::Encode {
                source: Box::new(std::io::Error::other("injected internal failure")),
            },
            "send",
        )
    });
    let response = sensitive_response(error_page(status, message));
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
    assert!(internal.lock().unwrap()[0].contains("injected internal failure"));
}
