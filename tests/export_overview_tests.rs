//! `GET /export/overview` - what the Web UI's graph overview draws (#846):
//! every node and relationship with its kind, title and small property
//! values. Large values (embeddings, long texts) are left out and named, so
//! the browser does not download the whole graph dump to draw a sample.

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
    build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())))
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let req = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(value) => req
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn node(app: &axum::Router, kind: &str, title: &str, body: &str, properties: Value) -> u64 {
    let (status, text) = send(
        app,
        "POST",
        "/nodes",
        Some(json!({"kind": kind, "title": title, "body": body, "body_html": "", "properties": properties})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{text}");
    serde_json::from_str::<Value>(&text).unwrap()["id"]
        .as_u64()
        .unwrap()
}

async fn edge(app: &axum::Router, from: u64, to: u64, kind: &str, properties: Value) -> u64 {
    let (status, text) = send(
        app,
        "POST",
        "/edges",
        Some(json!({"from_id": from, "to_id": to, "kind": kind, "weight": 1.0, "properties": properties})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{text}");
    serde_json::from_str::<Value>(&text).unwrap()["id"]
        .as_u64()
        .unwrap()
}

async fn overview(app: &axum::Router) -> (String, Value) {
    let (status, text) = send(app, "GET", "/export/overview", None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let value = serde_json::from_str(&text).expect("the overview is JSON");
    (text, value)
}

fn embedding() -> Value {
    Value::Array((0..1536).map(|i| json!(i as f64 / 1536.0)).collect())
}

#[tokio::test]
async fn an_empty_graph_has_an_empty_overview() {
    let app = make_app();
    let (_, value) = overview(&app).await;
    assert_eq!(value, json!({"nodes": [], "edges": []}));
}

#[tokio::test]
async fn the_overview_lists_every_node_and_relationship() {
    let app = make_app();
    let ada = node(&app, "Person", "Ada", "", json!({})).await;
    let bob = node(&app, "Person", "Bob", "", json!({})).await;
    let acme = node(&app, "Company", "Acme", "", json!({})).await;
    let knows = edge(&app, ada, bob, "KNOWS", json!({})).await;
    let works = edge(&app, ada, acme, "WORKS_AT", json!({})).await;

    let (_, value) = overview(&app).await;
    let nodes = value["nodes"].as_array().unwrap();
    assert_eq!(
        nodes
            .iter()
            .map(|n| (
                n["id"].as_u64().unwrap(),
                n["kind"].as_str().unwrap(),
                n["title"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        vec![
            (ada, "Person", "Ada"),
            (bob, "Person", "Bob"),
            (acme, "Company", "Acme")
        ],
        "nodes come in id order with their kind and title"
    );
    let edges = value["edges"].as_array().unwrap();
    assert_eq!(
        edges
            .iter()
            .map(|e| (
                e["id"].as_u64().unwrap(),
                e["from_id"].as_u64().unwrap(),
                e["to_id"].as_u64().unwrap(),
                e["kind"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        vec![(knows, ada, bob, "KNOWS"), (works, ada, acme, "WORKS_AT")]
    );
}

#[tokio::test]
async fn small_property_values_are_kept_as_they_are() {
    let app = make_app();
    node(
        &app,
        "Person",
        "Ada",
        "",
        json!({"name": "Ada", "age": 36, "active": true, "ratio": 0.5, "tags": ["a", "b"], "note": null}),
    )
    .await;
    let (_, value) = overview(&app).await;
    let n = &value["nodes"][0];
    assert_eq!(
        n["properties"],
        json!({"name": "Ada", "age": 36, "active": true, "ratio": 0.5, "tags": ["a", "b"], "note": null})
    );
    assert!(n.get("omitted").is_none(), "nothing was left out: {n}");
}

#[tokio::test]
async fn large_values_are_left_out_and_named() {
    let app = make_app();
    let long = "x".repeat(2000);
    node(
        &app,
        "Entity",
        "drevo",
        &"body ".repeat(1000),
        json!({"name": "drevo", "embedding": embedding(), "observations": long, "status": "done"}),
    )
    .await;
    let (text, value) = overview(&app).await;
    let n = &value["nodes"][0];
    assert_eq!(n["properties"], json!({"name": "drevo", "status": "done"}));
    assert_eq!(
        n["omitted"],
        json!(["embedding", "observations"]),
        "left-out keys, sorted"
    );
    assert!(
        n.get("body").is_none() && n.get("body_html").is_none(),
        "the body is not part of the overview"
    );
    assert!(
        !text.contains("xxxxxxxx"),
        "the long text must not be in the response"
    );
}

#[tokio::test]
async fn a_value_stays_up_to_256_bytes_of_json() {
    let app = make_app();
    // A string of 254 characters is 256 bytes of JSON with its quotes.
    node(
        &app,
        "T",
        "t",
        "",
        json!({"fits": "a".repeat(254), "too_long": "b".repeat(255)}),
    )
    .await;
    let (_, value) = overview(&app).await;
    let n = &value["nodes"][0];
    assert_eq!(n["properties"], json!({"fits": "a".repeat(254)}));
    assert_eq!(n["omitted"], json!(["too_long"]));
}

#[tokio::test]
async fn relationship_properties_follow_the_same_rule() {
    let app = make_app();
    let a = node(&app, "T", "a", "", json!({})).await;
    let b = node(&app, "T", "b", "", json!({})).await;
    edge(
        &app,
        a,
        b,
        "REL",
        json!({"since": 2020, "embedding": embedding()}),
    )
    .await;
    let (_, value) = overview(&app).await;
    let e = &value["edges"][0];
    assert_eq!(e["properties"], json!({"since": 2020}));
    assert_eq!(e["omitted"], json!(["embedding"]));
    assert_eq!(e["weight"], json!(1.0));
}

#[tokio::test]
async fn a_node_with_an_embedding_costs_little_and_the_json_is_compact() {
    let app = make_app();
    for i in 0..40 {
        node(
            &app,
            "Entity",
            &format!("node {i}"),
            &"text ".repeat(200),
            json!({"name": format!("node {i}"), "embedding": embedding()}),
        )
        .await;
    }
    let (text, value) = overview(&app).await;
    assert_eq!(value["nodes"].as_array().unwrap().len(), 40);
    assert!(
        text.len() < 40 * 200,
        "the overview of 40 nodes is {} bytes",
        text.len()
    );
    assert!(!text.contains('\n'), "the overview is compact JSON");
    let (_, dump) = send(&app, "GET", "/export/json", None).await;
    assert!(
        dump.len() > 100 * text.len(),
        "the full dump ({} bytes) dwarfs the overview ({} bytes)",
        dump.len(),
        text.len()
    );
}

#[tokio::test]
async fn the_overview_is_served_as_json() {
    let app = make_app();
    let req = Request::builder()
        .uri("/export/overview")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_type.starts_with("application/json"),
        "got {content_type}"
    );
}
