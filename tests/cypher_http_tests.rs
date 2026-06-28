//! Integration tests for the HTTP Cypher endpoint (`POST /cypher`).
//!
//! drevo's Cypher executor was reachable only over Bolt; the Web UI had no
//! way to run a query. This endpoint exposes the same `cypher::executor` over
//! HTTP so the browser (and any HTTP client) can run Cypher and get back both
//! a tabular result (`columns` + `rows`) and a `graph` projection (the Node /
//! Relationship / Path values, deduped) the UI renders on the canvas.
//!
//! Drives the real `axum::Router` via `tower::ServiceExt::oneshot`, the same
//! in-process pattern as `tests/http_api_tests.rs` / `tests/web_ui_tests.rs`.

#![cfg(feature = "http")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::api::{build_router, ApiState};
use drevo::db::Drevo;
use drevo::model::{NewEdge, NewNode, Properties};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

/// App over an in-memory db pre-populated with `(a:Person)-[:KNOWS]->(b:Person)`.
fn make_populated_app() -> axum::Router {
    let db = Arc::new(Drevo::open_in_memory().expect("open in-memory db"));
    let a = db.create_node(node("Person", "Alice")).expect("a");
    let b = db.create_node(node("Person", "Bob")).expect("b");
    db.create_edge(NewEdge {
        from_id: a.id,
        to_id: b.id,
        kind: "KNOWS".to_string(),
        weight: 1.0,
        properties: Properties::default(),
    })
    .expect("edge");
    build_router(ApiState::new(db))
}

async fn post_cypher(app: &axum::Router, query: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/cypher")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "query": query }).to_string()))
        .expect("build request");
    let resp = app.clone().oneshot(req).await.expect("router response");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes()
        .to_vec();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn cypher_returns_scalar_rows() {
    let app = make_populated_app();
    let (status, body) = post_cypher(&app, "RETURN 1 AS x").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["columns"], json!(["x"]));
    assert_eq!(body["rows"], json!([[1]]));
}

#[tokio::test]
async fn cypher_projects_matched_nodes_into_graph() {
    let app = make_populated_app();
    let (status, body) = post_cypher(&app, "MATCH (n) RETURN n").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["columns"], json!(["n"]));
    let nodes = body["graph"]["nodes"]
        .as_array()
        .expect("graph.nodes array");
    assert_eq!(nodes.len(), 2, "both nodes must reach the graph projection");
    // Each projected node carries the fields the UI's renderer reads.
    for n in nodes {
        assert!(n["id"].is_number());
        assert!(n["kind"].is_string());
        assert!(n.get("properties").is_some());
    }
}

#[tokio::test]
async fn cypher_projects_relationships_into_graph_edges() {
    let app = make_populated_app();
    let (status, body) = post_cypher(&app, "MATCH (a)-[r]->(b) RETURN a, r, b").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let edges = body["graph"]["edges"]
        .as_array()
        .expect("graph.edges array");
    assert_eq!(
        edges.len(),
        1,
        "the KNOWS edge must reach the graph projection"
    );
    assert_eq!(edges[0]["kind"], json!("KNOWS"));
    assert!(edges[0]["from_id"].is_number() && edges[0]["to_id"].is_number());
}

#[tokio::test]
async fn cypher_parse_error_is_400() {
    let app = make_populated_app();
    let (status, _body) = post_cypher(&app, "MATCH (n RETURN n").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a malformed query must be a 400, not a 500"
    );
}

#[tokio::test]
async fn cypher_reports_write_stats() {
    let app = make_populated_app();
    let (status, body) = post_cypher(&app, "CREATE (:Note {title: 'fresh'})").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["stats"]["nodes_created"], json!(1));
}
