//! Integration tests for the HTTP API (tasks 00037, 00038, 00039).
//!
//! Covers the scaffold (task 00037: router, state, error mapping),
//! the node CRUD endpoints (task 00038: POST/GET/PATCH/DELETE /nodes
//! plus the list-by-kind query), and the edge endpoints (task 00039:
//! POST/GET/DELETE /edges, list-by-kind, and edges-of-node).

#![cfg(feature = "http")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphnote_db::api::{build_router, ApiState};
use graphnote_db::db::GraphNoteDb;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn make_app() -> axum::Router {
    let db = Arc::new(GraphNoteDb::open_in_memory().expect("open in-memory db"));
    let state = ApiState::new(db);
    build_router(state)
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(uri);
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

fn new_node_body(kind: &str, title: &str, body: &str) -> Value {
    json!({
        "kind": kind,
        "title": title,
        "body": body,
        "body_html": "",
        "properties": {}
    })
}

fn new_edge_body(from_id: u64, to_id: u64, kind: &str) -> Value {
    json!({
        "from_id": from_id,
        "to_id": to_id,
        "kind": kind,
        "weight": 1.0,
        "properties": {}
    })
}

async fn create_two_nodes(app: &axum::Router) -> (u64, u64) {
    let (_, a) = send(app, "POST", "/nodes", Some(new_node_body("note", "A", ""))).await;
    let (_, b) = send(app, "POST", "/nodes", Some(new_node_body("note", "B", ""))).await;
    (
        a["id"].as_u64().expect("node a id"),
        b["id"].as_u64().expect("node b id"),
    )
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

// ---------------------------------------------------------------------
// Task 00038 — Node CRUD endpoints
// ---------------------------------------------------------------------

#[tokio::test]
async fn post_nodes_creates_node_and_returns_201() {
    let app = make_app();
    let (status, value) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "Hello", "world")),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(value["id"].as_u64().unwrap() >= 1);
    assert_eq!(value["kind"], "note");
    assert_eq!(value["title"], "Hello");
    assert_eq!(value["body"], "world");
    assert!(value["created_at"].is_number());
    assert!(value["updated_at"].is_number());
}

#[tokio::test]
async fn post_nodes_duplicate_title_returns_409() {
    let app = make_app();
    let body = new_node_body("note", "Dup", "a");
    let (first, _) = send(&app, "POST", "/nodes", Some(body.clone())).await;
    assert_eq!(first, StatusCode::CREATED);

    let (second, err) = send(&app, "POST", "/nodes", Some(body)).await;
    assert_eq!(second, StatusCode::CONFLICT);
    assert!(err["error"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("dup"));
}

#[tokio::test]
async fn post_nodes_rejects_invalid_json_with_400() {
    let app = make_app();
    let req = Request::builder()
        .method("POST")
        .uri("/nodes")
        .header("content-type", "application/json")
        .body(Body::from("{not-json"))
        .unwrap();
    let response = app.oneshot(req).await.expect("router response");
    // axum's Json extractor rejects malformed bodies as 400-class errors.
    assert!(response.status().is_client_error());
}

#[tokio::test]
async fn get_nodes_id_returns_existing_node() {
    let app = make_app();
    let (_, created) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "Target", "payload")),
    )
    .await;
    let id = created["id"].as_u64().unwrap();

    let (status, value) = send(&app, "GET", &format!("/nodes/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["id"].as_u64().unwrap(), id);
    assert_eq!(value["title"], "Target");
    assert_eq!(value["body"], "payload");
}

#[tokio::test]
async fn get_nodes_missing_returns_404() {
    let app = make_app();
    let (status, value) = send(&app, "GET", "/nodes/9999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(value["error"].is_string());
}

#[tokio::test]
async fn patch_nodes_updates_fields() {
    let app = make_app();
    let (_, created) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "Orig", "body")),
    )
    .await;
    let id = created["id"].as_u64().unwrap();
    let old_updated = created["updated_at"].as_i64().unwrap();

    // Sleep 2ms so updated_at strictly increases (ms resolution).
    std::thread::sleep(std::time::Duration::from_millis(2));

    let patch = json!({ "title": "Renamed", "body": "new" });
    let (status, value) = send(&app, "PATCH", &format!("/nodes/{id}"), Some(patch)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["id"].as_u64().unwrap(), id);
    assert_eq!(value["title"], "Renamed");
    assert_eq!(value["body"], "new");
    assert!(value["updated_at"].as_i64().unwrap() >= old_updated);
}

#[tokio::test]
async fn patch_nodes_missing_returns_404() {
    let app = make_app();
    let (status, _) = send(&app, "PATCH", "/nodes/42", Some(json!({ "title": "x" }))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn patch_nodes_duplicate_title_returns_409() {
    let app = make_app();
    let (_, a) = send(&app, "POST", "/nodes", Some(new_node_body("note", "A", ""))).await;
    let (_, b) = send(&app, "POST", "/nodes", Some(new_node_body("note", "B", ""))).await;
    let _ = a;
    let id_b = b["id"].as_u64().unwrap();
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("/nodes/{id_b}"),
        Some(json!({ "title": "A" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn delete_nodes_removes_node_and_returns_204() {
    let app = make_app();
    let (_, created) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "ToDelete", "")),
    )
    .await;
    let id = created["id"].as_u64().unwrap();

    let (status, _) = send(&app, "DELETE", &format!("/nodes/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = send(&app, "GET", &format!("/nodes/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_nodes_missing_returns_404() {
    let app = make_app();
    let (status, _) = send(&app, "DELETE", "/nodes/404", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_nodes_by_kind_paginates() {
    let app = make_app();
    for i in 0..5 {
        let (status, _) = send(
            &app,
            "POST",
            "/nodes",
            Some(new_node_body("note", &format!("n{i}"), "")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    // Add one of a different kind to ensure filtering works.
    let (_, _) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("task", "t0", "")),
    )
    .await;

    let (status, value) = send(&app, "GET", "/nodes?kind=note&limit=3&offset=0", None).await;
    assert_eq!(status, StatusCode::OK);
    let nodes = value["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 3);
    for node in nodes {
        assert_eq!(node["kind"], "note");
    }

    let (status, value) = send(&app, "GET", "/nodes?kind=note&limit=10&offset=3", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["nodes"].as_array().unwrap().len(), 2);

    // kind is required for list endpoint.
    let (status, _) = send(&app, "GET", "/nodes", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------
// Task 00039 — Edge endpoints
// ---------------------------------------------------------------------

#[tokio::test]
async fn post_edges_creates_edge_and_returns_201() {
    let app = make_app();
    let (from, to) = create_two_nodes(&app).await;

    let (status, value) = send(
        &app,
        "POST",
        "/edges",
        Some(new_edge_body(from, to, "links_to")),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(value["id"].as_u64().unwrap() >= 1);
    assert_eq!(value["from_id"].as_u64().unwrap(), from);
    assert_eq!(value["to_id"].as_u64().unwrap(), to);
    assert_eq!(value["kind"], "links_to");
    assert!((value["weight"].as_f64().unwrap() - 1.0).abs() < 1e-6);
    assert!(value["created_at"].is_number());
}

#[tokio::test]
async fn post_edges_missing_endpoint_returns_404() {
    let app = make_app();
    let (from, _) = create_two_nodes(&app).await;

    let (status, value) = send(
        &app,
        "POST",
        "/edges",
        Some(new_edge_body(from, 9999, "links_to")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(value["error"].is_string());
}

#[tokio::test]
async fn post_edges_rejects_invalid_json_with_400() {
    let app = make_app();
    let req = Request::builder()
        .method("POST")
        .uri("/edges")
        .header("content-type", "application/json")
        .body(Body::from("{broken"))
        .unwrap();
    let response = app.oneshot(req).await.expect("router response");
    assert!(response.status().is_client_error());
}

#[tokio::test]
async fn get_edges_id_returns_existing_edge() {
    let app = make_app();
    let (from, to) = create_two_nodes(&app).await;
    let (_, created) = send(
        &app,
        "POST",
        "/edges",
        Some(new_edge_body(from, to, "links_to")),
    )
    .await;
    let id = created["id"].as_u64().unwrap();

    let (status, value) = send(&app, "GET", &format!("/edges/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["id"].as_u64().unwrap(), id);
    assert_eq!(value["from_id"].as_u64().unwrap(), from);
    assert_eq!(value["to_id"].as_u64().unwrap(), to);
    assert_eq!(value["kind"], "links_to");
}

#[tokio::test]
async fn get_edges_missing_returns_404() {
    let app = make_app();
    let (status, value) = send(&app, "GET", "/edges/9999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(value["error"].is_string());
}

#[tokio::test]
async fn delete_edges_removes_edge_and_returns_204() {
    let app = make_app();
    let (from, to) = create_two_nodes(&app).await;
    let (_, created) = send(
        &app,
        "POST",
        "/edges",
        Some(new_edge_body(from, to, "links_to")),
    )
    .await;
    let id = created["id"].as_u64().unwrap();

    let (status, _) = send(&app, "DELETE", &format!("/edges/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = send(&app, "GET", &format!("/edges/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_edges_missing_returns_404() {
    let app = make_app();
    let (status, _) = send(&app, "DELETE", "/edges/404", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_edges_by_kind_paginates() {
    let app = make_app();
    let (from, to) = create_two_nodes(&app).await;
    for _ in 0..5 {
        let (status, _) = send(
            &app,
            "POST",
            "/edges",
            Some(new_edge_body(from, to, "links_to")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    // Different kind to ensure filtering works.
    let (_, _) = send(
        &app,
        "POST",
        "/edges",
        Some(new_edge_body(from, to, "tagged_with")),
    )
    .await;

    let (status, value) = send(&app, "GET", "/edges?kind=links_to&limit=3&offset=0", None).await;
    assert_eq!(status, StatusCode::OK);
    let edges = value["edges"].as_array().expect("edges array");
    assert_eq!(edges.len(), 3);
    for edge in edges {
        assert_eq!(edge["kind"], "links_to");
    }

    let (status, value) = send(&app, "GET", "/edges?kind=links_to&limit=10&offset=3", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["edges"].as_array().unwrap().len(), 2);

    // kind is required.
    let (status, _) = send(&app, "GET", "/edges", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_node_edges_returns_directional_edges() {
    let app = make_app();
    let (a, b) = create_two_nodes(&app).await;
    // a -> b (outgoing for a, incoming for b)
    let (_, _) = send(
        &app,
        "POST",
        "/edges",
        Some(new_edge_body(a, b, "links_to")),
    )
    .await;
    // b -> a (incoming for a, outgoing for b)
    let (_, _) = send(
        &app,
        "POST",
        "/edges",
        Some(new_edge_body(b, a, "replies_to")),
    )
    .await;

    let (status, value) = send(
        &app,
        "GET",
        &format!("/nodes/{a}/edges?direction=outgoing"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let edges = value["edges"].as_array().expect("edges array");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["from_id"].as_u64().unwrap(), a);
    assert_eq!(edges[0]["kind"], "links_to");

    let (status, value) = send(
        &app,
        "GET",
        &format!("/nodes/{a}/edges?direction=incoming"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let edges = value["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["to_id"].as_u64().unwrap(), a);
    assert_eq!(edges[0]["kind"], "replies_to");

    let (status, value) = send(
        &app,
        "GET",
        &format!("/nodes/{a}/edges?direction=both"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["edges"].as_array().unwrap().len(), 2);

    // Default direction = both
    let (status, value) = send(&app, "GET", &format!("/nodes/{a}/edges"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["edges"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn get_node_edges_invalid_direction_returns_400() {
    let app = make_app();
    let (a, _) = create_two_nodes(&app).await;
    let (status, value) = send(
        &app,
        "GET",
        &format!("/nodes/{a}/edges?direction=sideways"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(value["error"].is_string());
}
