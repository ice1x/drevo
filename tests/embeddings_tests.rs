//! HTTP tests for the OpenAI-compatible embeddings endpoint (Phase 19,
//! issue #217): `POST /v1/embeddings`.
//!
//! These run on the **default** feature set (no `embeddings-proxy`, so no
//! network client is compiled in). They therefore exercise everything that
//! does not require an upstream: request parsing/validation, the
//! not-configured `503` path, method rejection, and the OpenAI response
//! shape. The actual proxy round-trip is covered by unit tests over the pure
//! request/response helpers in `src/embeddings.rs` and by an opt-in live test
//! behind the `embeddings-proxy` feature.
//!
//! The security-relevant invariant asserted here (OWASP A10 / SSRF): the
//! outbound destination is never taken from the request — a body carrying a
//! `url`/`base_url`/`endpoint` field cannot select an upstream (with no backend
//! configured the request is refused with `503` before any outbound call). The
//! passthrough of such a field to a *configured* upstream is covered in
//! `embeddings_proxy_tests.rs`.

#![cfg(feature = "http")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::api::{build_router, ApiState};
use drevo::db::Drevo;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn make_app() -> axum::Router {
    let db = Arc::new(Drevo::open_in_memory().expect("open in-memory db"));
    let state = ApiState::new(db);
    build_router(state)
}

async fn post_embeddings(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).expect("serialize")))
        .expect("build request");
    let response = app.clone().oneshot(req).await.expect("router response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, value)
}

#[tokio::test]
async fn embeddings_without_backend_returns_503() {
    // No backend configured on the default state → the endpoint exists but
    // reports "not configured" with a 503 (mirrors the semantic-facet 400).
    let app = make_app();
    let (status, body) = post_embeddings(
        &app,
        json!({ "model": "text-embedding-3-small", "input": "hello" }),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let msg = body["error"].as_str().unwrap_or_default().to_lowercase();
    assert!(msg.contains("not configured"), "unexpected body: {body}");
    assert_eq!(body["status"], 503);
}

#[tokio::test]
async fn embeddings_accepts_string_and_array_input() {
    // Both the single-string and array forms of `input` must deserialize
    // (OpenAI accepts either). We can't produce vectors without a backend, so
    // a successful parse surfaces as the 503 (not a 400 parse error).
    let app = make_app();
    for input in [json!("just a string"), json!(["a", "b", "c"])] {
        let (status, _) = post_embeddings(&app, json!({ "model": "m", "input": input })).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "input {input:?} should parse then hit the not-configured path"
        );
    }
}

#[tokio::test]
async fn embeddings_empty_input_is_rejected_400() {
    // An empty batch has nothing to embed — a client error, checked before the
    // backend so it is deterministic even with no backend configured.
    let app = make_app();
    let (status, body) = post_embeddings(&app, json!({ "model": "m", "input": [] })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["status"], 400);
}

#[tokio::test]
async fn embeddings_ignores_request_supplied_url_ssrf_guard() {
    // OWASP A10 / SSRF: the outbound upstream is configured server-side only.
    // A body trying to smuggle a destination cannot select one — reaching the
    // 503 not-configured path proves no request field turned into an outbound
    // call. (Forwarding such a field to a *configured* upstream, harmlessly, is
    // covered by embeddings_proxy_tests.rs.)
    let app = make_app();
    let (status, body) = post_embeddings(
        &app,
        json!({
            "model": "m",
            "input": "hi",
            "url": "http://169.254.169.254/latest/meta-data/",
            "base_url": "http://localhost:9999",
            "endpoint": "file:///etc/passwd"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    let msg = body["error"].as_str().unwrap_or_default().to_lowercase();
    assert!(msg.contains("not configured"), "unexpected body: {body}");
}

#[tokio::test]
async fn embeddings_malformed_json_is_400() {
    let app = make_app();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .expect("build request");
    let response = app.oneshot(req).await.expect("router response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn embeddings_get_is_405() {
    let app = make_app();
    let req = Request::builder()
        .method("GET")
        .uri("/v1/embeddings")
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(req).await.expect("router response");
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}
