//! `GET`/`POST /config/embeddings` on both the KV and the durable-native
//! routers — the runtime, Web-UI-settable embeddings config (API key / upstream
//! / model). The key is write-only: the status view and every response must
//! never echo it back. Gated on `http`; the config endpoints themselves do not
//! need the `embeddings-proxy` feature (they only read/write the shared store).

#![cfg(feature = "http")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::embeddings::EmbeddingsConfigStore;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value, String) {
    let req = Request::builder().method(method).uri(uri);
    let req = if let Some(ref value) = body {
        req.header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(value).expect("serialize")))
    } else {
        req.body(Body::empty())
    }
    .expect("build request");
    let response = app.clone().oneshot(req).await.expect("router response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let raw = String::from_utf8_lossy(&bytes).to_string();
    let value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value, raw)
}

/// The end-to-end contract, run against whichever router (KV or native) is
/// passed in — both wire the same shared handlers.
async fn exercise(app: axum::Router) {
    // Unconfigured to start.
    let (st, body, _) = send(&app, "GET", "/config/embeddings", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["configured"], json!(false));
    assert_eq!(body["api_key_set"], json!(false));

    // Configure it. The response reports status, never the secret.
    let (st, body, raw) = send(
        &app,
        "POST",
        "/config/embeddings",
        Some(json!({
            "upstream": "https://api.openai.com/v1/embeddings",
            "api_key": "sk-super-secret",
            "model": "text-embedding-3-small"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "body: {raw}");
    assert_eq!(body["configured"], json!(true));
    assert_eq!(body["api_key_set"], json!(true));
    assert_eq!(
        body["upstream"],
        json!("https://api.openai.com/v1/embeddings")
    );
    assert_eq!(body["model"], json!("text-embedding-3-small"));
    assert!(
        !raw.contains("sk-super-secret"),
        "POST response leaked the api key: {raw}"
    );

    // A GET reflects it — and still never exposes the key.
    let (st, body, raw) = send(&app, "GET", "/config/embeddings", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["configured"], json!(true));
    assert_eq!(body["api_key_set"], json!(true));
    assert!(
        !raw.contains("sk-super-secret"),
        "GET status leaked the api key: {raw}"
    );
    // The response has no field that could carry the key at all.
    assert!(body.get("api_key").is_none());

    // A blank key keeps the stored secret (only the model changes).
    let (st, body, _) = send(
        &app,
        "POST",
        "/config/embeddings",
        Some(json!({ "upstream": "https://api.openai.com/v1/embeddings", "model": "m2" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        body["api_key_set"],
        json!(true),
        "blank key must keep the secret"
    );
    assert_eq!(body["model"], json!("m2"));

    // A malformed upstream is a 400, not a 503.
    let (st, _, _) = send(
        &app,
        "POST",
        "/config/embeddings",
        Some(json!({ "upstream": "file:///etc/passwd", "api_key": "k" })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[cfg(feature = "redb-backend")]
#[tokio::test]
async fn config_embeddings_endpoint_on_kv_router() {
    use drevo::api::{build_router, ApiState};
    use drevo::db::Drevo;
    let db = Arc::new(Drevo::open_in_memory().expect("db"));
    let store = EmbeddingsConfigStore::in_memory(None);
    let app = build_router(ApiState::new(db).with_embeddings_config_store(store));
    exercise(app).await;
}

#[tokio::test]
async fn config_embeddings_endpoint_on_native_router() {
    use drevo::native_api::{build_native_router, NativeApiState};
    use drevo::native_service::NativeService;
    let service = Arc::new(NativeService::in_memory());
    let store = EmbeddingsConfigStore::in_memory(None);
    let app = build_native_router(NativeApiState::new(service).with_embeddings_config_store(store));
    exercise(app).await;
}

/// Without a store wired in, the endpoint reports unavailable rather than
/// panicking (a state some tests build).
#[cfg(feature = "redb-backend")]
#[tokio::test]
async fn config_embeddings_without_store_is_unavailable() {
    use drevo::api::{build_router, ApiState};
    use drevo::db::Drevo;
    let db = Arc::new(Drevo::open_in_memory().expect("db"));
    let app = build_router(ApiState::new(db));
    let (st, _, _) = send(&app, "GET", "/config/embeddings", None).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
}
