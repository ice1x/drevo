//! Native agent-memory recall over the native engine (issue #533):
//! `CALL drevo.memory.recall(session, query, k)` and
//! `CALL drevo.memory.getConversation(session, limit)`.
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
