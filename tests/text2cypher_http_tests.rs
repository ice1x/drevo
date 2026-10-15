//! `GET /` reports whether text-to-Cypher (`drevo.cypher.fromText`) is
//! configured, so the web UI can show its **Ask** button as available or
//! explain how to set it up (issue #616).
//!
//! The generator is a process global, so this binary holds a single test that
//! checks the unconfigured state first and then installs a fake generator.
#![cfg(feature = "http")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::text2cypher::{install, CypherGenerator, Text2CypherError};
use tower::ServiceExt;

struct FakeGenerator;
impl CypherGenerator for FakeGenerator {
    fn generate(&self, _system: &str, question: &str) -> Result<String, Text2CypherError> {
        Ok(format!("// {question}"))
    }
    fn model(&self) -> Option<String> {
        Some("qwen2.5-coder:14b".to_string())
    }
    fn upstream(&self) -> Option<String> {
        Some("http://host.docker.internal:11434/v1/chat/completions".to_string())
    }
}

async fn status_json(app: &axum::Router) -> serde_json::Value {
    let req = Request::builder()
        .uri("/")
        .body(Body::empty())
        .expect("build request");
    let response = app.clone().oneshot(req).await.expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("status is JSON")
}

#[tokio::test]
async fn status_reports_text_to_cypher_model_once_configured() {
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));

    let before = status_json(&app).await;
    assert!(
        before["text2cypher"].is_null(),
        "unconfigured text-to-Cypher reports null, got {before}"
    );

    install(Arc::new(FakeGenerator));
    let after = status_json(&app).await;
    assert_eq!(after["text2cypher"]["model"], "qwen2.5-coder:14b");
    assert!(
        after["text2cypher"].get("upstream").is_none(),
        "the upstream URL stays on the server: {after}"
    );
}
