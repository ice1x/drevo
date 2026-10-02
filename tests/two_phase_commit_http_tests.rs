//! Two-phase commit over HTTP — #556 slice 2.
//!
//! HTTP has no explicit transactions, so prepare happens over Bolt, drevo-py
//! or the Rust API. HTTP gives operators and coordinators the rest:
//! `GET /transactions/prepared`, `POST /transactions/prepared/{gid}/commit`
//! and `…/rollback`, a 503 for writes refused by the fence, and the
//! `drevo_prepared_transactions` / `drevo_prepared_transaction_oldest_age_seconds`
//! gauges on `/metrics`.

#![cfg(feature = "http")]

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::cypher::parser::parse;
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn call(app: &axum::Router, method: &str, uri: &str, body: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = app.clone().oneshot(req).await.expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn prepared(svc: &NativeService, gid: &str) {
    let tx = svc.begin_tx();
    svc.execute_in_tx(tx, &parse("CREATE (:T)").unwrap(), HashMap::new())
        .unwrap();
    svc.prepare_tx(tx, gid).unwrap();
}

fn app_with_prepared(gid: &str) -> (axum::Router, Arc<NativeService>) {
    let svc = Arc::new(NativeService::in_memory());
    prepared(&svc, gid);
    (
        build_native_router(NativeApiState::new(Arc::clone(&svc))),
        svc,
    )
}

#[tokio::test]
async fn list_then_commit_over_http() {
    let (app, svc) = app_with_prepared("http-1");

    let (status, body) = call(&app, "GET", "/transactions/prepared", "").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let list = json["prepared"].as_array().expect("prepared array");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["gid"], "http-1");
    assert_eq!(list[0]["op_count"], 1);
    assert!(list[0]["prepared_at"].as_str().unwrap().ends_with('Z'));

    let (status, _) = call(&app, "POST", "/transactions/prepared/http-1/commit", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(svc.list_prepared().is_empty());
    let (_, body) = call(&app, "GET", "/transactions/prepared", "").await;
    assert_eq!(body, r#"{"prepared":[]}"#);
}

#[tokio::test]
async fn rollback_over_http_and_unknown_gid_is_404() {
    let (app, svc) = app_with_prepared("http-2");
    let (status, _) = call(&app, "POST", "/transactions/prepared/http-2/rollback", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(svc.list_prepared().is_empty());

    for action in ["commit", "rollback"] {
        let (status, body) = call(
            &app,
            "POST",
            &format!("/transactions/prepared/http-2/{action}"),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{action}: {body}");
        assert!(body.contains("http-2"), "{body}");
    }
}

#[tokio::test]
async fn writes_refused_by_the_fence_are_503() {
    let (app, _svc) = app_with_prepared("http-3");
    let (status, body) = call(&app, "POST", "/cypher", r#"{"query": "CREATE (:X)"}"#).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("http-3") && body.contains("retry"), "{body}");

    let (status, _) = call(
        &app,
        "POST",
        "/nodes",
        r#"{"kind": "note", "title": "blocked", "body": "", "body_html": "", "properties": {}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // Reads are unaffected.
    let (status, _) = call(
        &app,
        "POST",
        "/cypher",
        r#"{"query": "MATCH (n) RETURN count(n) AS c"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn metrics_report_prepared_transactions() {
    let (app, svc) = app_with_prepared("http-4");
    let (_, metrics) = call(&app, "GET", "/metrics", "").await;
    let value = |name: &str| -> f64 {
        metrics
            .lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("{name} missing"))
            .rsplit(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    assert_eq!(value("drevo_prepared_transactions"), 1.0);
    assert!(value("drevo_prepared_transaction_oldest_age_seconds") >= 0.0);

    svc.rollback_prepared("http-4").unwrap();
    let (_, metrics) = call(&app, "GET", "/metrics", "").await;
    assert!(metrics
        .lines()
        .any(|l| l == "drevo_prepared_transactions 0"));
}
