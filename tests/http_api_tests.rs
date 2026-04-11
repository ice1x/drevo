//! Integration tests for the HTTP API (tasks 00037, 00038, 00039, 00040, 00041).
//!
//! Covers the scaffold (task 00037: router, state, error mapping),
//! the node CRUD endpoints (task 00038: POST/GET/PATCH/DELETE /nodes
//! plus the list-by-kind query), the edge endpoints (task 00039:
//! POST/GET/DELETE /edges, list-by-kind, and edges-of-node), the
//! traversal endpoints (task 00040: /nodes/{id}/neighbors,
//! /paths/shortest, /nodes/{id}/subgraph), and the full-text search
//! endpoint (task 00041: POST /search/fts).

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
