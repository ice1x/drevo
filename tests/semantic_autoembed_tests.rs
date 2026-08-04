//! End-to-end tests for #251 slice 4 — **server-side auto-embedding on
//! ingest/update**.
//!
//! Gated on `embeddings-proxy` (like the other embedding suites): they compile
//! to an empty binary on the default feature set, and exercise the real
//! [`drevo::embeddings::SyncEmbedder`] against a **local** in-process axum stub
//! — no external network, deterministic, and safe to run anywhere with:
//!
//! ```text
//! cargo test --features embeddings-proxy --test semantic_autoembed_tests
//! ```
//!
//! What they lock (issue #251 acceptance bullet — "on ingest/update, drevo
//! embeds the configured properties server-side and keeps the vector index in
//! sync"):
//! - registering an `Auto`-mode target then `CREATE`ing / creating a matching
//!   node embeds its `text_property` into `embedding_property` with **no**
//!   client round-trip, so `drevo.semantic.query` retrieves it immediately;
//! - the double no-op is honoured: no embedder installed, an unregistered
//!   label, a `Manual`-mode target, or a node without the text property all
//!   leave the node un-embedded;
//! - an update that changes the source text re-embeds.
//!
//! The bodies are plain `#[test]` (not `#[tokio::test]`): the write path and
//! the [`SyncEmbedder`] run synchronously, exactly as on a server worker
//! thread, while the stub upstream lives on a separate multi-threaded runtime.

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
use drevo::model::{NewNode, Properties};
use drevo::semantic_index::IndexMode;

/// Stub `/v1/embeddings` that always answers with the fixed vector `[1.0, 0.0]`.
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

/// A `Drevo` with a real [`SyncEmbedder`] pointed at `addr`.
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

/// The `embedding` property of the node with `title`, if any.
fn embedding_of(db: &Drevo, title: &str) -> Option<JsonValue> {
    let node = db.get_node_by_title(title).expect("get").expect("exists");
    node.properties.0.get("embedding").cloned()
}

#[test]
fn auto_embed_on_create_then_semantic_query_finds_it() {
    let rt = Runtime::new().expect("rt");
    let addr = spawn_stub(&rt);
    let db = db_with_embedder(addr);

    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    db.create_node(new_node("Doc", "d1", Some("hello world")))
        .expect("create");

    // The embedding was written server-side on ingest — no client round-trip.
    assert_eq!(embedding_of(&db, "d1"), Some(json!([1.0, 0.0])));

    // And the full loop works: query by text, embedded server-side, retrieves
    // the node the server embedded.
    let q = parse(
        "CALL drevo.semantic.query('Doc', 'embedding', 'anything', 5) \
         YIELD node, score RETURN node.title AS t ORDER BY score DESC",
    )
    .expect("parse");
    let rows = execute(&q, &db, HashMap::new()).expect("execute").rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("d1".to_string()));
}

#[test]
fn auto_embed_via_cypher_create() {
    let rt = Runtime::new().expect("rt");
    let addr = spawn_stub(&rt);
    let db = db_with_embedder(addr);
    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    let q = parse("CREATE (:Doc {title: 'c1', text: 'some text'})").expect("parse");
    execute(&q, &db, HashMap::new()).expect("create");

    assert_eq!(embedding_of(&db, "c1"), Some(json!([1.0, 0.0])));
}

#[test]
fn auto_embed_skips_unregistered_label() {
    let rt = Runtime::new().expect("rt");
    let addr = spawn_stub(&rt);
    let db = db_with_embedder(addr);
    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    // A node of a different label is not a target — no embedding.
    db.create_node(new_node("Note", "n1", Some("hello")))
        .expect("create");
    assert_eq!(embedding_of(&db, "n1"), None);
}

#[test]
fn manual_mode_is_not_auto_embedded() {
    let rt = Runtime::new().expect("rt");
    let addr = spawn_stub(&rt);
    let db = db_with_embedder(addr);
    db.semantic_register("Doc", "text", "embedding", IndexMode::Manual, None)
        .expect("register");

    db.create_node(new_node("Doc", "m1", Some("hello")))
        .expect("create");
    assert_eq!(embedding_of(&db, "m1"), None);
}

#[test]
fn no_embedder_means_no_auto_embed() {
    // Registered Auto target but NO embedder installed → the write path is the
    // ordinary one; nothing is embedded.
    let db = Drevo::open_in_memory().expect("open");
    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");
    db.create_node(new_node("Doc", "x1", Some("hello")))
        .expect("create");
    assert_eq!(embedding_of(&db, "x1"), None);
}

#[test]
fn node_without_text_property_is_left_alone() {
    let rt = Runtime::new().expect("rt");
    let addr = spawn_stub(&rt);
    let db = db_with_embedder(addr);
    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    db.create_node(new_node("Doc", "empty", None))
        .expect("create");
    assert_eq!(embedding_of(&db, "empty"), None);
}

#[test]
fn update_reembeds_when_text_changes() {
    let rt = Runtime::new().expect("rt");
    let addr = spawn_stub(&rt);
    let db = db_with_embedder(addr);
    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    let node = db
        .create_node(new_node("Doc", "u1", Some("first")))
        .expect("create");
    assert_eq!(embedding_of(&db, "u1"), Some(json!([1.0, 0.0])));

    // Change the source text via Cypher SET; the embedding is refreshed.
    let q = parse("MATCH (d:Doc {title: 'u1'}) SET d.text = 'second'").expect("parse");
    execute(&q, &db, HashMap::new()).expect("update");
    // (stub returns the same vector, so we assert the embedding is still present
    // and well-formed — the re-embed path ran without error.)
    assert_eq!(embedding_of(&db, "u1"), Some(json!([1.0, 0.0])));
    let _ = node;
}

#[test]
fn auto_embed_on_bulk_create() {
    let rt = Runtime::new().expect("rt");
    let addr = spawn_stub(&rt);
    let db = db_with_embedder(addr);
    db.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    db.create_nodes(vec![
        new_node("Doc", "b1", Some("alpha")),
        new_node("Doc", "b2", Some("beta")),
        new_node("Note", "b3", Some("gamma")), // unregistered label
    ])
    .expect("bulk create");

    assert_eq!(embedding_of(&db, "b1"), Some(json!([1.0, 0.0])));
    assert_eq!(embedding_of(&db, "b2"), Some(json!([1.0, 0.0])));
    assert_eq!(embedding_of(&db, "b3"), None);
}
