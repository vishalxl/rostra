use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Router, middleware};
use tower::ServiceExt as _;

use super::cache_control;

async fn avatar_response(request: Request<Body>) -> Response {
    match request.uri().path() {
        "/profile/success/avatar" => (
            [
                (header::ETAG, "\"avatar\""),
                (header::CONTENT_SECURITY_POLICY, "default-src 'none'"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            "avatar",
        )
            .into_response(),
        "/profile/not-modified/avatar" => StatusCode::NOT_MODIFIED.into_response(),
        "/profile/canonical/avatar" => (
            StatusCode::PERMANENT_REDIRECT,
            [(header::LOCATION, "/profile/short/avatar?source=legacy")],
        )
            .into_response(),
        "/profile/restrictive/avatar" => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "private, no-store, max-age=0")],
        )
            .into_response(),
        "/profile/error-restrictive/avatar" => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CACHE_CONTROL, "no-store, private, max-age=0")],
        )
            .into_response(),
        "/profile/login/avatar" => (
            StatusCode::SEE_OTHER,
            [(
                header::LOCATION,
                "/unlock?redirect=%2Fprofile%2Flogin%2Favatar",
            )],
        )
            .into_response(),
        "/profile/missing/avatar" => StatusCode::NOT_FOUND.into_response(),
        "/profile/error/avatar" => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        _ => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "public, max-age=60")],
        )
            .into_response(),
    }
}

async fn run_response(path: &str) -> Response {
    Router::new()
        .fallback(any(avatar_response))
        .layer(middleware::from_fn(cache_control))
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn avatar_representations_and_canonical_redirect_keep_long_caching() {
    for path in [
        "/profile/success/avatar",
        "/profile/not-modified/avatar",
        "/profile/canonical/avatar?source=legacy",
    ] {
        let response = run_response(path).await;
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=86400",
            "{path}"
        );
    }

    let response = run_response("/profile/success/avatar").await;
    assert_eq!(response.headers()[header::ETAG], "\"avatar\"");
    assert_eq!(
        response.headers()[header::CONTENT_SECURITY_POLICY],
        "default-src 'none'"
    );
    assert_eq!(
        response.headers()[header::X_CONTENT_TYPE_OPTIONS],
        "nosniff"
    );

    let response = run_response("/profile/canonical/avatar?source=legacy").await;
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        response.headers()[header::LOCATION],
        "/profile/short/avatar?source=legacy"
    );
}

#[tokio::test]
async fn avatar_failures_are_private_and_no_store() {
    for path in [
        "/profile/login/avatar",
        "/profile/missing/avatar",
        "/profile/error/avatar",
    ] {
        let response = run_response(path).await;
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store",
            "{path}"
        );
    }

    let response = run_response("/profile/login/avatar").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[header::LOCATION],
        "/unlock?redirect=%2Fprofile%2Flogin%2Favatar"
    );
}

#[tokio::test]
async fn avatar_cache_policy_does_not_weaken_downstream_restrictions() {
    let response = run_response("/profile/restrictive/avatar").await;
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store, max-age=0"
    );

    let response = run_response("/profile/error-restrictive/avatar").await;
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "no-store, private, max-age=0"
    );
}

#[tokio::test]
async fn non_avatar_cache_policy_is_unchanged() {
    let response = run_response("/profile/success").await;
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=60"
    );
}
