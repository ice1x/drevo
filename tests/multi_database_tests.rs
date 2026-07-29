//! Integration tests for the multi-database catalog (create / list /
//! switch) exposed over HTTP.
//!
//! The catalog gives one process several named databases, each its own redb
//! file (see `src/catalog.rs`). Over HTTP:
//!
//! - `GET  /databases` lists them; `POST /databases` creates one.
//! - Every data endpoint selects a database with the `X-Drevo-Database`
//!   header or a `?db=<name>` query, defaulting to `default`.
//!
//! These tests drive the router with an in-memory catalog (`ApiState::new`),
//! so creating a database yields a fresh in-memory `Drevo` — enough to prove
//! routing and isolation without touching disk.

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
    build_router(ApiState::new(db))
}

/// Send a request, optionally selecting a database via the
/// `X-Drevo-Database` header. Returns `(status, json_body)`.
async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    db: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(name) = db {
        req = req.header("x-drevo-database", name);
    }
    let req = if let Some(ref value) = body {
        req.header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(value).expect("serialize body"),
            ))
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
        .expect("collect body")
        .to_bytes();
    let value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, value)
}

fn node_body(kind: &str, title: &str) -> Value {
    json!({ "kind": kind, "title": title, "body": "", "body_html": "", "properties": {} })
}

// ── listing ─────────────────────────────────────────────────────────────
#[tokio::test]
async fn get_databases_lists_default() {
    let app = make_app();
    let (status, body) = send(&app, "GET", "/databases", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["default"], "default");
    let names: Vec<&str> = body["databases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["default"]);
}

// ── creation ────────────────────────────────────────────────────────────
#[tokio::test]
async fn post_databases_creates_and_appears_in_list() {
    let app = make_app();
    let (status, body) = send(
        &app,
        "POST",
        "/databases",
        None,
        Some(json!({ "name": "projectA" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "projectA");

    let (_, list) = send(&app, "GET", "/databases", None, None).await;
    let names: Vec<&str> = list["databases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["default", "projectA"]);
}

#[tokio::test]
async fn post_databases_missing_name_is_400() {
    let app = make_app();
    let (status, _) = send(&app, "POST", "/databases", None, Some(json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_databases_invalid_name_is_400() {
    let app = make_app();
    let (status, _) = send(
        &app,
        "POST",
        "/databases",
        None,
        Some(json!({ "name": "bad name/../x" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_databases_duplicate_is_409() {
    let app = make_app();
    let body = json!({ "name": "dup" });
    let (first, _) = send(&app, "POST", "/databases", None, Some(body.clone())).await;
    assert_eq!(first, StatusCode::CREATED);
    let (second, _) = send(&app, "POST", "/databases", None, Some(body)).await;
    assert_eq!(second, StatusCode::CONFLICT);
}

// ── selection / isolation ───────────────────────────────────────────────
#[tokio::test]
async fn data_is_isolated_between_databases() {
    let app = make_app();
    // Create a second database and a node inside it (via the header).
    send(
        &app,
        "POST",
        "/databases",
        None,
        Some(json!({ "name": "projectA" })),
    )
    .await;
    let (status, created) = send(
        &app,
        "POST",
        "/nodes",
        Some("projectA"),
        Some(node_body("task", "only-in-A")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["title"], "only-in-A");

    // projectA sees the node...
    let (_, in_a) = send(&app, "GET", "/nodes?kind=task", Some("projectA"), None).await;
    assert_eq!(in_a["nodes"].as_array().unwrap().len(), 1);

    // ...the default database does not (separate handle, separate data).
    let (_, in_default) = send(&app, "GET", "/nodes?kind=task", None, None).await;
    assert_eq!(in_default["nodes"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn query_param_selects_database() {
    let app = make_app();
    send(
        &app,
        "POST",
        "/databases",
        None,
        Some(json!({ "name": "viaquery" })),
    )
    .await;
    // No header — selection comes from `?db=`.
    let (status, _) = send(
        &app,
        "POST",
        "/nodes?db=viaquery",
        None,
        Some(node_body("note", "q")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, listed) = send(&app, "GET", "/nodes?kind=note&db=viaquery", None, None).await;
    assert_eq!(listed["nodes"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn unknown_database_is_404() {
    let app = make_app();
    let (status, _) = send(&app, "GET", "/nodes?kind=task", Some("ghost"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn default_database_used_when_unspecified() {
    let app = make_app();
    let (status, _) = send(&app, "POST", "/nodes", None, Some(node_body("k", "t"))).await;
    assert_eq!(status, StatusCode::CREATED);
    // Same node is visible without any selector (both hit `default`).
    let (_, listed) = send(&app, "GET", "/nodes?kind=k", None, None).await;
    assert_eq!(listed["nodes"].as_array().unwrap().len(), 1);
}
