//! Round-trip tests for the `embeddings-proxy` backend (Phase 19, issue #217).
//!
//! These are gated on the `embeddings-proxy` feature: they compile to an empty
//! binary on the default feature set (so the standard CI test run is
//! unaffected and carries no network client), and exercise the real
//! [`ProxyBackend`] against a **local** in-process axum stub — no external
//! network, deterministic, and safe to run anywhere with:
//!
//! ```text
//! cargo test --features embeddings-proxy --test embeddings_proxy_tests
//! ```
//!
//! What they lock:
//! - a request is forwarded to the configured upstream with the OpenAI body
//!   shape (`model` + normalised `input`) and the configured bearer token;
//! - the upstream's OpenAI-shaped response is parsed and returned verbatim;
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

async fn stub_500() -> (axum::http::StatusCode, String) {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "upstream boom".to_string(),
    )
}

/// Bind a local stub server, returning its address and the capture handle.
async fn spawn(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    addr
}

#[tokio::test]
async fn proxy_forwards_and_parses_round_trip() {
    let cap = Captured::default();
    let addr = spawn(
        Router::new()
            .route("/v1/embeddings", post(stub_ok))
            .with_state(cap.clone()),
    )
    .await;

    let config = EmbeddingsConfig {
        upstream: format!("http://{addr}/v1/embeddings"),
        api_key: Some("sk-test-123".to_string()),
        model: None,
    };
    let backend = ProxyBackend::new(config).expect("build proxy");

    let req = EmbeddingsRequest {
        model: "text-embedding-3-small".to_string(),
        input: EmbeddingInput::Batch(vec!["alpha".into(), "beta".into()]),
    };
    let resp = backend.embed(&req).await.expect("embed");

    // Response parsed and passed through.
    assert_eq!(resp.object, "list");
    assert_eq!(resp.data.len(), 2);
    assert_eq!(resp.data[1].index, 1);
    assert_eq!(resp.data[1].embedding, vec![0.4, 0.5, 0.6]);
    assert_eq!(resp.model, "text-embedding-3-small");
    assert_eq!(resp.usage.total_tokens, 4);

    // The upstream saw the OpenAI body shape and the configured bearer token.
    let body = cap.body.lock().unwrap().clone().expect("captured body");
    assert_eq!(body["model"], "text-embedding-3-small");
    assert_eq!(body["input"], json!(["alpha", "beta"]));
    let auth = cap.auth.lock().unwrap().clone().expect("captured auth");
    assert_eq!(auth, "Bearer sk-test-123");
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

    let config = EmbeddingsConfig {
        upstream: format!("http://{addr}/v1/embeddings"),
        api_key: None,
        model: Some("default-model".to_string()),
    };
    let backend = ProxyBackend::new(config).expect("build proxy");
    let req = EmbeddingsRequest {
        model: String::new(),
        input: EmbeddingInput::Single("hi".into()),
    };
    backend.embed(&req).await.expect("embed");

    let body = cap.body.lock().unwrap().clone().expect("captured body");
    assert_eq!(body["model"], "default-model");
    // No api key configured → no Authorization header forwarded.
    assert!(cap.auth.lock().unwrap().is_none());
}

#[tokio::test]
async fn proxy_maps_upstream_5xx_to_upstream_error() {
    let addr = spawn(Router::new().route("/v1/embeddings", post(stub_500))).await;
    let config = EmbeddingsConfig {
        upstream: format!("http://{addr}/v1/embeddings"),
        api_key: None,
        model: None,
    };
    let backend = ProxyBackend::new(config).expect("build proxy");
    let req = EmbeddingsRequest {
        model: "m".to_string(),
        input: EmbeddingInput::Single("hi".into()),
    };
    let err = backend.embed(&req).await.expect_err("should fail");
    assert!(matches!(err, EmbeddingsError::Upstream(_)), "got {err:?}");
}
