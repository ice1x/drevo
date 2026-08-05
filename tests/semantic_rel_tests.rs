//! End-to-end tests for #266 — server-side auto-embedding for
//! **relationships/edges** (registerRel / auto-embed on rel write / queryRel /
//! reindexRel / status coverage).
//!
//! Gated on `embeddings-proxy`; run with:
//!
//! ```text
//! cargo test --features embeddings-proxy --test semantic_rel_tests
//! ```
//!
//! What they lock (issue #266 acceptance):
//! - a rule registered on a relationship type auto-embeds matching edges on
//!   create/update (fail-open, skip-unchanged), and `queryRel` retrieves them;
//! - `reindexRel` backfills pre-existing edges; `status` lists rel targets with
//!   the health columns and `target_kind = 'relationship'`;
//! - skip rules hold: manual target, no embedder, unregistered target.

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
use drevo::semantic_index::IndexMode;

async fn stub_embed(Json(_body): Json<JsonValue>) -> Json<JsonValue> {
    Json(json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [1.0, 0.0]}],
        "model": "stub",
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
        model: Some("stub".to_string()),
    };
    let db = Drevo::open_in_memory().expect("open");
    db.set_embedder(Arc::new(SyncEmbedder::from_config(cfg).expect("embedder")));
    db
}

fn run(db: &Drevo, src: &str) -> Vec<Vec<Value>> {
    let q = parse(src).expect("parse");
    execute(&q, db, HashMap::new()).expect("execute").rows
}

/// The `fact_embedding` of the single RELATES_TO edge, if present.
fn edge_embedding(db: &Drevo) -> Value {
    let rows = run(
        db,
        "MATCH ()-[r:RELATES_TO]->() RETURN r.fact_embedding AS e",
    );
    assert_eq!(rows.len(), 1, "expected exactly one RELATES_TO edge");
    rows[0][0].clone()
}

/// Create two nodes joined by one RELATES_TO edge carrying `fact`.
fn create_edge(db: &Drevo, fact: &str) {
    run(
        db,
        &format!(
            "CREATE (a:Doc {{title: 'a'}}), (b:Doc {{title: 'b'}}), \
             (a)-[:RELATES_TO {{fact: '{fact}'}}]->(b)"
        ),
    );
}

#[test]
fn rel_auto_embed_on_create_then_query_finds_it() {
    let rt = Runtime::new().expect("rt");
    let db = db_with_embedder(spawn_stub(&rt));
    db.semantic_register_rel(
        "RELATES_TO",
        "fact",
        "fact_embedding",
        IndexMode::Auto,
        None,
    )
    .expect("registerRel");

    create_edge(&db, "Alice mentors Bob");

    // The edge was embedded server-side on ingest.
    assert!(
        matches!(edge_embedding(&db), Value::List(_)),
        "fact_embedding should be populated"
    );

    // …and queryRel retrieves it from query text.
    let rows = run(
        &db,
        "CALL drevo.semantic.queryRel('RELATES_TO', 'fact_embedding', 'who mentors whom', 5) \
         YIELD rel, score RETURN rel",
    );
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Relationship(_)));
}

#[test]
fn rel_register_rel_via_cypher_and_auto_embed() {
    let rt = Runtime::new().expect("rt");
    let db = db_with_embedder(spawn_stub(&rt));
    run(
        &db,
        "CALL drevo.semantic.registerRel('RELATES_TO', 'fact', 'fact_embedding', 'auto') \
         YIELD label RETURN label",
    );
    create_edge(&db, "some fact");
    assert!(matches!(edge_embedding(&db), Value::List(_)));
}

#[test]
fn rel_reindex_backfills_preexisting_edges() {
    let rt = Runtime::new().expect("rt");
    let db = db_with_embedder(spawn_stub(&rt));
    // Edge created BEFORE the rule → not auto-embedded.
    create_edge(&db, "pre-existing fact");
    assert_eq!(edge_embedding(&db), Value::Null);

    db.semantic_register_rel(
        "RELATES_TO",
        "fact",
        "fact_embedding",
        IndexMode::Auto,
        None,
    )
    .expect("registerRel");

    let rows = run(
        &db,
        "CALL drevo.semantic.reindexRel('RELATES_TO', 'fact_embedding', 128) \
         YIELD scanned, embedded, skipped, remaining \
         RETURN scanned, embedded, skipped, remaining",
    );
    assert_eq!(rows[0][0], Value::Integer(1)); // scanned
    assert_eq!(rows[0][1], Value::Integer(1)); // embedded
    assert_eq!(rows[0][3], Value::Integer(0)); // remaining
    assert!(matches!(edge_embedding(&db), Value::List(_)));
}

#[test]
fn rel_status_lists_relationship_target_with_health() {
    let rt = Runtime::new().expect("rt");
    let db = db_with_embedder(spawn_stub(&rt));
    // Pre-existing un-embedded edge → pending backlog once registered.
    create_edge(&db, "fact");
    db.semantic_register_rel(
        "RELATES_TO",
        "fact",
        "fact_embedding",
        IndexMode::Auto,
        None,
    )
    .expect("registerRel");

    let rows = run(
        &db,
        "CALL drevo.semantic.status() \
         YIELD label, target_kind, pending_count, state \
         RETURN label, target_kind, pending_count, state",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("RELATES_TO".to_string()));
    assert_eq!(rows[0][1], Value::String("relationship".to_string()));
    assert_eq!(rows[0][2], Value::Integer(1)); // pending
    assert_eq!(rows[0][3], Value::String("degraded".to_string()));
}

#[test]
fn rel_manual_target_is_not_auto_embedded() {
    let rt = Runtime::new().expect("rt");
    let db = db_with_embedder(spawn_stub(&rt));
    db.semantic_register_rel(
        "RELATES_TO",
        "fact",
        "fact_embedding",
        IndexMode::Manual,
        None,
    )
    .expect("registerRel");
    create_edge(&db, "fact");
    assert_eq!(edge_embedding(&db), Value::Null);
}

#[test]
fn rel_no_embedder_no_auto_embed() {
    let db = Drevo::open_in_memory().expect("open");
    db.semantic_register_rel(
        "RELATES_TO",
        "fact",
        "fact_embedding",
        IndexMode::Auto,
        None,
    )
    .expect("registerRel");
    create_edge(&db, "fact");
    assert_eq!(edge_embedding(&db), Value::Null);
}

#[test]
fn rel_query_rel_unregistered_still_scans_empty() {
    // queryRel doesn't require a registered target (it just scans edges of the
    // type); with no matching edges it returns nothing.
    let rt = Runtime::new().expect("rt");
    let db = db_with_embedder(spawn_stub(&rt));
    let rows = run(
        &db,
        "CALL drevo.semantic.queryRel('RELATES_TO', 'fact_embedding', 'q', 5) \
         YIELD rel, score RETURN rel",
    );
    assert!(rows.is_empty());
}

#[test]
fn rel_reindex_rel_unregistered_errors() {
    let rt = Runtime::new().expect("rt");
    let db = db_with_embedder(spawn_stub(&rt));
    let q = parse(
        "CALL drevo.semantic.reindexRel('RELATES_TO', 'fact_embedding', 128) \
         YIELD scanned RETURN scanned",
    )
    .expect("parse");
    match execute(&q, &db, HashMap::new()).expect_err("should error") {
        ExecError::InvalidProcedureCall { name, message, .. } => {
            assert_eq!(name, "drevo.semantic.reindexRel");
            assert!(
                message.contains("no semantic relationship target registered"),
                "got: {message}"
            );
        }
        other => panic!("expected InvalidProcedureCall, got {other:?}"),
    }
}
