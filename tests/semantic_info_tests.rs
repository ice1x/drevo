//! End-to-end tests for #267 — `drevo.semantic.info()`, embedder capability
//! introspection (model id + vector dimension + upstream + presence).
//!
//! Gated on `embeddings-proxy`; run with:
//!
//! ```text
//! cargo test --features embeddings-proxy --test semantic_info_tests
//! ```
//!
//! What they lock (issue #267 acceptance):
//! - a client can read the active model id and vector dimension via Cypher;
//! - a client can tell whether a server-side embedder is actually installed
//!   (not merely whether the procedure exists);
//! - no secret is exposed (model + dimension + upstream only — the API key is
//!   never part of the upstream field).

#![cfg(feature = "embeddings-proxy")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value as JsonValue};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

use drevo::cypher::executor::{execute, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::embeddings::{EmbeddingsConfig, SyncEmbedder};

/// Stub answering a fixed 3-dimensional embedding, so `info` reports dimension 3.
async fn stub_embed(Json(_body): Json<JsonValue>) -> Json<JsonValue> {
    Json(json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}],
        "model": "text-embedding-3-small",
        "usage": {"total_tokens": 1}
    }))
}

fn spawn_stub(rt: &Runtime) -> SocketAddr {
    rt.block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let router = Router::new().route("/v1/embeddings", post(stub_embed));
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        addr
    })
}

fn info_row(db: &Drevo) -> Vec<Value> {
    let q = parse(
        "CALL drevo.semantic.info() \
         YIELD embedder_present, model, dimension, upstream \
         RETURN embedder_present, model, dimension, upstream",
    )
    .expect("parse");
    let rows = execute(&q, db, HashMap::new()).expect("exec").rows;
    assert_eq!(rows.len(), 1, "info() returns exactly one row");
    rows[0].clone()
}

#[test]
fn info_reports_model_dimension_upstream_when_configured() {
    let rt = Runtime::new().expect("rt");
    let addr = spawn_stub(&rt);
    let upstream = format!("http://{addr}/v1/embeddings");
    let cfg = EmbeddingsConfig {
        upstream: upstream.clone(),
        api_key: Some("sk-secret-should-not-leak".to_string()),
        model: Some("text-embedding-3-small".to_string()),
    };
    let db = Drevo::open_in_memory().expect("open");
    db.set_embedder(Arc::new(SyncEmbedder::from_config(cfg).expect("embedder")));

    let row = info_row(&db);
    assert_eq!(row[0], Value::Bool(true)); // embedder_present
    assert_eq!(row[1], Value::String("text-embedding-3-small".to_string())); // model
    assert_eq!(row[2], Value::Integer(3)); // dimension (probed from the stub)
    assert_eq!(row[3], Value::String(upstream)); // upstream URL

    // The API key must never appear anywhere in the introspection output.
    let rendered = format!("{row:?}");
    assert!(
        !rendered.contains("sk-secret"),
        "API key leaked: {rendered}"
    );
}

#[test]
fn info_reports_absent_when_no_embedder() {
    let db = Drevo::open_in_memory().expect("open");
    let row = info_row(&db);
    assert_eq!(row[0], Value::Bool(false)); // embedder_present
    assert_eq!(row[1], Value::Null); // model
    assert_eq!(row[2], Value::Null); // dimension
    assert_eq!(row[3], Value::Null); // upstream
}

#[test]
fn info_dimension_is_stable_across_calls() {
    // The probe is cached: a second call reports the same dimension.
    let rt = Runtime::new().expect("rt");
    let addr = spawn_stub(&rt);
    let cfg = EmbeddingsConfig {
        upstream: format!("http://{addr}/v1/embeddings"),
        api_key: None,
        model: Some("m".to_string()),
    };
    let db = Drevo::open_in_memory().expect("open");
    db.set_embedder(Arc::new(SyncEmbedder::from_config(cfg).expect("embedder")));

    assert_eq!(info_row(&db)[2], Value::Integer(3));
    assert_eq!(info_row(&db)[2], Value::Integer(3));
}
