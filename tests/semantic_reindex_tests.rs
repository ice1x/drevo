//! End-to-end tests for #262 — `drevo.semantic.reindex`, the resumable
//! backfill for nodes that already existed when an `Auto` target was
//! registered.
//!
//! Gated on `embeddings-proxy` (like the other embedding suites); run with:
//!
//! ```text
//! cargo test --features embeddings-proxy --test semantic_reindex_tests
//! ```
//!
//! What they lock (issue #262 acceptance):
//! - after registering a rule against a populated label and calling `reindex`,
//!   `drevo.semantic.query` returns the pre-existing nodes;
//! - the backfill is batched (`batch_size` + `remaining`) and idempotent
//!   (re-running embeds nothing new);
//! - the reported counts let a client confirm completion;
//! - it respects the skip rules — `Manual` target is a no-op, an unregistered
//!   target errors, nodes without text are skipped.

#![cfg(feature = "embeddings-proxy")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value as JsonValue};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

use drevo::cypher::executor::{execute, ExecError, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::embeddings::{EmbeddingsConfig, SyncEmbedder};
use drevo::model::{NewNode, Properties};
use drevo::semantic_index::IndexMode;

async fn stub_embed(Json(_body): Json<JsonValue>) -> Json<JsonValue> {
    Json(json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [1.0, 0.0]}],
        "model": "stub-embed",
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

fn db_with_embedder(addr: SocketAddr) -> Drevo {
    let cfg = EmbeddingsConfig {
        upstream: format!("http://{addr}/v1/embeddings"),
        api_key: None,
        model: Some("stub-embed".to_string()),
    };
    let db = Drevo::open_in_memory().expect("open");
    db.set_embedder(Arc::new(SyncEmbedder::from_config(cfg).expect("embedder")));
    db
}

fn new_node(kind: &str, title: &str, text: Option<&str>) -> NewNode {
    let mut props = HashMap::new();
    if let Some(text) = text {
        props.insert("text".to_string(), json!(text));
    }
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(props),
    }
}

fn embedding_of(db: &Drevo, title: &str) -> Option<JsonValue> {
    let node = db.get_node_by_title(title).expect("get").expect("exists");
    node.properties.0.get("embedding").cloned()
}

/// Run `drevo.semantic.reindex` and return (scanned, embedded, skipped, remaining).
fn reindex(db: &Drevo, label: &str, prop: &str, batch: usize) -> (i64, i64, i64, i64) {
    let src = format!(
        "CALL drevo.semantic.reindex('{label}', '{prop}', {batch}) \
         YIELD scanned, embedded, skipped, remaining \
         RETURN scanned, embedded, skipped, remaining"
    );
    let q = parse(&src).expect("parse");
    let rows = execute(&q, db, HashMap::new()).expect("execute").rows;
    assert_eq!(rows.len(), 1);
    let int = |v: &Value| match v {
        Value::Integer(i) => *i,
        other => panic!("expected Integer, got {other:?}"),
    };
    (
        int(&rows[0][0]),
        int(&rows[0][1]),
        int(&rows[0][2]),
        int(&rows[0][3]),
    )
}

#[test]
fn reindex_backfills_preexisting_nodes_then_query_finds_them() {
    let rt = Runtime::new().expect("rt");
    let db = db_with_embedder(spawn_stub(&rt));

    // Nodes created BEFORE the rule exists → not auto-embedded.
    db.create_node(new_node(
        "Doc",
        "old-1",
        Some("anxious thoughts about work"),
    ))
    .expect("create");
    db.create_node(new_node("Doc", "old-2", Some("deadlines and stress")))
        .expect("create");
    assert_eq!(embedding_of(&db, "old-1"), None);

    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    // Before backfill, semantic.query misses the pre-existing nodes.
    let q = parse(
        "CALL drevo.semantic.query('Doc', 'embedding', 'work stress', 5) \
         YIELD node, score RETURN node.title AS t",
    )
    .expect("parse");
    assert!(execute(&q, &db, HashMap::new())
        .expect("exec")
        .rows
        .is_empty());

    // Backfill.
    let (scanned, embedded, skipped, remaining) = reindex(&db, "Doc", "embedding", 128);
    assert_eq!((scanned, embedded, skipped, remaining), (2, 2, 0, 0));
    assert_eq!(embedding_of(&db, "old-1"), Some(json!([1.0, 0.0])));
    assert_eq!(embedding_of(&db, "old-2"), Some(json!([1.0, 0.0])));

    // Now the pre-existing nodes are retrievable.
    let rows = execute(&q, &db, HashMap::new()).expect("exec").rows;
    assert_eq!(rows.len(), 2);
}

#[test]
fn reindex_is_idempotent() {
    let rt = Runtime::new().expect("rt");
    let db = db_with_embedder(spawn_stub(&rt));
    db.create_node(new_node("Doc", "d1", Some("hello")))
        .expect("create");
    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    assert_eq!(reindex(&db, "Doc", "embedding", 128), (1, 1, 0, 0));
    // Second pass: nothing new, the node is skipped (already embedded).
    assert_eq!(reindex(&db, "Doc", "embedding", 128), (1, 0, 1, 0));
}

#[test]
fn reindex_is_batched_and_resumable() {
    let rt = Runtime::new().expect("rt");
    let db = db_with_embedder(spawn_stub(&rt));
    for i in 0..3 {
        db.create_node(new_node("Doc", &format!("d{i}"), Some("text")))
            .expect("create");
    }
    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    // batch_size 2 → 2 embedded, 1 left.
    let (scanned, embedded, skipped, remaining) = reindex(&db, "Doc", "embedding", 2);
    assert_eq!((scanned, embedded, skipped, remaining), (3, 2, 0, 1));
    // Resume → the last one embeds, none remain.
    let (scanned, embedded, skipped, remaining) = reindex(&db, "Doc", "embedding", 2);
    assert_eq!((scanned, embedded, skipped, remaining), (3, 1, 2, 0));
}

#[test]
fn reindex_skips_nodes_without_text() {
    let rt = Runtime::new().expect("rt");
    let db = db_with_embedder(spawn_stub(&rt));
    db.create_node(new_node("Doc", "with", Some("hi")))
        .expect("create");
    db.create_node(new_node("Doc", "without", None))
        .expect("create");
    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    let (scanned, embedded, skipped, remaining) = reindex(&db, "Doc", "embedding", 128);
    assert_eq!((scanned, embedded, skipped, remaining), (2, 1, 1, 0));
    assert_eq!(embedding_of(&db, "without"), None);
}

#[test]
fn reindex_manual_target_is_a_noop() {
    let rt = Runtime::new().expect("rt");
    let db = db_with_embedder(spawn_stub(&rt));
    db.create_node(new_node("Doc", "m1", Some("hi")))
        .expect("create");
    db.semantic_register("Doc", "text", "embedding", IndexMode::Manual, None)
        .expect("register");

    assert_eq!(reindex(&db, "Doc", "embedding", 128), (0, 0, 0, 0));
    assert_eq!(embedding_of(&db, "m1"), None);
}

#[test]
fn reindex_unregistered_target_errors() {
    let rt = Runtime::new().expect("rt");
    let db = db_with_embedder(spawn_stub(&rt));
    let q = parse(
        "CALL drevo.semantic.reindex('Doc', 'embedding', 128) \
         YIELD scanned RETURN scanned",
    )
    .expect("parse");
    match execute(&q, &db, HashMap::new()).expect_err("should error") {
        ExecError::InvalidProcedureCall { name, message, .. } => {
            assert_eq!(name, "drevo.semantic.reindex");
            assert!(
                message.contains("no semantic target registered"),
                "got: {message}"
            );
        }
        other => panic!("expected InvalidProcedureCall, got {other:?}"),
    }
}

#[test]
fn reindex_without_embedder_reports_backlog() {
    // Registered Auto target but no embedder → nothing embeds, all candidates
    // reported as remaining so a client sees the backlog.
    let db = Drevo::open_in_memory().expect("open");
    db.create_node(new_node("Doc", "d1", Some("hi")))
        .expect("create");
    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    assert_eq!(reindex(&db, "Doc", "embedding", 128), (1, 0, 0, 1));
    assert_eq!(embedding_of(&db, "d1"), None);
}
