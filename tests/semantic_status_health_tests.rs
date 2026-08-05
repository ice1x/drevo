//! End-to-end tests for #263 — surfacing swallowed auto-embed failures via
//! `drevo.semantic.status` (`pending_count` / `failed_count` / `last_error` /
//! `degraded` state).
//!
//! Gated on `embeddings-proxy`; run with:
//!
//! ```text
//! cargo test --features embeddings-proxy --test semantic_status_health_tests
//! ```
//!
//! What they lock (issue #263 acceptance):
//! - after an embedder outage during ingest, `status` reflects the un-embedded
//!   / failed nodes (`pending_count` > 0, `failed_count` > 0, `last_error` set,
//!   `state` = `degraded`);
//! - a fully-embedded target reads clean (all zeros, non-degraded);
//! - write durability is unchanged — the outage does not fail the write.

#![cfg(feature = "embeddings-proxy")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value as JsonValue};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

use drevo::cypher::executor::{execute, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::embeddings::{EmbeddingsConfig, SyncEmbedder};
use drevo::model::{NewNode, Properties};
use drevo::semantic_index::IndexMode;

/// A healthy stub: fixed embedding `[1.0, 0.0]`.
async fn stub_ok(Json(_body): Json<JsonValue>) -> Json<JsonValue> {
    Json(json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [1.0, 0.0]}],
        "model": "stub",
        "usage": {"total_tokens": 1}
    }))
}

/// A broken upstream: always 500, so every embed attempt fails (and is
/// swallowed by the fail-open write path).
async fn stub_500() -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "upstream boom".to_string(),
    )
}

fn spawn(rt: &Runtime, router: Router) -> SocketAddr {
    rt.block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        addr
    })
}

fn db_at(addr: SocketAddr) -> Drevo {
    let cfg = EmbeddingsConfig {
        upstream: format!("http://{addr}/v1/embeddings"),
        api_key: None,
        model: Some("stub".to_string()),
    };
    let db = Drevo::open_in_memory().expect("open");
    db.set_embedder(Arc::new(SyncEmbedder::from_config(cfg).expect("embedder")));
    db
}

fn doc(title: &str, text: &str) -> NewNode {
    let mut props = HashMap::new();
    props.insert("text".to_string(), json!(text));
    NewNode {
        kind: "Doc".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(props),
    }
}

/// Read the single status row's columns by name.
fn status_row(db: &Drevo) -> HashMap<String, Value> {
    let q = parse(
        "CALL drevo.semantic.status() \
         YIELD label, state, pending_count, failed_count, last_error \
         RETURN label, state, pending_count, failed_count, last_error",
    )
    .expect("parse");
    let rows = execute(&q, db, HashMap::new()).expect("exec").rows;
    assert_eq!(rows.len(), 1, "expected exactly one registered target");
    let cols = [
        "label",
        "state",
        "pending_count",
        "failed_count",
        "last_error",
    ];
    cols.iter()
        .zip(rows[0].iter())
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

#[test]
fn outage_during_ingest_is_surfaced_as_degraded() {
    let rt = Runtime::new().expect("rt");
    let addr = spawn(&rt, Router::new().route("/v1/embeddings", post(stub_500)));
    let db = db_at(addr);
    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    // The write still succeeds (fail-open) even though the embedder is down.
    let node = db
        .create_node(doc("d1", "hello"))
        .expect("write must not fail");
    assert!(
        !node.properties.0.contains_key("embedding"),
        "no embedding was written during the outage"
    );

    // …but the degraded condition is now observable.
    let row = status_row(&db);
    assert_eq!(row["state"], Value::String("degraded".to_string()));
    assert_eq!(row["pending_count"], Value::Integer(1));
    assert_eq!(row["failed_count"], Value::Integer(1));
    match &row["last_error"] {
        Value::String(s) => assert!(!s.is_empty(), "last_error should carry a reason"),
        other => panic!("expected a last_error string, got {other:?}"),
    }
}

#[test]
fn failed_count_accumulates_across_writes() {
    let rt = Runtime::new().expect("rt");
    let addr = spawn(&rt, Router::new().route("/v1/embeddings", post(stub_500)));
    let db = db_at(addr);
    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    db.create_node(doc("d1", "a")).expect("write");
    db.create_node(doc("d2", "b")).expect("write");
    db.create_node(doc("d3", "c")).expect("write");

    let row = status_row(&db);
    assert_eq!(row["pending_count"], Value::Integer(3));
    assert_eq!(row["failed_count"], Value::Integer(3));
    assert_eq!(row["state"], Value::String("degraded".to_string()));
}

#[test]
fn healthy_target_reads_clean() {
    let rt = Runtime::new().expect("rt");
    let addr = spawn(&rt, Router::new().route("/v1/embeddings", post(stub_ok)));
    let db = db_at(addr);
    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    db.create_node(doc("d1", "hello")).expect("write");

    let row = status_row(&db);
    assert_eq!(row["state"], Value::String("enabled".to_string()));
    assert_eq!(row["pending_count"], Value::Integer(0));
    assert_eq!(row["failed_count"], Value::Integer(0));
    assert_eq!(row["last_error"], Value::Null);
}

#[test]
fn preexisting_unembedded_nodes_show_as_pending() {
    // Nodes created before the rule → un-embedded → pending backlog even though
    // no embed was ever attempted (failed_count stays 0).
    let rt = Runtime::new().expect("rt");
    let addr = spawn(&rt, Router::new().route("/v1/embeddings", post(stub_ok)));
    let db = db_at(addr);
    db.create_node(doc("old", "hello")).expect("write");
    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    let row = status_row(&db);
    assert_eq!(row["pending_count"], Value::Integer(1));
    assert_eq!(row["failed_count"], Value::Integer(0));
    assert_eq!(row["state"], Value::String("degraded".to_string()));

    // Draining the backlog clears the degraded state.
    let reindex = parse(
        "CALL drevo.semantic.reindex('Doc', 'embedding', 128) \
         YIELD embedded RETURN embedded",
    )
    .expect("parse");
    execute(&reindex, &db, HashMap::new()).expect("reindex");

    let row = status_row(&db);
    assert_eq!(row["pending_count"], Value::Integer(0));
    assert_eq!(row["state"], Value::String("enabled".to_string()));
}
