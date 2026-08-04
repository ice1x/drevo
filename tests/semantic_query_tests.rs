//! End-to-end tests for `CALL drevo.semantic.query(label, property, text, k)`
//! (#251 slice 3) — the server-side text-embedding retrieval procedure.
//!
//! Gated on `embeddings-proxy` (like [`embeddings_proxy_tests`]): they compile
//! to an empty binary on the default feature set, and exercise the real
//! [`drevo::embeddings::SyncEmbedder`] against a **local** in-process axum stub
//! — no external network, deterministic, and safe to run anywhere with:
//!
//! ```text
//! cargo test --features embeddings-proxy --test semantic_query_tests
//! ```
//!
//! What they lock:
//! - the query **text** (not a vector) is embedded server-side and the result
//!   is used to rank `label` nodes by cosine similarity (same scan as
//!   `drevo.vector.query`), returning the top-`k` in descending score order;
//! - the query text is actually forwarded to the configured upstream (captured
//!   by the stub) — proving the bridge from the synchronous executor to the
//!   async embedder works without a runtime-in-runtime panic;
//! - a handle with **no** embedder installed reports a clean "not configured"
//!   error rather than panicking.
//!
//! The test bodies are plain `#[test]` (not `#[tokio::test]`): the executor and
//! the [`SyncEmbedder`] run synchronously on the test thread, exactly as they
//! do on a server worker thread, while the stub upstream lives on a separate
//! multi-threaded runtime kept alive for the test's duration.

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
/// fixed query direction `[1.0, 0.0]`, so ranking is deterministic regardless
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

/// Spawn the stub on `rt` and return its address. The server task runs on the
/// runtime's worker threads and keeps serving as long as `rt` is alive.
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

/// Seed three chunks: `a` is identical to the stub's query direction, `b`
/// close, `c` orthogonal.
fn seed_chunks(db: &Drevo) {
    let q = parse(
        "CREATE (:Chunk {title: 'a', embedding: [1.0, 0.0]}), \
                (:Chunk {title: 'b', embedding: [0.8, 0.6]}), \
                (:Chunk {title: 'c', embedding: [0.0, 1.0]})",
    )
    .expect("parse seed");
    execute(&q, db, HashMap::new()).expect("seed");
}

fn titles(rows: &[Vec<Value>]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected String title, got {other:?}"),
        })
        .collect()
}

#[test]
fn semantic_query_embeds_text_and_ranks_by_cosine() {
    let rt = Runtime::new().expect("runtime");
    let cap = Captured::default();
    let addr = spawn_stub(&rt, cap.clone());

    let cfg = EmbeddingsConfig {
        upstream: format!("http://{addr}/v1/embeddings"),
        api_key: None,
        model: Some("stub-embed".to_string()),
    };
    let embedder = SyncEmbedder::from_config(cfg).expect("build embedder");

    let db = Drevo::open_in_memory().expect("open");
    assert!(db.set_embedder(Arc::new(embedder)), "first set installs");
    seed_chunks(&db);

    let q = parse(
        "CALL drevo.semantic.query('Chunk', 'embedding', 'find alpha please', 3) \
         YIELD node, score RETURN node.title AS t ORDER BY score DESC",
    )
    .expect("parse query");
    let rows = execute(&q, &db, HashMap::new()).expect("execute").rows;

    // Ranked by cosine to the embedded query direction [1.0, 0.0].
    assert_eq!(titles(&rows), vec!["a", "b", "c"]);

    // The query text really reached the upstream (sync → async bridge worked).
    let body = cap
        .body
        .lock()
        .unwrap()
        .clone()
        .expect("captured a request");
    assert_eq!(body["input"], json!(["find alpha please"]));
    assert_eq!(body["model"], "stub-embed");
}

#[test]
fn semantic_query_honours_k_limit() {
    let rt = Runtime::new().expect("runtime");
    let addr = spawn_stub(&rt, Captured::default());
    let cfg = EmbeddingsConfig {
        upstream: format!("http://{addr}/v1/embeddings"),
        api_key: None,
        model: None,
    };
    let db = Drevo::open_in_memory().expect("open");
    db.set_embedder(Arc::new(SyncEmbedder::from_config(cfg).expect("embedder")));
    seed_chunks(&db);

    let q = parse(
        "CALL drevo.semantic.query('Chunk', 'embedding', 'anything', 2) \
         YIELD node, score RETURN node.title AS t ORDER BY score DESC",
    )
    .expect("parse");
    let rows = execute(&q, &db, HashMap::new()).expect("execute").rows;
    assert_eq!(titles(&rows), vec!["a", "b"]);
}

#[test]
fn semantic_query_without_embedder_reports_not_configured() {
    // No upstream spawned, no embedder installed — the procedure must fail
    // cleanly instead of panicking.
    let db = Drevo::open_in_memory().expect("open");
    seed_chunks(&db);

    let q = parse("CALL drevo.semantic.query('Chunk', 'embedding', 'x', 3) YIELD node, score")
        .expect("parse");
    let err = execute(&q, &db, HashMap::new()).expect_err("should error");
    match err {
        ExecError::InvalidProcedureCall { name, message, .. } => {
            assert_eq!(name, "drevo.semantic.query");
            assert!(
                message.contains("not configured"),
                "expected a not-configured message, got: {message}"
            );
        }
        other => panic!("expected InvalidProcedureCall, got {other:?}"),
    }
}
