//! End-to-end tests for `CALL drevo.memory.recallSemantic(session, text, k)`
//! (issue #533, semantic-recall slice) — session-scoped agent-memory recall by
//! **meaning**: the query text is embedded server-side and `:Message` nodes of
//! the session are ranked by cosine similarity to it.
//!
//! Gated on `embeddings-proxy` like [`semantic_query_tests`]: the real
//! [`drevo::embeddings::SyncEmbedder`] talks to a **local** in-process axum stub
//! (no external network, deterministic). Run with:
//!
//! ```text
//! cargo test --features embeddings-proxy --test semantic_memory_tests
//! ```
//!
//! What they lock:
//! - the query **text** reaches the configured upstream and the resulting
//!   vector ranks this session's messages best-first (other sessions excluded);
//! - a paraphrase with no shared token is still recalled — the point of
//!   semantic over BM25 recall (`drevo.memory.recall` would miss it);
//! - with no embedder installed the call fails with a clean engine-capability
//!   error instead of panicking.

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

use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::embeddings::{EmbeddingsConfig, SyncEmbedder};
use drevo::native_service::NativeService;

/// What the stub upstream captured from the last request.
#[derive(Clone, Default)]
struct Captured {
    body: Arc<Mutex<Option<JsonValue>>>,
}

/// Stub `/v1/embeddings`: captures the request and always answers with the
/// fixed direction `[1.0, 0.0]`, so ranking is deterministic.
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

fn db_with_stub(rt: &Runtime, cap: Captured) -> NativeService {
    let addr = spawn_stub(rt, cap);
    let cfg = EmbeddingsConfig {
        upstream: format!("http://{addr}/v1/embeddings"),
        api_key: None,
        model: Some("stub-embed".to_string()),
    };
    let db = NativeService::in_memory();
    assert!(
        db.set_embedder(Arc::new(SyncEmbedder::from_config(cfg).expect("embedder"))),
        "first set installs"
    );
    db
}

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
        .unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

/// A message with a pre-computed embedding (as auto-embed would leave it).
fn add_embedded(svc: &NativeService, session: &str, seq: i64, text: &str, v: [f64; 2]) {
    run_ok(
        svc,
        &format!(
            "CREATE (:Message {{session: '{session}', seq: {seq}, role: 'user', \
             text: '{text}', body: '{text}', embedding: [{}, {}]}})",
            v[0], v[1]
        ),
    );
}

fn texts(res: &ExecResult) -> Vec<String> {
    res.rows
        .iter()
        .map(|r| match &r[0] {
            Value::String(t) => t.clone(),
            other => panic!("expected a text string, got {other:?}"),
        })
        .collect()
}

#[test]
fn recall_semantic_embeds_the_query_and_ranks_the_sessions_messages() {
    let rt = Runtime::new().expect("runtime");
    let cap = Captured::default();
    let db = db_with_stub(&rt, cap.clone());
    add_embedded(&db, "s1", 1, "we shipped the release", [1.0, 0.0]);
    add_embedded(&db, "s1", 2, "lunch plans", [0.0, 1.0]);
    add_embedded(&db, "s2", 1, "other session, same meaning", [1.0, 0.0]);

    let res = run_ok(
        &db,
        "CALL drevo.memory.recallSemantic('s1', 'what did we deploy?', 5) YIELD node, score \
         RETURN node.text AS text, score",
    );
    // Paraphrase: no token overlap with "we shipped the release", yet it ranks
    // first by meaning; the other session never leaks in.
    assert_eq!(texts(&res), vec!["we shipped the release", "lunch plans"]);

    let body = cap
        .body
        .lock()
        .unwrap()
        .clone()
        .expect("captured a request");
    assert_eq!(body["input"], json!(["what did we deploy?"]));
}

#[test]
fn recall_semantic_respects_k() {
    let rt = Runtime::new().expect("runtime");
    let db = db_with_stub(&rt, Captured::default());
    add_embedded(&db, "s", 1, "a", [1.0, 0.0]);
    add_embedded(&db, "s", 2, "b", [0.8, 0.6]);
    add_embedded(&db, "s", 3, "c", [0.0, 1.0]);

    let res = run_ok(
        &db,
        "CALL drevo.memory.recallSemantic('s', 'anything', 1) YIELD node RETURN node.text AS text",
    );
    assert_eq!(texts(&res), vec!["a"]);
}

/// The realistic agent flow: register `:Message` for auto-embedding once, then
/// write turns *without* vectors (as the MCP `add_message` does) — the write
/// path embeds them, and semantic recall finds them with no client-side work.
///
/// The target reads `text`, not `body`: `CREATE` lifts `body`/`title` out of
/// the property map into the node's dedicated fields, which auto-embed does not
/// see, so a `body` target would never fire.
#[test]
fn auto_embedded_messages_are_recalled_semantically() {
    let rt = Runtime::new().expect("runtime");
    let db = db_with_stub(&rt, Captured::default());
    run_ok(
        &db,
        "CALL drevo.semantic.register('Message', 'text', 'embedding', 'auto') \
         YIELD label RETURN label",
    );
    for (session, seq, text) in [
        ("s1", 1, "first turn"),
        ("s1", 2, "second turn"),
        ("s2", 1, "elsewhere"),
    ] {
        run_ok(
            &db,
            &format!(
                "CREATE (:Message {{session: '{session}', seq: {seq}, role: 'user', \
                 text: '{text}', body: '{text}'}})"
            ),
        );
    }

    let res = run_ok(
        &db,
        "CALL drevo.memory.recallSemantic('s1', 'anything', 10) YIELD node \
         RETURN node.text AS text ORDER BY text",
    );
    assert_eq!(texts(&res), vec!["first turn", "second turn"]);
}

/// The whole loop server-side, no client Cypher: turns written with the native
/// `drevo.memory.addMessage` are auto-embedded on write (the procedure goes
/// through the regular CREATE path) and recalled by meaning.
#[test]
fn native_add_message_turns_are_auto_embedded_and_recalled() {
    let rt = Runtime::new().expect("runtime");
    let db = db_with_stub(&rt, Captured::default());
    run_ok(
        &db,
        "CALL drevo.semantic.register('Message', 'text', 'embedding', 'auto') \
         YIELD label RETURN label",
    );
    for (session, text) in [
        ("s1", "deploy went fine"),
        ("s1", "ok thanks"),
        ("s2", "other"),
    ] {
        run_ok(
            &db,
            &format!(
                "CALL drevo.memory.addMessage('{session}', 'user', '{text}') YIELD node \
                 RETURN node"
            ),
        );
    }
    let embedded = run_ok(
        &db,
        "MATCH (m:Message {session: 's1'}) WHERE m.embedding IS NOT NULL RETURN count(m) AS n",
    );
    assert_eq!(
        embedded.rows[0][0],
        Value::Integer(2),
        "both turns embedded on write"
    );

    let res = run_ok(
        &db,
        "CALL drevo.memory.recallSemantic('s1', 'how was the release?', 10) YIELD node \
         RETURN node.text AS text ORDER BY text",
    );
    assert_eq!(texts(&res), vec!["deploy went fine", "ok thanks"]);
}

#[test]
fn recall_semantic_without_embedder_reports_a_capability_error() {
    let db = NativeService::in_memory();
    add_embedded(&db, "s", 1, "a", [1.0, 0.0]);

    let q =
        parse("CALL drevo.memory.recallSemantic('s', 'x', 3) YIELD node, score").expect("parse");
    let err = db.execute(&q, HashMap::new()).expect_err("should error");
    match err {
        ExecError::EngineCapability { feature } => assert_eq!(feature, "semantic embedding"),
        other => panic!("expected EngineCapability, got {other:?}"),
    }
}
