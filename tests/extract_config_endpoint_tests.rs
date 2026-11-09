//! `GET`/`POST /config/extract`: the runtime-settable chat endpoint behind
//! `drevo.extract.*` (upstream / API key / model / timeout). The key is
//! write-only: no response ever carries it. Gated on `http`.
#![cfg(feature = "http")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::extract::settings::ExtractConfigStore;
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
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
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value, raw)
}

#[tokio::test]
async fn the_endpoint_is_unavailable_without_a_store() {
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));
    let (st, _, raw) = send(&app, "GET", "/config/extract", None).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        raw.contains("no chat upstream is configured for extraction"),
        "{raw}"
    );
}

#[tokio::test]
async fn status_apply_and_a_blank_key_keeps_the_secret() {
    let app = build_native_router(
        NativeApiState::new(Arc::new(NativeService::in_memory()))
            .with_extract_config_store(ExtractConfigStore::in_memory(None)),
    );
    let (st, body, _) = send(&app, "GET", "/config/extract", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["configured"], json!(false));

    let (st, body, raw) = send(
        &app,
        "POST",
        "/config/extract",
        Some(json!({
            "upstream": "https://llm.example/v1/chat/completions",
            "api_key": "sk-extract-secret",
            "model": "gpt-4.1-mini",
            "timeout_ms": 30000
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{raw}");
    assert_eq!(
        body,
        json!({
            "configured": true,
            "upstream": "https://llm.example/v1/chat/completions",
            "model": "gpt-4.1-mini",
            "api_key_set": true,
            "timeout_ms": 30000
        })
    );
    assert!(!raw.contains("sk-extract-secret"), "leaked the key: {raw}");

    let (st, body, _) = send(
        &app,
        "POST",
        "/config/extract",
        Some(json!({ "upstream": "http://host.docker.internal:11434/v1/chat/completions" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["api_key_set"], json!(true));
    assert_eq!(body["model"], json!("gpt-4o-mini"));
    assert_eq!(body["timeout_ms"], json!(60000));

    let (st, _, raw) = send(
        &app,
        "POST",
        "/config/extract",
        Some(json!({ "upstream": "ftp://nope" })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(raw.contains("unsupported scheme in `ftp://nope`"), "{raw}");
    let (_, body, _) = send(&app, "GET", "/config/extract", None).await;
    assert_eq!(
        body["upstream"],
        json!("http://host.docker.internal:11434/v1/chat/completions")
    );
}
