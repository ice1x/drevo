//! Integration tests for the HTTP API over the native router
//! (`drevo::native_api::build_native_router`).
//!
//! Covers the read/create surface the native router serves: node create +
//! list-by-kind + pagination (`POST`/`GET /nodes`), edge create + list
//! (`POST`/`GET /edges`), the traversal endpoints (`/nodes/{id}/neighbors`,
//! `/paths/shortest`, `/nodes/{id}/subgraph`), the full-text search endpoint
//! (`POST /search/fts`), faceting (`/facets`), server metadata (`GET /status`),
//! and JSON export/import.
//!
//! REST *mutation* (`PATCH`/`DELETE` on nodes/edges), the multi-database
//! catalog, the JSON `health`/`ready`/error-envelope bodies, and the
//! redb-specific storage introspection were surfaces of the retired KV router;
//! on the native engine mutation is expressed through Cypher (`MATCH … SET` /
//! `DELETE` on `/cypher`), so they are not part of this suite.

#![cfg(feature = "http")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn make_app() -> axum::Router {
    let db = Arc::new(NativeService::in_memory());
    let state = NativeApiState::new(db);
    build_native_router(state)
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

    assert_eq!(value["name"], "drevo");
    assert!(
        value["version"].is_string(),
        "version should be a string, got {value:?}"
    );
    let version = value["version"].as_str().unwrap();
    assert!(!version.is_empty(), "version should not be empty");
    // The reported version must be the build-injected `drevo::VERSION` (the
    // release git tag), NOT a stale `CARGO_PKG_VERSION` (0.0.0 on every
    // deployed build, since the release flow leaves Cargo.toml at 0.0.0).
    assert_eq!(
        version,
        drevo::VERSION,
        "GET / must report drevo::VERSION, not a hardcoded crate version"
    );
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
// Task 00042 — Admin endpoints (GET /status)
// ---------------------------------------------------------------------

#[tokio::test]
async fn get_status_returns_server_metadata() {
    let app = make_app();
    let (status, value) = send(&app, "GET", "/status", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["name"], "drevo");
    assert!(value["version"].is_string());
    let version = value["version"].as_str().unwrap();
    assert!(!version.is_empty());
    assert_eq!(
        version,
        drevo::VERSION,
        "GET /status must report drevo::VERSION (the build-injected release version)"
    );
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
// client would interact with drevo. Each test verifies
// cross-endpoint consistency and data integrity.
// =====================================================================

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

// ---------------------------------------------------------------------
// Edge properties roundtrip
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// Admin endpoints in workflow context
// ---------------------------------------------------------------------

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

// =====================================================================
// Task 00109 — query-string boundary validation
//
// The HTTP API audit (audit/AUDIT-http-api.md, findings F4/F5) requires
// regression coverage for: `limit` cap saturation on /nodes and /edges,
// negative-limit rejection via serde, huge offsets returning empty
// without arithmetic overflow, and `depth=0` semantics on /neighbors.
// =====================================================================

#[tokio::test]
async fn list_nodes_limit_above_cap_is_clamped() {
    let app = make_app();

    // Seed slightly more than MAX_LIST_LIMIT would be expensive, so we
    // assert the simpler invariant: requesting `limit` well above the
    // cap still succeeds (200) and never errors. The handler clamps
    // before reaching the storage layer.
    for i in 0..3 {
        let (status, _) = send(
            &app,
            "POST",
            "/nodes",
            Some(new_node_body("note", &format!("ClampNode-{i}"), "")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let (status, value) = send(&app, "GET", "/nodes?kind=note&limit=9999", None).await;
    assert_eq!(status, StatusCode::OK);
    let nodes = value["nodes"].as_array().expect("nodes array");
    // Three seeded nodes — fewer than the cap; the point is no error.
    assert_eq!(nodes.len(), 3);
}

#[tokio::test]
async fn list_nodes_negative_limit_returns_400() {
    let app = make_app();
    let (status, value) = send(&app, "GET", "/nodes?kind=note&limit=-1", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(value["error"].is_string());
    assert_eq!(value["status"].as_u64().unwrap(), 400);
}

#[tokio::test]
async fn list_edges_limit_above_cap_is_clamped() {
    let app = make_app();
    let (a, b) = create_two_nodes(&app).await;
    let (status, _) = send(
        &app,
        "POST",
        "/edges",
        Some(new_edge_body(a, b, "links_to")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, value) = send(&app, "GET", "/edges?kind=links_to&limit=9999", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["edges"].as_array().expect("edges array").len(), 1);
}

#[tokio::test]
async fn list_nodes_huge_offset_returns_empty() {
    let app = make_app();
    let (_, _) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "OffsetEdge", "")),
    )
    .await;

    let (status, value) = send(
        &app,
        "GET",
        "/nodes?kind=note&limit=10&offset=999999999",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(value["nodes"].as_array().expect("nodes array").is_empty());
}

#[tokio::test]
async fn get_node_neighbors_depth_zero_returns_empty() {
    let app = make_app();
    let (a, b) = create_two_nodes(&app).await;
    let (status, _) = send(
        &app,
        "POST",
        "/edges",
        Some(new_edge_body(a, b, "links_to")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // depth=0 follows the BFS contract verified in
    // src/traversal.rs::bfs_depth_zero_returns_empty: zero hops means
    // an empty neighbor list. Crucially, this must NOT 404 — a missing
    // start node still returns 404 (handler line ~501 in src/api.rs),
    // so callers can distinguish "node has no neighbors at depth 0"
    // from "node doesn't exist".
    let (status, value) = send(&app, "GET", &format!("/nodes/{a}/neighbors?depth=0"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(value["nodes"].as_array().expect("nodes array").is_empty());

    // Same path against a non-existent node must still 404.
    let (status, value) = send(&app, "GET", "/nodes/9999/neighbors?depth=0", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(value["status"].as_u64().unwrap(), 404);
}

// ---------------------------------------------------------------------
// Task 00055 — JSON export / import endpoints
// ---------------------------------------------------------------------

#[tokio::test]
async fn get_export_json_returns_drevo_json_v1_document() {
    let app = make_app();
    // Seed a single node so the dump is non-trivial.
    let (status, _) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "Export Me", "")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let req = Request::builder()
        .method("GET")
        .uri("/export/json")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let ct = response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ct.starts_with("application/json"),
        "unexpected content-type: {ct}"
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["format"], "drevo-json-v1");
    let nodes = body["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["title"], "Export Me");
}

#[tokio::test]
async fn post_import_json_loads_dump_into_empty_db() {
    // Build a source dump via the export endpoint.
    let src_app = make_app();
    send(
        &src_app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "From Source", "")),
    )
    .await;
    let req = Request::builder()
        .method("GET")
        .uri("/export/json")
        .body(Body::empty())
        .unwrap();
    let response = src_app.oneshot(req).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let dump_string = String::from_utf8(bytes.to_vec()).unwrap();

    // Now POST that dump into a fresh app.
    let dst_app = make_app();
    let (status, body) = send(
        &dst_app,
        "POST",
        "/import/json",
        Some(json!({ "dump": dump_string })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["nodes_imported"], 1);
    assert_eq!(body["edges_imported"], 0);
    assert_eq!(body["nodes_skipped"], 0);
    assert_eq!(body["edges_skipped"], 0);

    // Verify via a follow-up GET that the node is queryable.
    let (status, body) = send(&dst_app, "GET", "/nodes/1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "From Source");
}

#[tokio::test]
async fn post_import_json_rejects_unknown_format_with_500() {
    let app = make_app();
    let bad = r#"{"format":"v999","exported_at":0,"next_node_id":1,"next_edge_id":1,"nodes":[],"edges":[]}"#;
    let (status, _) = send(&app, "POST", "/import/json", Some(json!({ "dump": bad }))).await;
    // DumpError::UnsupportedFormat → DrevoError::Io → 500.
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn post_import_json_rejects_missing_body_with_400() {
    let app = make_app();
    let req = Request::builder()
        .method("POST")
        .uri("/import/json")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    // Missing `dump` field → JsonRejection → BadRequest 400.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------
// Task 00056 — GraphML export endpoint
// ---------------------------------------------------------------------

#[tokio::test]
async fn get_export_graphml_returns_graphml_document() {
    let app = make_app();
    // Seed a node so the document has at least one `<node>` element.
    let (status, _) = send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "GraphML Me", "")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let req = Request::builder()
        .method("GET")
        .uri("/export/graphml")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let ct = response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ct.starts_with("application/xml"),
        "unexpected content-type: {ct}"
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let xml = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
    assert!(xml.contains("<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\""));
    assert!(xml.contains("<node id=\"n1\">"));
    assert!(xml.contains("<data key=\"d_title\">GraphML Me</data>"));
    assert!(xml.trim_end().ends_with("</graphml>"));
}

#[tokio::test]
async fn export_graphml_rejects_non_get_methods_with_405() {
    let app = make_app();
    let req = Request::builder()
        .method("POST")
        .uri("/export/graphml")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn get_export_graphml_empty_database_still_well_formed() {
    let app = make_app();
    let req = Request::builder()
        .method("GET")
        .uri("/export/graphml")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let xml = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(xml.contains("<graph id=\"drevo\" edgedefault=\"directed\">"));
    assert_eq!(xml.matches("<node ").count(), 0);
    assert_eq!(xml.matches("<edge ").count(), 0);
}

// ---------------------------------------------------------------------
// Task 00057 — GraphML import endpoint (POST /import/graphml)
// ---------------------------------------------------------------------

/// Fetch the full GraphML export from an app as a `String`.
async fn fetch_graphml(app: &axum::Router) -> String {
    let req = Request::builder()
        .method("GET")
        .uri("/export/graphml")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn post_import_graphml_loads_export_into_empty_db() {
    // Build a source graph and export it as GraphML.
    let src_app = make_app();
    send(
        &src_app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "Graph Source", "some body")),
    )
    .await;
    let xml = fetch_graphml(&src_app).await;

    // Import that document into a fresh app.
    let dst_app = make_app();
    let (status, body) = send(
        &dst_app,
        "POST",
        "/import/graphml",
        Some(json!({ "graphml": xml })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["nodes_imported"], 1);
    assert_eq!(body["edges_imported"], 0);
    assert_eq!(body["nodes_skipped"], 0);

    // The node is queryable with its original id preserved.
    let (status, body) = send(&dst_app, "GET", "/nodes/1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "Graph Source");

    // Re-importing the same document is idempotent (all rows skipped).
    let (status, body) = send(
        &dst_app,
        "POST",
        "/import/graphml",
        Some(json!({ "graphml": xml })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["nodes_imported"], 0);
    assert_eq!(body["nodes_skipped"], 1);
}

#[tokio::test]
async fn post_import_graphml_rejects_malformed_document_with_500() {
    let app = make_app();
    let (status, _) = send(
        &app,
        "POST",
        "/import/graphml",
        Some(json!({ "graphml": "<graphml><graph><node id=" })),
    )
    .await;
    // MalformedGraphml → DrevoError::Io → 500.
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn post_import_graphml_rejects_missing_body_with_400() {
    let app = make_app();
    let req = Request::builder()
        .method("POST")
        .uri("/import/graphml")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    // Missing `graphml` field → JsonRejection → BadRequest 400.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn import_graphml_rejects_non_post_methods_with_405() {
    let app = make_app();
    let req = Request::builder()
        .method("GET")
        .uri("/import/graphml")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// ---------------------------------------------------------------
// Keyword faceting endpoint (task 00133): GET /facets
// ---------------------------------------------------------------

#[tokio::test]
async fn get_facets_requires_kind() {
    let app = make_app();
    let (status, body) = send(&app, "GET", "/facets", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("kind"));
}

#[tokio::test]
async fn get_facets_groups_by_keyword_with_none_collapse() {
    let app = make_app();
    send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "A", "graph traversal")),
    )
    .await;
    send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "B", "graph storage")),
    )
    .await;
    send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "C", "vector search")),
    )
    .await;

    let (status, body) = send(&app, "GET", "/facets?kind=note", None).await;
    assert_eq!(status, StatusCode::OK);
    let facets = body["facets"].as_array().expect("facets array");
    // "graph" appears in two documents, ranked first.
    assert_eq!(facets[0]["facet"], "graph");
    assert_eq!(facets[0]["count"], 2);
}

#[tokio::test]
async fn get_facets_lexical_collapse_folds_variants() {
    let app = make_app();
    send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("entry", "Mon", "anxiety before work")),
    )
    .await;
    send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("entry", "Tue", "lingering anxieties today")),
    )
    .await;

    let (status, body) = send(&app, "GET", "/facets?kind=entry&collapse=lexical", None).await;
    assert_eq!(status, StatusCode::OK);
    let facets = body["facets"].as_array().expect("facets array");
    // anxiety / anxieties tie at one document each, so the representative is
    // the alphabetically-first surface form; locate the facet by membership.
    let theme = facets
        .iter()
        .find(|f| {
            f["members"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m == "anxiety")
        })
        .expect("a facet containing 'anxiety'");
    assert_eq!(theme["count"], 2);
    let members: Vec<&str> = theme["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    assert!(members.contains(&"anxieties"));
    assert!(members.contains(&"anxiety"));
}

#[tokio::test]
async fn get_facets_semantic_collapse_is_rejected_on_http() {
    let app = make_app();
    send(
        &app,
        "POST",
        "/nodes",
        Some(new_node_body("note", "A", "some body")),
    )
    .await;
    let (status, body) = send(&app, "GET", "/facets?kind=note&collapse=semantic", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("embedder"));
}

#[tokio::test]
async fn get_facets_unknown_collapse_mode_is_rejected() {
    let app = make_app();
    let (status, body) = send(&app, "GET", "/facets?kind=note&collapse=bogus", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("collapse"));
}

// ── Phase 15 task 00093 — Web UI kinetics served over HTTP ───────────────

/// Fetch a `/ui` asset over the real router and return `(status,
/// content_type, body_text)`. The shared `send` helper assumes a JSON
/// body, but the UI assets are HTML / JS / CSS, so this fetches raw text.
async fn fetch_text(app: &axum::Router, uri: &str) -> (StatusCode, String, String) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("build request");
    let response = app.clone().oneshot(req).await.expect("router response");
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

#[tokio::test]
async fn ui_index_serves_fcose_kinetics_to_client() {
    let app = make_app();
    let (status, content_type, body) = fetch_text(&app, "/ui").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/html"));
    // The kinetics extensions reach the browser, vendored same-origin
    // (not a CDN — see src/web_ui.rs / PR #189).
    assert!(body.contains("cytoscape-fcose"));
    assert!(body.contains("/ui/vendor/cytoscape-fcose.js"));
    assert!(body.contains("id=\"cy-tooltip\""));
}

#[tokio::test]
async fn ui_app_js_serves_kinetics_behaviours_to_client() {
    let app = make_app();
    let (status, content_type, body) = fetch_text(&app, "/ui/app.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("javascript"));
    // fcose layout + double-click expansion + dynamic colour + tooltips
    // all reach the client in one bundle.
    assert!(body.contains("name: \"fcose\""));
    assert!(body.contains("expandNode"));
    assert!(body.contains("subgraph?depth=1"));
    assert!(body.contains("colorForKind"));
    assert!(body.contains("cy-tooltip"));
    assert!(!body.contains("concentric"));
}

#[tokio::test]
async fn ui_styles_css_styles_tooltip_for_client() {
    let app = make_app();
    let (status, content_type, body) = fetch_text(&app, "/ui/styles.css").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/css"));
    assert!(body.contains("#cy-tooltip"));
}

#[tokio::test]
async fn ui_graph_math_module_serves_to_client() {
    // The pure geometry helpers app.js relies on for the live drag spring are
    // served same-origin and are unit-tested by graph_math.test.js (node).
    let app = make_app();
    let (status, content_type, body) = fetch_text(&app, "/ui/graph_math.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("javascript"));
    assert!(body.contains("meanEdgeLength"));
    assert!(body.contains("DrevoGraphMath"));
}

// ── #253 slice 1 — storage-bloat observability ──────────────────────────

#[tokio::test]
async fn storage_bloat_endpoint_rejects_post() {
    let app = make_app();
    let (status, _) = send(&app, "POST", "/storage/bloat", None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

// ── Storage UI panel — per-keyspace breakdown ───────────────────────────

#[tokio::test]
async fn storage_keyspaces_endpoint_rejects_post() {
    let app = make_app();
    let (status, _) = send(&app, "POST", "/storage/keyspaces", None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn storage_shrink_endpoint_rejects_get() {
    let app = make_app();
    let (status, _) = send(&app, "GET", "/storage/shrink", None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn storage_benchmark_endpoint_returns_metrics() {
    // The benchmark runs write throughput on a THROWAWAY in-memory database
    // (never the live graph) and FTS latency read-only against the target db.
    let app = make_app();
    let (status, body) = send(&app, "POST", "/storage/benchmark", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["incr_write_nodes_per_sec"].as_f64().unwrap_or(0.0) > 0.0,
        "incremental write throughput must be measured, got {body:?}"
    );
    assert!(
        body["batch_write_nodes_per_sec"].as_f64().unwrap_or(0.0) > 0.0,
        "batch write throughput must be measured"
    );
    assert!(
        body["search_median_ms"].is_number(),
        "search latency must be reported"
    );
    assert!(body["incr_n"].as_u64().unwrap_or(0) > 0);
}

#[tokio::test]
async fn storage_benchmark_endpoint_rejects_get() {
    let app = make_app();
    let (status, _) = send(&app, "GET", "/storage/benchmark", None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn metrics_endpoint_exposes_storage_file_bytes_gauge() {
    let app = make_app();
    let (status, _content_type, body) = fetch_text(&app, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    // The gauge is registered and rendered (0 for the in-memory backend).
    assert!(
        body.contains("drevo_storage_file_bytes"),
        "metrics output missing storage gauge:\n{body}"
    );
}

// ---------------------------------------------------------------------------
// Multi-database catalog lifecycle (issue #523) — `GET`/`POST /databases`,
// `DELETE /databases/{name}`. Query routing to a non-default database is a
// follow-up slice; here we exercise create / list / drop and their errors.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn databases_lifecycle_create_list_drop() {
    let app = make_app();

    // A fresh server lists only the default database.
    let (status, dbs) = send(&app, "GET", "/databases", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dbs["databases"], json!(["drevo"]));
    assert_eq!(dbs["default"], "drevo");

    // Create a second database — 201 with the updated, sorted list.
    let (status, created) = send(
        &app,
        "POST",
        "/databases",
        Some(json!({ "name": "analytics" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["databases"], json!(["analytics", "drevo"]));

    // It shows up in a subsequent listing.
    let (status, dbs) = send(&app, "GET", "/databases", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dbs["databases"], json!(["analytics", "drevo"]));

    // Drop it — 200 with the list back to just the default.
    let (status, after) = send(&app, "DELETE", "/databases/analytics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after["databases"], json!(["drevo"]));
}

#[tokio::test]
async fn create_database_rejects_invalid_name() {
    let app = make_app();
    let (status, _) = send(
        &app,
        "POST",
        "/databases",
        Some(json!({ "name": "has space" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Nothing was added.
    let (_, dbs) = send(&app, "GET", "/databases", None).await;
    assert_eq!(dbs["databases"], json!(["drevo"]));
}

#[tokio::test]
async fn create_duplicate_database_is_conflict() {
    let app = make_app();
    let (status, _) = send(&app, "POST", "/databases", Some(json!({ "name": "dup" }))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = send(&app, "POST", "/databases", Some(json!({ "name": "dup" }))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // Re-creating the default name conflicts too.
    let (status, _) = send(&app, "POST", "/databases", Some(json!({ "name": "drevo" }))).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn drop_default_database_is_conflict() {
    let app = make_app();
    let (status, _) = send(&app, "DELETE", "/databases/drevo", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // The default survives.
    let (_, dbs) = send(&app, "GET", "/databases", None).await;
    assert_eq!(dbs["databases"], json!(["drevo"]));
}

#[tokio::test]
async fn drop_unknown_database_is_not_found() {
    let app = make_app();
    let (status, _) = send(&app, "DELETE", "/databases/ghost", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
