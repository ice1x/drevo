//! Integration tests for the HTTP API scaffold (task 00037).
//!
//! Validates that the axum `Router` can be built from a `GraphNoteDb`
//! and that the root endpoint returns server metadata. Endpoint-level
//! tests for nodes, edges, traversal, and search live in later tasks.

#![cfg(feature = "http")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphnote_db::api::{build_router, ApiState};
use graphnote_db::db::GraphNoteDb;
use http_body_util::BodyExt;
use tower::ServiceExt;

fn make_app() -> axum::Router {
    let db = Arc::new(GraphNoteDb::open_in_memory().expect("open in-memory db"));
    let state = ApiState::new(db);
    build_router(state)
}

#[tokio::test]
async fn root_returns_server_info() {
    let app = make_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/")
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("router response");

    assert_eq!(response.status(), StatusCode::OK);

    let body = response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&body).expect("json body");

    assert_eq!(value["name"], "graphnote-db");
    assert!(
        value["version"].is_string(),
        "version should be a string, got {value:?}"
    );
    let version = value["version"].as_str().unwrap();
    assert!(!version.is_empty(), "version should not be empty");
}

#[tokio::test]
async fn unknown_route_returns_404() {
    let app = make_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/does-not-exist")
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("router response");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_state_is_cloneable_and_shares_db() {
    // The state must be `Clone` so axum can hand it to every request
    // without recreating the database. Both clones must observe the
    // same underlying `GraphNoteDb`.
    let db = Arc::new(GraphNoteDb::open_in_memory().expect("open in-memory db"));
    let state_a = ApiState::new(Arc::clone(&db));
    let state_b = state_a.clone();

    assert!(Arc::ptr_eq(&state_a.db, &state_b.db));
    assert_eq!(Arc::strong_count(&db), 3);
}
