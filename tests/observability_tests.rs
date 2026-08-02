//! Phase 15 task `00130` — integration tests for the observability surface.
//!
//! Drives the real `axum::Router` returned by [`drevo::api::build_router`] via
//! `tower::ServiceExt::oneshot` — the same in-process pattern as
//! `tests/web_ui_tests.rs` and `tests/http_api_tests.rs` (no TCP listener, so
//! the suite runs on any CI cell that compiles the `http` feature).
//!
//! Scope:
//!   * `GET /metrics` returns 200 with the Prometheus `text/plain;
//!     version=0.0.4` content type and the standard drevo metric families;
//!   * the per-request instrumentation middleware counts requests by status
//!     class and records latency, so traffic shows up in a subsequent scrape;
//!   * unknown methods on `/metrics` fall through to the 405 handler.

#![cfg(feature = "http")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::api::{build_router, ApiState};
use drevo::db::Drevo;
use http_body_util::BodyExt;
use tower::ServiceExt;

fn make_app() -> axum::Router {
    let db = Arc::new(Drevo::open_in_memory().expect("open in-memory db"));
    let state = ApiState::new(db);
    build_router(state)
}

/// Send a request with the given method/uri and return (status, content-type,
/// body text).
async fn send(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, String, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("build request");
    let response = app.clone().oneshot(req).await.expect("router response");
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes()
        .to_vec();
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

#[tokio::test]
async fn metrics_endpoint_serves_prometheus_text() {
    let app = make_app();
    let (status, content_type, body) = send(&app, "GET", "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "text/plain; version=0.0.4; charset=utf-8");
    // Standard metric families are always present (registered at startup).
    assert!(
        body.contains("# TYPE drevo_http_requests_total counter"),
        "missing http requests counter:\n{body}"
    );
    assert!(
        body.contains("# TYPE drevo_http_request_duration_seconds histogram"),
        "missing http latency histogram:\n{body}"
    );
    assert!(
        body.contains("drevo_process_uptime_seconds"),
        "missing uptime gauge:\n{body}"
    );
    assert!(
        body.contains(&format!(
            "drevo_build_info{{version=\"{}\"}} 1",
            drevo::VERSION
        )),
        "missing build_info:\n{body}"
    );
}

#[tokio::test]
async fn requests_are_counted_by_status_class() {
    let app = make_app();
    // A 200 (root) and a 404 (unknown path) flow through the middleware.
    let (ok, _, _) = send(&app, "GET", "/").await;
    assert_eq!(ok, StatusCode::OK);
    let (missing, _, _) = send(&app, "GET", "/no-such-path").await;
    assert_eq!(missing, StatusCode::NOT_FOUND);

    let (_, _, body) = send(&app, "GET", "/metrics").await;
    // At least one 2xx (the `/` request) and one 4xx (the 404) were recorded.
    // The `/metrics` scrapes themselves are also 2xx, so assert ">= 1".
    let twoxx = extract_counter(&body, "drevo_http_requests_total{status=\"2xx\"}");
    let fourxx = extract_counter(&body, "drevo_http_requests_total{status=\"4xx\"}");
    assert!(twoxx >= 1, "expected >=1 2xx, got {twoxx}:\n{body}");
    assert!(fourxx >= 1, "expected >=1 4xx, got {fourxx}:\n{body}");
    // The latency histogram counted every completed request too.
    let hist_count = extract_counter(&body, "drevo_http_request_duration_seconds_count");
    assert!(
        hist_count >= 2,
        "expected >=2 observations, got {hist_count}"
    );
}

#[tokio::test]
async fn in_flight_counts_only_the_active_scrape() {
    let app = make_app();
    // Several requests that each fully complete (the middleware decrements on
    // the way out). They must NOT accumulate in the in-flight gauge.
    for _ in 0..3 {
        let _ = send(&app, "GET", "/").await;
    }
    let (_, _, body) = send(&app, "GET", "/metrics").await;
    // The scrape itself is in flight while it renders, so the gauge reads
    // exactly 1 — not 4. A value > 1 would mean `request_finished` never ran.
    assert!(
        body.contains("drevo_http_requests_in_flight 1"),
        "in-flight must show only the active scrape (1), proving prior \
         requests decremented:\n{body}"
    );
}

#[tokio::test]
async fn metrics_rejects_post_with_405() {
    let app = make_app();
    let (status, _, _) = send(&app, "POST", "/metrics").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

/// Pull the integer value off a `name 123` exposition line (the prefix is the
/// full `name{labels}` up to the value). Returns 0 if the line is absent.
fn extract_counter(body: &str, prefix: &str) -> u64 {
    body.lines()
        .find_map(|line| {
            let rest = line.strip_prefix(prefix)?;
            rest.trim().parse::<u64>().ok()
        })
        .unwrap_or(0)
}
