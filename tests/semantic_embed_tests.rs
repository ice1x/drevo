//! End-to-end tests for `CALL drevo.semantic.embed(text) YIELD vector` (#272)
//! — the standalone server-side embedding procedure.
//!
//! Unlike `drevo.semantic.query` (which fuses embed + an *unfiltered*
//! brute-force scan), `embed` hands the query vector back so the client runs
//! its **own filtered** Cypher with `cosine_similarity` (#202). This unblocks
//! filter/group-scoped semantic retrieval without a client-side embedder
//! (downstream: graphiti#20).
//!
//! Gated on `embeddings-proxy` (like [`semantic_query_tests`]): they exercise
//! the real [`drevo::embeddings::SyncEmbedder`] against a local in-process axum
//! stub — no external network, deterministic. Run with:
//!
//! ```text
//! cargo test --features embeddings-proxy --test semantic_embed_tests
//! ```

#![cfg(feature = "embeddings-proxy")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value as JsonValue};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

use drevo::cypher::executor::{execute, ExecError, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::embeddings::{EmbeddingsConfig, SyncEmbedder};

/// What the stub upstream captured from the last request, for assertions.
#[derive(Clone, Default)]
struct Captured {
    body: Arc<Mutex<Option<JsonValue>>>,
}

/// Stub `/v1/embeddings`: captures the request body and always answers with the
/// fixed query direction `[1.0, 0.0]`, so results are deterministic regardless
/// of the (arbitrary) query text.
async fn stub_embed(State(cap): State<Captured>, Json(body): Json<JsonValue>) -> Json<JsonValue> {
    *cap.body.lock().unwrap() = Some(body);
    Json(json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [1.0, 0.0]}],
        "model": "stub-embed",
        "usage": {"total_tokens": 1}
    }))
}

fn spawn_stub(rt: &Runtime, cap: Captured) -> SocketAddr {
    rt.block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let router = Router::new()
            .route("/v1/embeddings", post(stub_embed))
            .with_state(cap);
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        addr
    })
}

fn embedder_for(addr: SocketAddr) -> SyncEmbedder {
    let cfg = EmbeddingsConfig {
        upstream: format!("http://{addr}/v1/embeddings"),
        api_key: None,
        model: Some("stub-embed".to_string()),
    };
    SyncEmbedder::from_config(cfg).expect("build embedder")
}

/// The `embed` call returns exactly one row with a single `vector` column that
/// holds the upstream embedding as a Cypher list of floats.
#[test]
fn semantic_embed_returns_query_vector() {
    let rt = Runtime::new().expect("runtime");
    let cap = Captured::default();
    let addr = spawn_stub(&rt, cap.clone());

    let db = Drevo::open_in_memory().expect("open");
    assert!(db.set_embedder(Arc::new(embedder_for(addr))), "installs");

    let q = parse(
        "CALL drevo.semantic.embed('anxious thoughts about work') YIELD vector RETURN vector",
    )
    .expect("parse");
    let rows = execute(&q, &db, HashMap::new()).expect("execute").rows;

    assert_eq!(rows.len(), 1, "one row");
    match &rows[0][0] {
        Value::List(items) => {
            let got: Vec<f64> = items
                .iter()
                .map(|v| match v {
                    Value::Float(f) => *f,
                    Value::Integer(i) => *i as f64,
                    other => panic!("expected numeric vector element, got {other:?}"),
                })
                .collect();
            assert_eq!(got, vec![1.0, 0.0], "the upstream embedding, as a list");
        }
        other => panic!("expected a List vector, got {other:?}"),
    }

    // The query text really reached the upstream (sync -> async bridge worked).
    let body = cap
        .body
        .lock()
        .unwrap()
        .clone()
        .expect("captured a request");
    assert_eq!(body["input"], json!(["anxious thoughts about work"]));
}

/// The acceptance criterion from #272: `embed` composes with a client's own
/// **filtered** Cypher via `cosine_similarity`, ranking only the rows that pass
/// the predicate — the thing `semantic.query` cannot do because it drops filters.
#[test]
fn semantic_embed_feeds_filtered_cosine_similarity() {
    let rt = Runtime::new().expect("runtime");
    let addr = spawn_stub(&rt, Captured::default());
    let db = Drevo::open_in_memory().expect("open");
    db.set_embedder(Arc::new(embedder_for(addr)));

    // Two tenants; `a`/`b`/`c` are the usual near/close/orthogonal directions.
    let seed = parse(
        "CREATE (:Chunk {title: 'a', group_id: 'g1', embedding: [1.0, 0.0]}), \
                (:Chunk {title: 'b', group_id: 'g1', embedding: [0.8, 0.6]}), \
                (:Chunk {title: 'c', group_id: 'g1', embedding: [0.0, 1.0]}), \
                (:Chunk {title: 'other', group_id: 'g2', embedding: [1.0, 0.0]})",
    )
    .expect("parse seed");
    execute(&seed, &db, HashMap::new()).expect("seed");

    // Embed server-side, then rank ONLY tenant g1 by cosine to that vector.
    let q = parse(
        "CALL drevo.semantic.embed('anything') YIELD vector \
         WITH vector \
         MATCH (n:Chunk) WHERE n.group_id = 'g1' AND n.embedding IS NOT NULL \
         WITH n, cosine_similarity(n.embedding, vector) AS score \
         RETURN n.title AS t ORDER BY score DESC",
    )
    .expect("parse");
    let rows = execute(&q, &db, HashMap::new()).expect("execute").rows;

    let titles: Vec<String> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected String title, got {other:?}"),
        })
        .collect();
    // g2's 'other' is filtered out even though it is a perfect match; g1 ranked.
    assert_eq!(titles, vec!["a", "b", "c"]);
}

/// With no embedder installed the call fails cleanly (mirrors `semantic.query`),
/// so a client can catch it and fall back to an external embedder.
#[test]
fn semantic_embed_without_embedder_reports_not_configured() {
    let db = Drevo::open_in_memory().expect("open");
    let q = parse("CALL drevo.semantic.embed('x') YIELD vector RETURN vector").expect("parse");
    let err = execute(&q, &db, HashMap::new()).expect_err("should error");
    match err {
        ExecError::InvalidProcedureCall { name, message, .. } => {
            assert_eq!(name, "drevo.semantic.embed");
            assert!(
                message.contains("not configured"),
                "expected a not-configured message, got: {message}"
            );
        }
        other => panic!("expected InvalidProcedureCall, got {other:?}"),
    }
}
