//! Round-trip tests for the `embeddings-proxy` backend (Phase 19, #217 +
//! passthrough).
//!
//! Gated on the `embeddings-proxy` feature: they compile to an empty binary on
//! the default feature set (so the standard CI test run is unaffected and
//! carries no network client), and exercise the real [`ProxyBackend`] against a
//! **local** in-process axum stub — no external network, deterministic, and
//! safe to run anywhere with:
//!
//! ```text
//! cargo test --features embeddings-proxy --test embeddings_proxy_tests
//! ```
//!
//! What they lock:
//! - a request is forwarded to the configured upstream with the OpenAI body
//!   shape (`model` + normalised `input`) and the configured bearer token;
//! - the upstream's JSON response is returned **verbatim** (passthrough) — the
//!   proxy re-types nothing, so base64 embeddings and extra fields survive;
//! - provider-specific request fields (`dimensions`, `input_type`, …) pass
//!   through to the upstream;
//! - the outbound destination is the configured upstream even when the request
//!   body carries a `url` (SSRF boundary);
//! - a non-2xx upstream becomes an [`EmbeddingsError::Upstream`] (→ 502).

#![cfg(feature = "embeddings-proxy")]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use drevo::embeddings::{
    EmbeddingInput, EmbeddingsConfig, EmbeddingsError, EmbeddingsRequest, ProxyBackend,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

/// What the stub upstream captured from the last request, for assertions.
#[derive(Clone, Default)]
struct Captured {
    body: Arc<Mutex<Option<Value>>>,
    auth: Arc<Mutex<Option<String>>>,
}

async fn stub_ok(
    State(cap): State<Captured>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    *cap.body.lock().unwrap() = Some(body);
    *cap.auth.lock().unwrap() = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Json(json!({
        "object": "list",
        "data": [
            {"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]},
            {"object": "embedding", "index": 1, "embedding": [0.4, 0.5, 0.6]}
        ],
        "model": "text-embedding-3-small",
        "usage": {"prompt_tokens": 4, "total_tokens": 4}
    }))
}

/// A stub that answers with a base64-encoded embedding + a provider-specific
/// extra field — neither of which the proxy could survive if it re-typed the
/// body into a float-vector struct.
async fn stub_base64() -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": "gAAAAA=="}],
        "model": "voyage-3",
        "usage": {"total_tokens": 2},
        "provider_extra": {"note": "kept verbatim"}
    }))
}

async fn stub_500() -> (axum::http::StatusCode, String) {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "upstream boom".to_string(),
    )
}

/// Bind a local stub server, returning its address.
async fn spawn(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    addr
}

fn proxy_at(addr: SocketAddr, api_key: Option<&str>, model: Option<&str>) -> ProxyBackend {
    ProxyBackend::new(EmbeddingsConfig {
        upstream: format!("http://{addr}/v1/embeddings"),
        api_key: api_key.map(str::to_string),
        model: model.map(str::to_string),
    })
    .expect("build proxy")
}

fn req(model: &str, input: EmbeddingInput) -> EmbeddingsRequest {
    EmbeddingsRequest {
        model: model.to_string(),
        input,
        extra: serde_json::Map::new(),
    }
}

#[tokio::test]
async fn proxy_forwards_and_returns_upstream_verbatim() {
    let cap = Captured::default();
    let addr = spawn(
        Router::new()
            .route("/v1/embeddings", post(stub_ok))
            .with_state(cap.clone()),
    )
    .await;
    let backend = proxy_at(addr, Some("sk-test-123"), None);

    let resp = backend
        .embed(&req(
            "text-embedding-3-small",
            EmbeddingInput::Batch(vec!["alpha".into(), "beta".into()]),
        ))
        .await
        .expect("embed");

    // Response is the upstream body, verbatim (untyped Value passthrough).
    assert_eq!(resp["object"], "list");
    assert_eq!(resp["data"].as_array().unwrap().len(), 2);
    assert_eq!(resp["data"][1]["index"], 1);
    assert_eq!(resp["data"][1]["embedding"], json!([0.4, 0.5, 0.6]));
    assert_eq!(resp["model"], "text-embedding-3-small");
    assert_eq!(resp["usage"]["total_tokens"], 4);

    // The upstream saw the OpenAI body shape and the configured bearer token.
    let body = cap.body.lock().unwrap().clone().expect("captured body");
    assert_eq!(body["model"], "text-embedding-3-small");
    assert_eq!(body["input"], json!(["alpha", "beta"]));
    let auth = cap.auth.lock().unwrap().clone().expect("captured auth");
    assert_eq!(auth, "Bearer sk-test-123");
}

#[tokio::test]
async fn proxy_passes_base64_and_extra_response_fields_through() {
    let addr = spawn(Router::new().route("/v1/embeddings", post(stub_base64))).await;
    let backend = proxy_at(addr, None, None);

    let resp = backend
        .embed(&req("voyage-3", EmbeddingInput::Single("hi".into())))
        .await
        .expect("embed");

    // A base64 string embedding (not a float array) survives — proof the proxy
    // does not re-type the response.
    assert_eq!(resp["data"][0]["embedding"], json!("gAAAAA=="));
    // A provider-specific field the OpenAI schema never mentions survives too.
    assert_eq!(resp["provider_extra"]["note"], "kept verbatim");
}

#[tokio::test]
async fn proxy_forwards_provider_specific_request_fields() {
    let cap = Captured::default();
    let addr = spawn(
        Router::new()
            .route("/v1/embeddings", post(stub_ok))
            .with_state(cap.clone()),
    )
    .await;
    let backend = proxy_at(addr, None, None);

    // Voyage/OpenAI extras arrive via serde flatten and must reach the upstream.
    let request: EmbeddingsRequest = serde_json::from_str(
        r#"{"model":"voyage-3","input":"hi","input_type":"document","dimensions":512}"#,
    )
    .unwrap();
    backend.embed(&request).await.expect("embed");

    let body = cap.body.lock().unwrap().clone().expect("captured body");
    assert_eq!(body["model"], "voyage-3");
    assert_eq!(body["input"], json!(["hi"]));
    assert_eq!(body["input_type"], "document");
    assert_eq!(body["dimensions"], 512);
}

#[tokio::test]
async fn proxy_uses_config_model_when_request_omits_it() {
    let cap = Captured::default();
    let addr = spawn(
        Router::new()
            .route("/v1/embeddings", post(stub_ok))
            .with_state(cap.clone()),
    )
    .await;
    let backend = proxy_at(addr, None, Some("default-model"));

    backend
        .embed(&req("", EmbeddingInput::Single("hi".into())))
        .await
        .expect("embed");

    let body = cap.body.lock().unwrap().clone().expect("captured body");
    assert_eq!(body["model"], "default-model");
    // No api key configured → no Authorization header forwarded.
    assert!(cap.auth.lock().unwrap().is_none());
}

#[tokio::test]
async fn proxy_targets_configured_upstream_even_with_url_in_body() {
    // SSRF boundary: a `url` in the request body cannot change the destination.
    // The request still reaches THIS configured stub; the stray field merely
    // rides along in the forwarded body (the stub, like a real upstream,
    // ignores it).
    let cap = Captured::default();
    let addr = spawn(
        Router::new()
            .route("/v1/embeddings", post(stub_ok))
            .with_state(cap.clone()),
    )
    .await;
    let backend = proxy_at(addr, None, None);

    let request: EmbeddingsRequest = serde_json::from_str(
        r#"{"model":"m","input":"hi","url":"http://169.254.169.254/latest/meta-data/"}"#,
    )
    .unwrap();
    let resp = backend.embed(&request).await.expect("embed");

    // It reached the configured stub (got the stub's canned answer)…
    assert_eq!(resp["model"], "text-embedding-3-small");
    // …and the stray `url` was forwarded to that fixed upstream, not acted on.
    let body = cap.body.lock().unwrap().clone().expect("captured body");
    assert_eq!(body["url"], "http://169.254.169.254/latest/meta-data/");
}

#[tokio::test]
async fn proxy_maps_upstream_5xx_to_upstream_error() {
    let addr = spawn(Router::new().route("/v1/embeddings", post(stub_500))).await;
    let backend = proxy_at(addr, None, None);
    let err = backend
        .embed(&req("m", EmbeddingInput::Single("hi".into())))
        .await
        .expect_err("should fail");
    assert!(matches!(err, EmbeddingsError::Upstream(_)), "got {err:?}");
}
