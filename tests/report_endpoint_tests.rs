//! `GET /report` — the server half of the Web UI's "Report a problem" (#552).
//!
//! The UI combines this with what only the browser knows (the failing query,
//! UI errors, a screenshot) into a prefilled GitHub issue. The endpoint must
//! describe the server well enough to triage a bug — build, engine, limits,
//! graph size, recent problems — and must never leak a secret: the issue is
//! public.

#![cfg(feature = "http")]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::problems::ProblemLog;
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    let response = app.clone().oneshot(req).await.expect("response");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

fn seeded() -> Arc<NativeService> {
    let service = Arc::new(NativeService::in_memory());
    let q = drevo::cypher::parser::parse(
        "CREATE (:Person {name: 'a'})-[:KNOWS]->(:Person {name: 'b'})-[:LIKES]->(:Topic {name: 'c'})",
    )
    .expect("parse");
    service
        .execute(&q, std::collections::HashMap::new())
        .expect("seed");
    service
}

#[tokio::test]
async fn report_describes_build_engine_limits_graph_and_problems() {
    let service = seeded();
    service.set_statement_timeout(Some(Duration::from_millis(30_000)));
    let app = build_native_router(NativeApiState::new(Arc::clone(&service)));
    let marker = format!("report-endpoint-marker-{}", std::process::id());
    ProblemLog::global().record("ERROR", "drevo::test", &marker, Vec::new());

    let (status, r) = get_json(&app, "/report").await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(r["server"]["name"], "drevo");
    assert_eq!(r["server"]["version"], drevo::VERSION);
    assert_eq!(r["server"]["engine"], "native-durable");
    assert!(r["server"]["uptime_seconds"].is_u64());
    assert!(r["server"].get("git_sha").is_some());
    assert!(r["server"].get("build_date").is_some());

    assert_eq!(r["config"]["statement_timeout_ms"], 30_000);

    assert_eq!(r["graph"]["database"], "drevo");
    assert_eq!(r["graph"]["nodes"], 3);
    assert_eq!(r["graph"]["edges"], 2);
    let labels: Vec<&str> = r["graph"]["labels"]
        .as_array()
        .expect("labels")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(labels, vec!["Person", "Topic"]);

    assert!(r["problems"]
        .as_array()
        .expect("problems")
        .iter()
        .any(|p| p["message"] == marker.as_str()));
    assert_eq!(r["issue_repo"], "ice1x/drevo");
    assert!(r["generated_at"].as_str().is_some_and(|s| s.ends_with('Z')));
}

#[tokio::test]
async fn report_without_a_timeout_says_off() {
    let app = build_native_router(NativeApiState::new(seeded()));
    let (_, r) = get_json(&app, "/report").await;
    assert!(r["config"]["statement_timeout_ms"].is_null());
}

#[tokio::test]
async fn report_targets_a_named_database_and_404s_an_unknown_one() {
    let app = build_native_router(NativeApiState::new(seeded()));
    let create = Request::builder()
        .method("POST")
        .uri("/cypher")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"query": "CREATE DATABASE other"}"#))
        .expect("request");
    assert!(app
        .clone()
        .oneshot(create)
        .await
        .expect("response")
        .status()
        .is_success());

    let (status, r) = get_json(&app, "/report?db=other").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(r["graph"]["database"], "other");
    assert_eq!(r["graph"]["nodes"], 0);

    let (status, _) = get_json(&app, "/report?db=missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
