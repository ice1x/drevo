//! Native agent-memory recall over the native engine (issue #533):
//! `CALL drevo.memory.recall(session, query, k)` (BM25),
//! `CALL drevo.memory.recallVector(session, vector, k)` (cosine) and
//! `CALL drevo.memory.getConversation(session, limit)`. The server-side
//! text-embedding variant, `drevo.memory.recallSemantic`, needs an embedder and
//! lives in `tests/semantic_memory_tests.rs` (feature `embeddings-proxy`).
//!
//! These read the `:Message { session, seq, role, text, body }` short-term
//! memory chain the agent-memory MCP (drevo-mcp #15/#16) writes — here seeded
//! with Cypher `CREATE` (with `body = text`, so the full-text index reaches it).
//! Runs through [`NativeService::execute`], which wires the FTS index the recall
//! procedure reuses.

use std::collections::HashMap;

use drevo::cypher::executor::{ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
        .unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

/// Add a message to a session's chain, mirroring the MCP write (`body = text`
/// so it is full-text searchable).
fn add(svc: &NativeService, session: &str, seq: i64, role: &str, text: &str) {
    run_ok(
        svc,
        &format!(
            "CREATE (:Message {{session: '{session}', seq: {seq}, role: '{role}', \
             text: '{text}', body: '{text}'}})"
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
fn recall_returns_only_this_sessions_matches() {
    let svc = NativeService::in_memory();
    add(&svc, "s1", 1, "user", "the zqxalpha deploy note");
    add(&svc, "s1", 2, "assistant", "some unrelated chatter");
    // Same distinctive token, different session — must NOT be recalled for s1.
    add(&svc, "s2", 1, "user", "another zqxalpha thing entirely");

    let res = run_ok(
        &svc,
        "CALL drevo.memory.recall('s1', 'zqxalpha', 5) YIELD node, score \
         RETURN node.text AS text, score ORDER BY score DESC",
    );
    assert_eq!(texts(&res), vec!["the zqxalpha deploy note"]);
    // A BM25 score comes back.
    assert!(matches!(res.rows[0][1], Value::Float(_)));
}

#[test]
fn recall_respects_k() {
    let svc = NativeService::in_memory();
    for seq in 1..=5 {
        add(
            &svc,
            "s",
            seq,
            "user",
            &format!("zqxbeta message number {seq}"),
        );
    }
    let res = run_ok(
        &svc,
        "CALL drevo.memory.recall('s', 'zqxbeta', 2) YIELD node, score RETURN node.text AS text",
    );
    assert_eq!(res.rows.len(), 2, "k caps the result count");
}

#[test]
fn recall_on_an_empty_session_is_empty() {
    let svc = NativeService::in_memory();
    add(&svc, "s1", 1, "user", "zqxgamma present here");
    let res = run_ok(
        &svc,
        "CALL drevo.memory.recall('other', 'zqxgamma', 5) YIELD node RETURN node.text AS text",
    );
    assert!(res.rows.is_empty());
}

#[test]
fn get_conversation_returns_messages_in_chronological_order() {
    let svc = NativeService::in_memory();
    add(&svc, "s3", 1, "user", "first");
    add(&svc, "s3", 2, "assistant", "second");
    add(&svc, "s3", 3, "user", "third");
    // A different session's message must not leak in.
    add(&svc, "other", 1, "user", "elsewhere");

    let res = run_ok(
        &svc,
        "CALL drevo.memory.getConversation('s3', 50) YIELD node \
         RETURN node.seq AS seq, node.text AS text ORDER BY seq",
    );
    assert_eq!(
        res.rows
            .iter()
            .map(|r| match &r[1] {
                Value::String(t) => t.clone(),
                other => panic!("got {other:?}"),
            })
            .collect::<Vec<_>>(),
        vec!["first", "second", "third"]
    );
}

/// Add a message carrying an embedding in `prop` (as the auto-embed write path
/// or a client-side embedder would leave it).
fn add_embedded(svc: &NativeService, session: &str, seq: i64, text: &str, prop: &str, v: [f64; 2]) {
    run_ok(
        svc,
        &format!(
            "CREATE (:Message {{session: '{session}', seq: {seq}, role: 'user', \
             text: '{text}', body: '{text}', {prop}: [{}, {}]}})",
            v[0], v[1]
        ),
    );
}

#[test]
fn recall_vector_ranks_this_sessions_messages_by_cosine() {
    let svc = NativeService::in_memory();
    add_embedded(&svc, "s1", 1, "orthogonal", "embedding", [0.0, 1.0]);
    add_embedded(&svc, "s1", 2, "exact", "embedding", [1.0, 0.0]);
    add_embedded(&svc, "s1", 3, "close", "embedding", [0.8, 0.6]);
    // Identical to the query, but another session — must NOT be recalled.
    add_embedded(&svc, "s2", 1, "foreign", "embedding", [1.0, 0.0]);

    let res = run_ok(
        &svc,
        "CALL drevo.memory.recallVector('s1', [1.0, 0.0], 5) YIELD node, score \
         RETURN node.text AS text, score",
    );
    // Best-first by cosine similarity, only session s1.
    assert_eq!(texts(&res), vec!["exact", "close", "orthogonal"]);
    match res.rows[0][1] {
        Value::Float(s) => assert!((s - 1.0).abs() < 1e-6, "exact match scores 1.0, got {s}"),
        ref other => panic!("expected a float score, got {other:?}"),
    }
}

#[test]
fn recall_vector_respects_k_and_skips_unembedded_messages() {
    let svc = NativeService::in_memory();
    add_embedded(&svc, "s", 1, "a", "embedding", [1.0, 0.0]);
    add_embedded(&svc, "s", 2, "b", "embedding", [0.8, 0.6]);
    add_embedded(&svc, "s", 3, "c", "embedding", [0.0, 1.0]);
    // No embedding at all — skipped, never an error.
    add(&svc, "s", 4, "user", "plain");

    let res = run_ok(
        &svc,
        "CALL drevo.memory.recallVector('s', [1.0, 0.0], 2) YIELD node RETURN node.text AS text",
    );
    assert_eq!(texts(&res), vec!["a", "b"]);

    let all = run_ok(
        &svc,
        "CALL drevo.memory.recallVector('s', [1.0, 0.0], 10) YIELD node RETURN node.text AS text",
    );
    assert_eq!(all.rows.len(), 3, "the un-embedded message is skipped");
}

#[test]
fn recall_vector_uses_the_registered_message_embedding_property() {
    let svc = NativeService::in_memory();
    // A semantic target for :Message stores vectors in `vec`, not `embedding`.
    run_ok(
        &svc,
        "CALL drevo.semantic.register('Message', 'body', 'vec', 'manual') YIELD label RETURN label",
    );
    add_embedded(&svc, "s", 1, "via-registered-prop", "vec", [1.0, 0.0]);

    let res = run_ok(
        &svc,
        "CALL drevo.memory.recallVector('s', [1.0, 0.0], 5) YIELD node RETURN node.text AS text",
    );
    assert_eq!(texts(&res), vec!["via-registered-prop"]);
}

#[test]
fn get_conversation_keeps_the_most_recent_limit() {
    let svc = NativeService::in_memory();
    for seq in 1..=4 {
        add(&svc, "s", seq, "user", &format!("m{seq}"));
    }
    // limit = 2 → the two most recent (seq 3, 4), still chronological.
    let res = run_ok(
        &svc,
        "CALL drevo.memory.getConversation('s', 2) YIELD node \
         RETURN node.seq AS seq, node.text AS text ORDER BY seq",
    );
    let got: Vec<String> = res
        .rows
        .iter()
        .map(|r| match &r[1] {
            Value::String(t) => t.clone(),
            other => panic!("got {other:?}"),
        })
        .collect();
    assert_eq!(got, vec!["m3", "m4"]);
}
