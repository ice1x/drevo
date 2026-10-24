//! `GET`/`POST /config/text2cypher`: the runtime, Web-UI-settable text-to-Cypher
//! provider (upstream / API key / model) behind `drevo.cypher.fromText`. The
//! key is write-only: no response ever carries it. Gated on `http`.
#![cfg(feature = "http")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::text2cypher::Text2CypherConfigStore;
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

fn app_with(store: Arc<Text2CypherConfigStore>) -> axum::Router {
    let service = Arc::new(NativeService::in_memory());
    build_native_router(NativeApiState::new(service).with_text2cypher_config_store(store))
}

#[tokio::test]
async fn the_endpoint_is_unavailable_without_a_store() {
    let service = Arc::new(NativeService::in_memory());
    let app = build_native_router(NativeApiState::new(service));
    let (st, _, _) = send(&app, "GET", "/config/text2cypher", None).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn status_apply_and_a_blank_key_keeps_the_secret() {
    let app = app_with(Text2CypherConfigStore::in_memory(None));

    let (st, body, raw) = send(&app, "GET", "/config/text2cypher", None).await;
    assert_eq!(st, StatusCode::OK, "body: {raw}");
    assert_eq!(body["configured"], json!(false));
    assert_eq!(body["api_key_set"], json!(false));

    let (st, body, raw) = send(
        &app,
        "POST",
        "/config/text2cypher",
        Some(json!({
            "upstream": "https://api.openai.com/v1/chat/completions",
            "api_key": "sk-super-secret",
            "model": "gpt-4o-mini"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "body: {raw}");
    assert_eq!(body["configured"], json!(true));
    assert_eq!(body["api_key_set"], json!(true));
    assert_eq!(
        body["upstream"],
        json!("https://api.openai.com/v1/chat/completions")
    );
    assert_eq!(body["model"], json!("gpt-4o-mini"));
    assert!(!raw.contains("sk-super-secret"), "leaked the key: {raw}");

    let (st, body, raw) = send(&app, "GET", "/config/text2cypher", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["api_key_set"], json!(true));
    assert!(body.get("api_key").is_none());
    assert!(!raw.contains("sk-super-secret"), "leaked the key: {raw}");

    // A blank key keeps the stored secret; a blank model takes the default.
    let (st, body, _) = send(
        &app,
        "POST",
        "/config/text2cypher",
        Some(json!({ "upstream": "http://host.docker.internal:11434/v1/chat/completions" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        body["api_key_set"],
        json!(true),
        "blank key must keep the secret"
    );
    assert_eq!(body["model"], json!("gpt-4o-mini"));

    // A malformed upstream is a 400 and changes nothing.
    let (st, _, _) = send(
        &app,
        "POST",
        "/config/text2cypher",
        Some(json!({ "upstream": "ftp://nope", "api_key": "k" })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (_, body, _) = send(&app, "GET", "/config/text2cypher", None).await;
    assert_eq!(
        body["upstream"],
        json!("http://host.docker.internal:11434/v1/chat/completions")
    );
}

#[tokio::test]
async fn the_setting_is_persisted_sealed_and_wins_over_the_env_next_boot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("text2cypher_config.json");
    let store = Text2CypherConfigStore::load(path.clone(), None);
    let app = app_with(store);
    let (st, _, _) = send(
        &app,
        "POST",
        "/config/text2cypher",
        Some(json!({ "upstream": "https://llm.example/v1/chat/completions", "api_key": "sk-persist", "model": "m" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let on_disk = std::fs::read(&path).unwrap();
    assert!(
        String::from_utf8_lossy(&on_disk).contains("sk-persist"),
        "a plain store keeps the file readable"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // Next boot: the file wins over the env fallback.
    let env = drevo::text2cypher::Text2CypherConfig::from_env(|k| match k {
        "DREVO_TEXT2CYPHER_UPSTREAM" => Some("https://env.example/v1/chat/completions".to_string()),
        _ => None,
    })
    .unwrap();
    let reloaded = Text2CypherConfigStore::load(path, env);
    let snap = reloaded.snapshot().unwrap();
    assert_eq!(snap.upstream, "https://llm.example/v1/chat/completions");
    assert_eq!(snap.api_key.as_deref(), Some("sk-persist"));
    assert_eq!(snap.model, "m");
}

/// Right after a POST, `drevo.cypher.fromText` talks to the new upstream.
#[cfg(feature = "embeddings-proxy")]
#[tokio::test]
async fn a_saved_upstream_serves_from_text_at_once() {
    use axum::routing::post;
    use std::collections::HashMap;

    // A chat/completions upstream that always answers one query.
    let upstream = axum::Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            axum::Json(json!({
                "choices": [{"message": {"role": "assistant",
                    "content": "MATCH (b:Bug) RETURN b.title LIMIT 3"}}]
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

    let service = Arc::new(NativeService::in_memory());
    let app = build_native_router(
        NativeApiState::new(Arc::clone(&service))
            .with_text2cypher_config_store(Text2CypherConfigStore::in_memory(None)),
    );
    let (st, _, _) = send(
        &app,
        "POST",
        "/config/text2cypher",
        Some(json!({ "upstream": format!("http://{addr}/v1/chat/completions"), "model": "fake" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let result = tokio::task::spawn_blocking(move || {
        service.execute(
            &drevo::cypher::parser::parse(
                "CALL drevo.cypher.fromText('three bug titles') YIELD cypher RETURN cypher",
            )
            .unwrap(),
            HashMap::new(),
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        result.rows[0][0],
        drevo::cypher::executor::Value::String("MATCH (b:Bug) RETURN b.title LIMIT 3".into())
    );
}
