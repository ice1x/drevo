//! Integration tests for the HTTP API (tasks 00037–00044).
//!
//! Covers the scaffold (task 00037: router, state, error mapping),
//! the node CRUD endpoints (task 00038: POST/GET/PATCH/DELETE /nodes
//! plus the list-by-kind query), the edge endpoints (task 00039:
//! POST/GET/DELETE /edges, list-by-kind, and edges-of-node), the
//! traversal endpoints (task 00040: /nodes/{id}/neighbors,
//! /paths/shortest, /nodes/{id}/subgraph), the full-text search
//! endpoint (task 00041: POST /search/fts), the admin endpoints
//! (task 00042: GET /health, GET /status), the unified JSON
//! error handling (task 00043), and end-to-end integration tests
//! exercising full workflows through the HTTP API (task 00044).

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

// ---------------------------------------------------------------------
// Task 00040 — Traversal endpoints
// ---------------------------------------------------------------------

/// Build a small chain of nodes `a -> b -> c -> d` connected by
/// `links_to` edges. Returns the four node ids in order.
async fn build_chain(app: &axum::Router) -> (u64, u64, u64, u64) {
    let (_, a) = send(app, "POST", "/nodes", Some(new_node_body("note", "A", ""))).await;
    let (_, b) = send(app, "POST", "/nodes", Some(new_node_body("note", "B", ""))).await;
    let (_, c) = send(app, "POST", "/nodes", Some(new_node_body("note", "C", ""))).await;
    let (_, d) = send(app, "POST", "/nodes", Some(new_node_body("note", "D", ""))).await;
    let a = a["id"].as_u64().unwrap();
    let b = b["id"].as_u64().unwrap();
    let c = c["id"].as_u64().unwrap();
    let d = d["id"].as_u64().unwrap();
    for (from, to) in [(a, b), (b, c), (c, d)] {
        let (status, _) = send(
            app,
            "POST",
            "/edges",
            Some(new_edge_body(from, to, "links_to")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    (a, b, c, d)
}

#[tokio::test]
async fn get_node_neighbors_default_outgoing_depth_one() {
    let app = make_app();
    let (a, b, _c, _d) = build_chain(&app).await;

    let (status, value) = send(&app, "GET", &format!("/nodes/{a}/neighbors"), None).await;
    assert_eq!(status, StatusCode::OK);
    let nodes = value["nodes"].as_array().expect("nodes array");
    // Default depth=1 returns only direct neighbor `b`.
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["id"].as_u64().unwrap(), b);
}

#[tokio::test]
async fn get_node_neighbors_with_depth_follows_chain() {
    let app = make_app();
    let (a, b, c, _d) = build_chain(&app).await;

    let (status, value) = send(&app, "GET", &format!("/nodes/{a}/neighbors?depth=2"), None).await;
    assert_eq!(status, StatusCode::OK);
    let nodes = value["nodes"].as_array().unwrap();
    let ids: std::collections::HashSet<u64> =
        nodes.iter().map(|n| n["id"].as_u64().unwrap()).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&b));
    assert!(ids.contains(&c));
}

#[tokio::test]
async fn get_node_neighbors_respects_direction_and_kind() {
    let app = make_app();
    let (a, b) = create_two_nodes(&app).await;
    // a -> b via links_to
    let (_, _) = send(
        &app,
        "POST",
        "/edges",
        Some(new_edge_body(a, b, "links_to")),
    )
    .await;
    // b -> a via replies_to
    let (_, _) = send(
        &app,
        "POST",
        "/edges",
        Some(new_edge_body(b, a, "replies_to")),
    )
    .await;

    // Outgoing from a → only b (via links_to).
    let (status, value) = send(
        &app,
        "GET",
        &format!("/nodes/{a}/neighbors?direction=outgoing"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let nodes = value["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["id"].as_u64().unwrap(), b);

    // Incoming to a → only b (via replies_to).
    let (status, value) = send(
        &app,
        "GET",
        &format!("/nodes/{a}/neighbors?direction=incoming"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["nodes"].as_array().unwrap().len(), 1);

    // Both directions filtered by edge kind=replies_to → only the incoming neighbor.
    let (status, value) = send(
        &app,
        "GET",
        &format!("/nodes/{a}/neighbors?direction=both&kind=replies_to"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let nodes = value["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["id"].as_u64().unwrap(), b);

    // Filter on a non-existent kind → empty.
    let (status, value) = send(
        &app,
        "GET",
        &format!("/nodes/{a}/neighbors?kind=missing"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(value["nodes"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn get_node_neighbors_missing_returns_404() {
    let app = make_app();
    let (status, value) = send(&app, "GET", "/nodes/9999/neighbors", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(value["error"].is_string());
}

#[tokio::test]
async fn get_node_neighbors_invalid_direction_returns_400() {
    let app = make_app();
    let (a, _) = create_two_nodes(&app).await;
    let (status, _) = send(
        &app,
        "GET",
        &format!("/nodes/{a}/neighbors?direction=up"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_paths_shortest_returns_path_ids() {
    let app = make_app();
    let (a, b, c, d) = build_chain(&app).await;

    let (status, value) = send(
        &app,
        "GET",
        &format!("/paths/shortest?from={a}&to={d}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let path = value["path"].as_array().expect("path array");
    let ids: Vec<u64> = path.iter().map(|v| v.as_u64().unwrap()).collect();
    assert_eq!(ids, vec![a, b, c, d]);
}

#[tokio::test]
async fn get_paths_shortest_unreachable_returns_null_path() {
    let app = make_app();
    let (a, b) = create_two_nodes(&app).await;
    // No edges; b is not reachable from a.
    let (status, value) = send(
        &app,
        "GET",
        &format!("/paths/shortest?from={a}&to={b}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(value["path"].is_null());
}

#[tokio::test]
async fn get_paths_shortest_missing_source_returns_404() {
    let app = make_app();
    let (_, b) = create_two_nodes(&app).await;
    let (status, value) = send(
        &app,
        "GET",
        &format!("/paths/shortest?from=9999&to={b}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(value["error"].is_string());
}

#[tokio::test]
async fn get_paths_shortest_missing_target_returns_404() {
    let app = make_app();
    let (a, _) = create_two_nodes(&app).await;
    let (status, _) = send(
        &app,
        "GET",
        &format!("/paths/shortest?from={a}&to=9999"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_paths_shortest_missing_params_returns_400() {
    let app = make_app();
    let (status, _) = send(&app, "GET", "/paths/shortest", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = send(&app, "GET", "/paths/shortest?from=1", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_node_subgraph_returns_nodes_and_edges() {
    let app = make_app();
    let (a, b, c, _d) = build_chain(&app).await;

    // depth=2 from a discovers a, b, c; edges between them (a->b, b->c).
    let (status, value) = send(&app, "GET", &format!("/nodes/{a}/subgraph?depth=2"), None).await;
    assert_eq!(status, StatusCode::OK);
    let nodes = value["nodes"].as_array().unwrap();
    let ids: std::collections::HashSet<u64> =
        nodes.iter().map(|n| n["id"].as_u64().unwrap()).collect();
    assert!(ids.contains(&a));
    assert!(ids.contains(&b));
    assert!(ids.contains(&c));
    assert_eq!(ids.len(), 3);

    let edges = value["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2);
}

#[tokio::test]
async fn get_node_subgraph_default_depth_one() {
    let app = make_app();
    let (a, b, _c, _d) = build_chain(&app).await;

    let (status, value) = send(&app, "GET", &format!("/nodes/{a}/subgraph"), None).await;
    assert_eq!(status, StatusCode::OK);
    let nodes = value["nodes"].as_array().unwrap();
    let ids: std::collections::HashSet<u64> =
        nodes.iter().map(|n| n["id"].as_u64().unwrap()).collect();
    // Depth=1 discovers only a and b; a->b edge is included.
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&a));
    assert!(ids.contains(&b));
    assert_eq!(value["edges"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn get_node_subgraph_missing_returns_404() {
    let app = make_app();
    let (status, value) = send(&app, "GET", "/nodes/9999/subgraph", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(value["error"].is_string());
}

// ---------------------------------------------------------------------
// Search endpoint (task 00041) — POST /search/fts
// ---------------------------------------------------------------------

async fn seed_search_corpus(app: &axum::Router) -> (u64, u64, u64) {
    let (_, a) = send(
        app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "note",
            "Rust programming language",
            "Rust is a systems programming language focused on safety and performance.",
        )),
    )
    .await;
    let (_, b) = send(
        app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "note",
            "Python tutorial",
            "Python is a high-level interpreted programming language.",
        )),
    )
    .await;
    let (_, c) = send(
        app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "note",
            "Cooking recipes",
            "Delicious pasta and pizza recipes from Italy.",
        )),
    )
    .await;
    (
        a["id"].as_u64().unwrap(),
        b["id"].as_u64().unwrap(),
        c["id"].as_u64().unwrap(),
    )
}

#[tokio::test]
async fn search_fts_returns_scored_results() {
    let app = make_app();
    let (rust_id, _py_id, _cook_id) = seed_search_corpus(&app).await;

    let (status, value) = send(
        &app,
        "POST",
        "/search/fts",
        Some(json!({ "query": "Rust programming", "limit": 10 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let results = value["results"].as_array().expect("results array");
    assert!(!results.is_empty(), "expected at least one result");

    // First result should be the "Rust programming language" node since it
    // matches both query tokens most strongly.
    let top = &results[0];
    assert_eq!(top["node"]["id"].as_u64().unwrap(), rust_id);
    assert!(
        top["score"].as_f64().unwrap() > 0.0,
        "expected positive score"
    );
}

#[tokio::test]
async fn search_fts_respects_limit() {
    let app = make_app();
    let _ = seed_search_corpus(&app).await;

    let (status, value) = send(
        &app,
        "POST",
        "/search/fts",
        Some(json!({ "query": "programming", "limit": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results = value["results"].as_array().unwrap();
    assert!(results.len() <= 1);
}

#[tokio::test]
async fn search_fts_default_limit_when_omitted() {
    let app = make_app();
    let _ = seed_search_corpus(&app).await;

    let (status, value) = send(
        &app,
        "POST",
        "/search/fts",
        Some(json!({ "query": "programming" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results = value["results"].as_array().unwrap();
    // Default limit is non-zero; expect at least one match.
    assert!(!results.is_empty());
}

#[tokio::test]
async fn search_fts_no_match_returns_empty_results() {
    let app = make_app();
    let _ = seed_search_corpus(&app).await;

    let (status, value) = send(
        &app,
        "POST",
        "/search/fts",
        Some(json!({ "query": "zzzzz_nothing_matches", "limit": 10 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results = value["results"].as_array().unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn search_fts_missing_query_returns_400() {
    let app = make_app();
    let (status, value) = send(&app, "POST", "/search/fts", Some(json!({ "limit": 10 }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(value["error"].is_string());
}

#[tokio::test]
async fn search_fts_empty_query_returns_empty_results() {
    let app = make_app();
    let _ = seed_search_corpus(&app).await;

    let (status, value) = send(
        &app,
        "POST",
        "/search/fts",
        Some(json!({ "query": "", "limit": 10 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(value["results"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn search_fts_malformed_body_returns_400() {
    let app = make_app();
    let req = Request::builder()
        .method("POST")
        .uri("/search/fts")
        .header("content-type", "application/json")
        .body(Body::from("not-json"))
        .expect("build request");
    let response = app.oneshot(req).await.expect("router response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn search_fts_limit_zero_returns_empty() {
    let app = make_app();
    let _ = seed_search_corpus(&app).await;

    let (status, value) = send(
        &app,
        "POST",
        "/search/fts",
        Some(json!({ "query": "programming", "limit": 0 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(value["results"].as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------
// Task 00042 — Admin endpoints (GET /health, GET /status)
// ---------------------------------------------------------------------

#[tokio::test]
async fn get_health_returns_ok_status() {
    let app = make_app();
    let (status, value) = send(&app, "GET", "/health", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["status"], "ok");
}

#[tokio::test]
async fn get_health_is_cheap_and_does_not_touch_state() {
    // /health must work even before any database activity; it is meant
    // to be called by Kubernetes liveness probes on a fresh pod.
    let app = make_app();
    for _ in 0..5 {
        let (status, value) = send(&app, "GET", "/health", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["status"], "ok");
    }
}

#[tokio::test]
async fn get_status_returns_server_metadata() {
    let app = make_app();
    let (status, value) = send(&app, "GET", "/status", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["name"], "graphnote-db");
    assert!(value["version"].is_string());
    let version = value["version"].as_str().unwrap();
    assert!(!version.is_empty());
    assert!(
        value["uptime_seconds"].is_u64(),
        "uptime_seconds should be a non-negative integer, got {value:?}"
    );
}

#[tokio::test]
async fn get_status_uptime_is_monotonic() {
    // Two successive /status calls with a sleep between them must
    // report a non-decreasing uptime (either same second or higher).
    let app = make_app();
    let (_, first) = send(&app, "GET", "/status", None).await;
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (_, second) = send(&app, "GET", "/status", None).await;

    let a = first["uptime_seconds"].as_u64().unwrap();
    let b = second["uptime_seconds"].as_u64().unwrap();
    assert!(
        b >= a,
        "uptime must be monotonically non-decreasing: {a} then {b}"
    );
    assert!(
        b >= 1,
        "after a ~1s sleep uptime_seconds should be >= 1, got {b}"
    );
}

// ---------------------------------------------------------------------
// Task 00043 — Unified JSON error handling
// ---------------------------------------------------------------------

#[tokio::test]
async fn unknown_route_returns_json_404_with_status_field() {
    let app = make_app();
    let (status, value) = send(&app, "GET", "/does-not-exist", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(value["error"].is_string(), "error field must be a string");
    assert_eq!(
        value["status"].as_u64().unwrap(),
        404,
        "body must include numeric status code"
    );
}

#[tokio::test]
async fn method_not_allowed_returns_json_405() {
    let app = make_app();
    // PUT is not defined on /nodes — should yield 405.
    let (status, value) = send(&app, "PUT", "/nodes", Some(json!({}))).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(value["error"].is_string());
    assert_eq!(value["status"].as_u64().unwrap(), 405);
}

#[tokio::test]
async fn db_error_responses_include_status_field() {
    let app = make_app();
    // Node not found → 404 with status field.
    let (status, value) = send(&app, "GET", "/nodes/9999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(value["error"].is_string());
    assert_eq!(value["status"].as_u64().unwrap(), 404);
}

#[tokio::test]
async fn bad_request_responses_include_status_field() {
    let app = make_app();
    // Missing required 'kind' parameter → 400 with status field.
    let (status, value) = send(&app, "GET", "/nodes", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(value["error"].is_string());
    assert_eq!(value["status"].as_u64().unwrap(), 400);
}

#[tokio::test]
async fn conflict_responses_include_status_field() {
    let app = make_app();
    let body = new_node_body("note", "ConflictTest", "");
    let (first, _) = send(&app, "POST", "/nodes", Some(body.clone())).await;
    assert_eq!(first, StatusCode::CREATED);

    let (status, value) = send(&app, "POST", "/nodes", Some(body)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(value["error"].is_string());
    assert_eq!(value["status"].as_u64().unwrap(), 409);
}

#[tokio::test]
async fn error_response_content_type_is_json() {
    let app = make_app();
    let req = Request::builder()
        .method("GET")
        .uri("/does-not-exist")
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(req).await.expect("router response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let content_type = response
        .headers()
        .get("content-type")
        .expect("content-type header")
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("application/json"),
        "expected application/json, got {content_type}"
    );
}

#[tokio::test]
async fn malformed_json_body_returns_400_with_status_field() {
    let app = make_app();
    let req = Request::builder()
        .method("POST")
        .uri("/nodes")
        .header("content-type", "application/json")
        .body(Body::from("{not-json"))
        .unwrap();
    let response = app.clone().oneshot(req).await.expect("router response");
    let status = response.status();
    assert!(status.is_client_error());
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    let value: Value = serde_json::from_slice(&bytes).expect("json body");
    assert!(value["error"].is_string());
    assert_eq!(value["status"].as_u64().unwrap(), status.as_u16() as u64);
}

#[tokio::test]
async fn search_fts_missing_query_includes_status_400() {
    let app = make_app();
    let (status, value) = send(&app, "POST", "/search/fts", Some(json!({ "limit": 10 }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(value["status"].as_u64().unwrap(), 400);
}

#[tokio::test]
async fn shortest_path_missing_params_includes_status_400() {
    let app = make_app();
    let (status, value) = send(&app, "GET", "/paths/shortest", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(value["status"].as_u64().unwrap(), 400);
}

// =====================================================================
// Task 00044 — End-to-end integration tests
//
// These tests exercise full workflows through the HTTP API, combining
// multiple endpoints in realistic sequences that mirror how a real
// client would interact with GraphNote DB. Each test verifies
// cross-endpoint consistency and data integrity.
// =====================================================================

// ---------------------------------------------------------------------
// Lifecycle: node + edge CRUD through HTTP
// ---------------------------------------------------------------------

#[tokio::test]
async fn integration_full_node_lifecycle() {
    let app = make_app();

    // Create
    let (status, node) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "Lifecycle Test", "initial body")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = node["id"].as_u64().unwrap();

    // Read back
    let (status, fetched) = send(&app, "GET", &format!("/nodes/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["title"], "Lifecycle Test");
    assert_eq!(fetched["body"], "initial body");

    // Update
    std::thread::sleep(std::time::Duration::from_millis(2));
    let (status, updated) = send(
        &app,
        "PATCH",
        &format!("/nodes/{id}"),
        Some(json!({ "title": "Updated Title", "body": "updated body" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["title"], "Updated Title");
    assert_eq!(updated["body"], "updated body");
    assert!(updated["updated_at"].as_i64().unwrap() >= node["updated_at"].as_i64().unwrap());

    // Verify via list endpoint
    let (status, list) = send(&app, "GET", "/nodes?kind=note&limit=10", None).await;
    assert_eq!(status, StatusCode::OK);
    let nodes = list["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["title"], "Updated Title");

    // Delete
    let (status, _) = send(&app, "DELETE", &format!("/nodes/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify gone
    let (status, _) = send(&app, "GET", &format!("/nodes/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // List should be empty now
    let (status, list) = send(&app, "GET", "/nodes?kind=note&limit=10", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(list["nodes"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn integration_full_edge_lifecycle() {
    let app = make_app();
    let (a, b) = create_two_nodes(&app).await;

    // Create edge
    let (status, edge) = send(
        &app,
        "POST",
        "/edges",
        Some(new_edge_body(a, b, "links_to")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let edge_id = edge["id"].as_u64().unwrap();

    // Read back
    let (status, fetched) = send(&app, "GET", &format!("/edges/{edge_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["from_id"].as_u64().unwrap(), a);
    assert_eq!(fetched["to_id"].as_u64().unwrap(), b);

    // Visible via node edges endpoint
    let (status, resp) = send(&app, "GET", &format!("/nodes/{a}/edges"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["edges"].as_array().unwrap().len(), 1);

    // Visible via list edges by kind
    let (status, resp) = send(&app, "GET", "/edges?kind=links_to&limit=10", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["edges"].as_array().unwrap().len(), 1);

    // Delete edge
    let (status, _) = send(&app, "DELETE", &format!("/edges/{edge_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify gone from all endpoints
    let (status, _) = send(&app, "GET", &format!("/edges/{edge_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, resp) = send(&app, "GET", &format!("/nodes/{a}/edges"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["edges"].as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------
// Cascade: deleting a node removes its edges
// ---------------------------------------------------------------------

#[tokio::test]
async fn integration_node_deletion_cascades_to_edges() {
    let app = make_app();
    let (a, b) = create_two_nodes(&app).await;

    // Create edges in both directions
    let (_, e1) = send(
        &app,
        "POST",
        "/edges",
        Some(new_edge_body(a, b, "links_to")),
    )
    .await;
    let (_, e2) = send(
        &app,
        "POST",
        "/edges",
        Some(new_edge_body(b, a, "replies_to")),
    )
    .await;
    let e1_id = e1["id"].as_u64().unwrap();
    let e2_id = e2["id"].as_u64().unwrap();

    // Delete node a — both edges should be removed
    let (status, _) = send(&app, "DELETE", &format!("/nodes/{a}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Edges should be gone
    let (status, _) = send(&app, "GET", &format!("/edges/{e1_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&app, "GET", &format!("/edges/{e2_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Node b should have no edges
    let (status, resp) = send(&app, "GET", &format!("/nodes/{b}/edges"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["edges"].as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------
// FTS consistency: search reflects mutations
// ---------------------------------------------------------------------

#[tokio::test]
async fn integration_fts_reflects_node_creation_and_update() {
    let app = make_app();

    // Create a node with searchable content
    let (_, node) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "note",
            "Quantum computing basics",
            "Qubits and superposition in quantum mechanics",
        )),
    )
    .await;
    let id = node["id"].as_u64().unwrap();

    // Search should find it
    let (status, resp) = send(
        &app,
        "POST",
        "/search/fts",
        Some(json!({ "query": "quantum", "limit": 10 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results = resp["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0]["node"]["id"].as_u64().unwrap(), id);

    // Update body to completely different topic
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("/nodes/{id}"),
        Some(json!({
            "title": "Gardening tips",
            "body": "How to grow tomatoes and basil in your backyard"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Searching for new content should find it
    let (_, resp) = send(
        &app,
        "POST",
        "/search/fts",
        Some(json!({ "query": "tomatoes", "limit": 10 })),
    )
    .await;
    let results = resp["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0]["node"]["id"].as_u64().unwrap(), id);
}

#[tokio::test]
async fn integration_fts_reflects_node_deletion() {
    let app = make_app();

    let (_, node) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "note",
            "Ephemeral content for deletion test",
            "This unique xylophone content will be deleted",
        )),
    )
    .await;
    let id = node["id"].as_u64().unwrap();

    // Verify it is searchable
    let (_, resp) = send(
        &app,
        "POST",
        "/search/fts",
        Some(json!({ "query": "xylophone", "limit": 10 })),
    )
    .await;
    assert!(!resp["results"].as_array().unwrap().is_empty());

    // Delete the node
    let (status, _) = send(&app, "DELETE", &format!("/nodes/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // FTS should no longer return it
    let (_, resp) = send(
        &app,
        "POST",
        "/search/fts",
        Some(json!({ "query": "xylophone", "limit": 10 })),
    )
    .await;
    assert!(resp["results"].as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------
// Traversal through HTTP: multi-hop graph exploration
// ---------------------------------------------------------------------

#[tokio::test]
async fn integration_traversal_full_graph_exploration() {
    let app = make_app();

    // Build a diamond graph: a -> b, a -> c, b -> d, c -> d
    let (_, a) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "Root", "")),
    )
    .await;
    let (_, b) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "Left", "")),
    )
    .await;
    let (_, c) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "Right", "")),
    )
    .await;
    let (_, d) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "Sink", "")),
    )
    .await;
    let a = a["id"].as_u64().unwrap();
    let b = b["id"].as_u64().unwrap();
    let c = c["id"].as_u64().unwrap();
    let d = d["id"].as_u64().unwrap();

    for (from, to) in [(a, b), (a, c), (b, d), (c, d)] {
        let (status, _) = send(
            &app,
            "POST",
            "/edges",
            Some(new_edge_body(from, to, "links_to")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    // Neighbors from a at depth 1: b, c
    let (_, resp) = send(
        &app,
        "GET",
        &format!("/nodes/{a}/neighbors?depth=1&direction=outgoing"),
        None,
    )
    .await;
    let ids: std::collections::HashSet<u64> = resp["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&b));
    assert!(ids.contains(&c));

    // Neighbors from a at depth 2: b, c, d
    let (_, resp) = send(
        &app,
        "GET",
        &format!("/nodes/{a}/neighbors?depth=2&direction=outgoing"),
        None,
    )
    .await;
    let ids: std::collections::HashSet<u64> = resp["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(ids.contains(&d));

    // Shortest path a -> d
    let (_, resp) = send(
        &app,
        "GET",
        &format!("/paths/shortest?from={a}&to={d}"),
        None,
    )
    .await;
    let path: Vec<u64> = resp["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    assert_eq!(*path.first().unwrap(), a);
    assert_eq!(*path.last().unwrap(), d);
    // Should be 3 hops (a -> b/c -> d)
    assert_eq!(path.len(), 3);

    // Subgraph from a at depth 2: all 4 nodes, 4 edges
    let (_, resp) = send(&app, "GET", &format!("/nodes/{a}/subgraph?depth=2"), None).await;
    assert_eq!(resp["nodes"].as_array().unwrap().len(), 4);
    assert_eq!(resp["edges"].as_array().unwrap().len(), 4);
}

// ---------------------------------------------------------------------
// Scenario: CBT journal workflow through HTTP
// ---------------------------------------------------------------------

#[tokio::test]
async fn integration_scenario_cbt_journal() {
    let app = make_app();

    // Create CBT entities
    let (_, thought) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "thought",
            "I always fail",
            "Automatic negative thought about failure",
        )),
    )
    .await;
    let (_, emotion) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "emotion",
            "Anxiety",
            "Feeling anxious and worried",
        )),
    )
    .await;
    let (_, distortion) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "cognitive_distortion",
            "Overgeneralization",
            "Drawing broad conclusions from single events",
        )),
    )
    .await;
    let (_, rational) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "rational_response",
            "Evidence-based reframe",
            "I have succeeded many times before; one failure does not define me",
        )),
    )
    .await;

    let thought_id = thought["id"].as_u64().unwrap();
    let emotion_id = emotion["id"].as_u64().unwrap();
    let distortion_id = distortion["id"].as_u64().unwrap();
    let rational_id = rational["id"].as_u64().unwrap();

    // Create CBT relationship edges
    for (from, to, kind) in [
        (thought_id, emotion_id, "triggers"),
        (thought_id, distortion_id, "exhibits"),
        (distortion_id, rational_id, "challenged_by"),
    ] {
        let (status, _) = send(&app, "POST", "/edges", Some(new_edge_body(from, to, kind))).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    // Query: list all thoughts
    let (status, resp) = send(&app, "GET", "/nodes?kind=thought&limit=10", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["nodes"].as_array().unwrap().len(), 1);

    // Query: neighbors of the thought (emotion + distortion)
    let (_, resp) = send(
        &app,
        "GET",
        &format!("/nodes/{thought_id}/neighbors?direction=outgoing"),
        None,
    )
    .await;
    let neighbor_ids: std::collections::HashSet<u64> = resp["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_u64().unwrap())
        .collect();
    assert_eq!(neighbor_ids.len(), 2);
    assert!(neighbor_ids.contains(&emotion_id));
    assert!(neighbor_ids.contains(&distortion_id));

    // Query: subgraph from thought at depth 2 reaches the rational response
    let (_, resp) = send(
        &app,
        "GET",
        &format!("/nodes/{thought_id}/subgraph?depth=2"),
        None,
    )
    .await;
    let sub_ids: std::collections::HashSet<u64> = resp["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_u64().unwrap())
        .collect();
    assert!(sub_ids.contains(&rational_id));

    // FTS: search for distortion content
    let (_, resp) = send(
        &app,
        "POST",
        "/search/fts",
        Some(json!({ "query": "overgeneralization", "limit": 10 })),
    )
    .await;
    let results = resp["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0]["node"]["id"].as_u64().unwrap(), distortion_id);
}

// ---------------------------------------------------------------------
// Scenario: task dependency chain through HTTP
// ---------------------------------------------------------------------

#[tokio::test]
async fn integration_scenario_task_dependency_chain() {
    let app = make_app();

    // Create tasks with dependencies
    let (_, deploy) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "task",
            "Deploy to production",
            "Final deployment step",
        )),
    )
    .await;
    let (_, test) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "task",
            "Run integration tests",
            "Must pass before deploy",
        )),
    )
    .await;
    let (_, build) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "task",
            "Build artifacts",
            "Compile and package",
        )),
    )
    .await;
    let (_, review) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("task", "Code review", "Peer review required")),
    )
    .await;

    let deploy_id = deploy["id"].as_u64().unwrap();
    let test_id = test["id"].as_u64().unwrap();
    let build_id = build["id"].as_u64().unwrap();
    let review_id = review["id"].as_u64().unwrap();

    // Dependency chain: review -> build -> test -> deploy
    for (from, to) in [
        (deploy_id, test_id),
        (test_id, build_id),
        (build_id, review_id),
    ] {
        let (status, _) = send(
            &app,
            "POST",
            "/edges",
            Some(new_edge_body(from, to, "depends_on")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    // Shortest path from deploy to review reveals full dependency chain
    let (_, resp) = send(
        &app,
        "GET",
        &format!("/paths/shortest?from={deploy_id}&to={review_id}"),
        None,
    )
    .await;
    let path: Vec<u64> = resp["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    assert_eq!(path, vec![deploy_id, test_id, build_id, review_id]);

    // List all tasks via kind index
    let (_, resp) = send(&app, "GET", "/nodes?kind=task&limit=10", None).await;
    assert_eq!(resp["nodes"].as_array().unwrap().len(), 4);

    // Search tasks by title content
    let (_, resp) = send(
        &app,
        "POST",
        "/search/fts",
        Some(json!({ "query": "production", "limit": 5 })),
    )
    .await;
    let results = resp["results"].as_array().unwrap();
    assert!(!results.is_empty());
    let found_ids: Vec<u64> = results
        .iter()
        .map(|r| r["node"]["id"].as_u64().unwrap())
        .collect();
    assert!(
        found_ids.contains(&deploy_id),
        "expected deploy node in FTS results, got {found_ids:?}"
    );
}

// ---------------------------------------------------------------------
// Scenario: story editor graph through HTTP
// ---------------------------------------------------------------------

#[tokio::test]
async fn integration_scenario_story_editor() {
    let app = make_app();

    // Create story structure
    let (_, book) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "book",
            "The Great Adventure",
            "A tale of courage",
        )),
    )
    .await;
    let (_, ch1) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "chapter",
            "Chapter 1: The Beginning",
            "It all started...",
        )),
    )
    .await;
    let (_, ch2) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "chapter",
            "Chapter 2: The Journey",
            "They set off...",
        )),
    )
    .await;
    let (_, hero) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body(
            "character",
            "Hero",
            "The protagonist of the story",
        )),
    )
    .await;

    let book_id = book["id"].as_u64().unwrap();
    let ch1_id = ch1["id"].as_u64().unwrap();
    let ch2_id = ch2["id"].as_u64().unwrap();
    let hero_id = hero["id"].as_u64().unwrap();

    // Build structure edges
    for (from, to, kind) in [
        (book_id, ch1_id, "contains"),
        (book_id, ch2_id, "contains"),
        (ch1_id, ch2_id, "followed_by"),
        (ch1_id, hero_id, "involves"),
        (ch2_id, hero_id, "involves"),
    ] {
        let (status, _) = send(&app, "POST", "/edges", Some(new_edge_body(from, to, kind))).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    // Subgraph from book at depth 2 captures the whole story structure
    let (_, resp) = send(
        &app,
        "GET",
        &format!("/nodes/{book_id}/subgraph?depth=2"),
        None,
    )
    .await;
    let sub_nodes = resp["nodes"].as_array().unwrap();
    assert_eq!(sub_nodes.len(), 4);
    let sub_edges = resp["edges"].as_array().unwrap();
    assert_eq!(sub_edges.len(), 5);

    // Outgoing edges of book are the "contains" edges
    let (_, resp) = send(
        &app,
        "GET",
        &format!("/nodes/{book_id}/edges?direction=outgoing"),
        None,
    )
    .await;
    let edges = resp["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2);
    for e in edges {
        assert_eq!(e["kind"], "contains");
    }

    // Search for a chapter by content
    let (_, resp) = send(
        &app,
        "POST",
        "/search/fts",
        Some(json!({ "query": "journey", "limit": 5 })),
    )
    .await;
    assert!(!resp["results"].as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------
// Cross-endpoint consistency: properties roundtrip
// ---------------------------------------------------------------------

#[tokio::test]
async fn integration_properties_roundtrip_through_http() {
    let app = make_app();

    let body = json!({
        "kind": "task",
        "title": "Task with props",
        "body": "",
        "body_html": "",
        "properties": {
            "priority": "high",
            "estimate_hours": 8,
            "tags": ["backend", "urgent"]
        }
    });

    let (status, node) = send(&app, "POST", "/nodes", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = node["id"].as_u64().unwrap();

    // Read back and verify properties survived the roundtrip
    let (_, fetched) = send(&app, "GET", &format!("/nodes/{id}"), None).await;
    assert_eq!(fetched["properties"]["priority"], "high");
    assert_eq!(fetched["properties"]["estimate_hours"], 8);
    let tags = fetched["properties"]["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 2);
    assert_eq!(tags[0], "backend");
    assert_eq!(tags[1], "urgent");

    // Update properties via patch
    let (status, updated) = send(
        &app,
        "PATCH",
        &format!("/nodes/{id}"),
        Some(json!({
            "properties": {
                "priority": "low",
                "estimate_hours": 2
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["properties"]["priority"], "low");
    assert_eq!(updated["properties"]["estimate_hours"], 2);
}

// ---------------------------------------------------------------------
// Edge properties roundtrip
// ---------------------------------------------------------------------

#[tokio::test]
async fn integration_edge_properties_roundtrip() {
    let app = make_app();
    let (a, b) = create_two_nodes(&app).await;

    let body = json!({
        "from_id": a,
        "to_id": b,
        "kind": "weighted_link",
        "weight": 0.75,
        "properties": {
            "label": "important",
            "confidence": 0.95
        }
    });

    let (status, edge) = send(&app, "POST", "/edges", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);
    let edge_id = edge["id"].as_u64().unwrap();

    // Read back
    let (_, fetched) = send(&app, "GET", &format!("/edges/{edge_id}"), None).await;
    assert!((fetched["weight"].as_f64().unwrap() - 0.75).abs() < 1e-6);
    assert_eq!(fetched["properties"]["label"], "important");
    assert!((fetched["properties"]["confidence"].as_f64().unwrap() - 0.95).abs() < 1e-6);
}

// ---------------------------------------------------------------------
// Admin endpoints in workflow context
// ---------------------------------------------------------------------

#[tokio::test]
async fn integration_health_and_status_during_operations() {
    let app = make_app();

    // Health works before any data operations
    let (status, _) = send(&app, "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);

    // Create some data
    for i in 0..10 {
        let (status, _) = send(
            &app,
            "POST",
            "/nodes",
            Some(new_node_body("note", &format!("Note {i}"), "")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    // Health and status still work after data operations
    let (status, health) = send(&app, "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["status"], "ok");

    let (status, st) = send(&app, "GET", "/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(st["name"], "graphnote-db");
    assert!(st["uptime_seconds"].is_u64());
}

// ---------------------------------------------------------------------
// Multiple kinds: verify kind isolation
// ---------------------------------------------------------------------

#[tokio::test]
async fn integration_kind_isolation() {
    let app = make_app();

    // Create nodes of different kinds
    let (_, _) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "My Note", "")),
    )
    .await;
    let (_, _) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("task", "My Task", "")),
    )
    .await;
    let (_, _) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("bug", "My Bug", "")),
    )
    .await;

    // Each kind list should contain exactly 1
    for kind in ["note", "task", "bug"] {
        let (status, resp) = send(&app, "GET", &format!("/nodes?kind={kind}&limit=10"), None).await;
        assert_eq!(status, StatusCode::OK);
        let nodes = resp["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1, "expected 1 node of kind '{kind}'");
        assert_eq!(nodes[0]["kind"], kind);
    }

    // Non-existent kind returns empty
    let (status, resp) = send(&app, "GET", "/nodes?kind=nonexistent&limit=10", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["nodes"].as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------
// Pagination consistency across operations
// ---------------------------------------------------------------------

#[tokio::test]
async fn integration_pagination_consistency() {
    let app = make_app();

    // Create 10 nodes
    for i in 0..10 {
        let (status, _) = send(
            &app,
            "POST",
            "/nodes",
            Some(new_node_body("page_test", &format!("Page Node {i}"), "")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    // Paginate through all nodes in pages of 3
    let mut all_ids = std::collections::HashSet::new();
    for offset in (0..10).step_by(3) {
        let (status, resp) = send(
            &app,
            "GET",
            &format!("/nodes?kind=page_test&limit=3&offset={offset}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let nodes = resp["nodes"].as_array().unwrap();
        for n in nodes {
            all_ids.insert(n["id"].as_u64().unwrap());
        }
    }
    // All 10 unique nodes should have been seen
    assert_eq!(all_ids.len(), 10);
}

// ---------------------------------------------------------------------
// Error consistency: all error endpoints return JSON with status field
// ---------------------------------------------------------------------

#[tokio::test]
async fn integration_all_error_paths_return_structured_json() {
    let app = make_app();

    // 404 — unknown route
    let (status, val) = send(&app, "GET", "/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(val["error"].is_string());
    assert_eq!(val["status"].as_u64().unwrap(), 404);

    // 404 — missing node
    let (status, val) = send(&app, "GET", "/nodes/99999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(val["error"].is_string());
    assert_eq!(val["status"].as_u64().unwrap(), 404);

    // 404 — missing edge
    let (status, val) = send(&app, "GET", "/edges/99999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(val["error"].is_string());
    assert_eq!(val["status"].as_u64().unwrap(), 404);

    // 400 — missing required params
    let (status, val) = send(&app, "GET", "/nodes", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(val["error"].is_string());
    assert_eq!(val["status"].as_u64().unwrap(), 400);

    // 405 — wrong method
    let (status, val) = send(&app, "PUT", "/health", Some(json!({}))).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(val["error"].is_string());
    assert_eq!(val["status"].as_u64().unwrap(), 405);

    // 409 — duplicate title
    let (_, _) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "UniqueTitle", "")),
    )
    .await;
    let (status, val) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "UniqueTitle", "")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(val["error"].is_string());
    assert_eq!(val["status"].as_u64().unwrap(), 409);
}
