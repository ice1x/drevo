//! Native agent memory over the native engine (issue #533).
//!
//! Reads: `CALL drevo.memory.recall(session, query, k)` (BM25),
//! `CALL drevo.memory.recallVector(session, vector, k)` (cosine) and
//! `CALL drevo.memory.getConversation(session, limit)`. The server-side
//! text-embedding variant, `drevo.memory.recallSemantic`, needs an embedder and
//! lives in `tests/semantic_memory_tests.rs` (feature `embeddings-proxy`).
//!
//! Writes: `CALL drevo.memory.addMessage(session, role, text)` and
//! `CALL drevo.memory.recordReasoning(session, step, tool, outcome)` build the
//! same graph the agent-memory MCP (drevo-mcp #15/#16) writes with Cypher — a
//! `:Message { id, session, seq, role, text, body, created_at }` chain linked by
//! `:NEXT`, and `:ReasoningTrace` nodes `:INITIATED_BY` the latest message.
//!
//! The read tests seed with Cypher `CREATE` (with `body = text`, so the
//! full-text index reaches it). Runs through [`NativeService::execute`], which
//! wires the FTS index the recall procedure reuses.

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

// ----- writes: drevo.memory.addMessage / drevo.memory.recordReasoning -------

fn add_msg(svc: &NativeService, session: &str, role: &str, text: &str) -> ExecResult {
    run_ok(
        svc,
        &format!(
            "CALL drevo.memory.addMessage('{session}', '{role}', '{text}') YIELD node \
             RETURN node.seq AS seq, node.role AS role, node.text AS text, node.body AS body, \
             node.id IS NOT NULL AS has_id, node.created_at IS NOT NULL AS has_ts"
        ),
    )
}

fn ints(res: &ExecResult, col: usize) -> Vec<i64> {
    res.rows
        .iter()
        .map(|r| match &r[col] {
            Value::Integer(i) => *i,
            other => panic!("expected an integer, got {other:?}"),
        })
        .collect()
}

#[test]
fn add_message_yields_the_stored_message() {
    let svc = NativeService::in_memory();
    let res = add_msg(&svc, "s", "user", "hello there");
    assert_eq!(res.rows.len(), 1);
    let row = &res.rows[0];
    assert_eq!(
        row[0],
        Value::Integer(1),
        "first message of a session is seq 1"
    );
    assert_eq!(row[1], Value::String("user".into()));
    assert_eq!(row[2], Value::String("hello there".into()));
    // `text` is mirrored into `body` so the full-text index reaches it.
    assert_eq!(row[3], Value::String("hello there".into()));
    assert_eq!(row[4], Value::Bool(true), "has an id");
    assert_eq!(row[5], Value::Bool(true), "has a created_at timestamp");
    assert_eq!(res.stats.nodes_created, 1);
    assert_eq!(
        res.stats.relationships_created, 0,
        "no predecessor, no :NEXT"
    );
}

#[test]
fn add_message_chains_the_session_with_seq_and_next() {
    let svc = NativeService::in_memory();
    add_msg(&svc, "s", "user", "one");
    let second = add_msg(&svc, "s", "assistant", "two");
    assert_eq!(second.stats.nodes_created, 1);
    assert_eq!(
        second.stats.relationships_created, 1,
        "linked to its predecessor"
    );
    add_msg(&svc, "s", "user", "three");

    let chain = run_ok(
        &svc,
        "MATCH (a:Message {session: 's'})-[:NEXT]->(b:Message) \
         RETURN a.seq AS from, b.seq AS to ORDER BY from",
    );
    assert_eq!(ints(&chain, 0), vec![1, 2]);
    assert_eq!(ints(&chain, 1), vec![2, 3]);

    let convo = run_ok(
        &svc,
        "CALL drevo.memory.getConversation('s', 10) YIELD node RETURN node.text AS text",
    );
    assert_eq!(texts(&convo), vec!["one", "two", "three"]);
}

#[test]
fn add_message_sessions_are_independent() {
    let svc = NativeService::in_memory();
    add_msg(&svc, "a", "user", "a1");
    add_msg(&svc, "a", "user", "a2");
    let b1 = add_msg(&svc, "b", "user", "b1");
    assert_eq!(ints(&b1, 0), vec![1], "a new session restarts at seq 1");
    assert_eq!(
        b1.stats.relationships_created, 0,
        "never linked across sessions"
    );
}

#[test]
fn added_messages_are_recallable() {
    let svc = NativeService::in_memory();
    add_msg(&svc, "s", "user", "the zqxdelta rollout");
    add_msg(&svc, "s", "assistant", "noted");
    let res = run_ok(
        &svc,
        "CALL drevo.memory.recall('s', 'zqxdelta', 5) YIELD node RETURN node.text AS text",
    );
    assert_eq!(texts(&res), vec!["the zqxdelta rollout"]);
}

#[test]
fn record_reasoning_links_the_trace_to_the_latest_message() {
    let svc = NativeService::in_memory();
    add_msg(&svc, "s", "user", "first");
    add_msg(&svc, "s", "user", "please deploy");

    let res = run_ok(
        &svc,
        "CALL drevo.memory.recordReasoning('s', 'decided to deploy', 'shell', 'ok') YIELD node \
         RETURN node.step AS step, node.tool AS tool, node.outcome AS outcome, \
         node.session AS session, node.id IS NOT NULL AS has_id",
    );
    assert_eq!(
        res.rows[0],
        vec![
            Value::String("decided to deploy".into()),
            Value::String("shell".into()),
            Value::String("ok".into()),
            Value::String("s".into()),
            Value::Bool(true),
        ]
    );
    assert_eq!(res.stats.nodes_created, 1);
    assert_eq!(res.stats.relationships_created, 1);

    let link = run_ok(
        &svc,
        "MATCH (t:ReasoningTrace)-[:INITIATED_BY]->(m:Message) RETURN m.text AS text",
    );
    assert_eq!(texts(&link), vec!["please deploy"]);
}

#[test]
fn record_reasoning_without_messages_or_optional_fields() {
    let svc = NativeService::in_memory();
    let res = run_ok(
        &svc,
        "CALL drevo.memory.recordReasoning('empty', 'thinking', null, null) YIELD node \
         RETURN node.step AS step, node.tool AS tool, node.outcome AS outcome",
    );
    assert_eq!(
        res.rows[0],
        vec![Value::String("thinking".into()), Value::Null, Value::Null]
    );
    assert_eq!(res.stats.nodes_created, 1);
    assert_eq!(
        res.stats.relationships_created, 0,
        "no message to anchor to"
    );
}

#[test]
fn standalone_add_message_call_returns_the_node_column() {
    let svc = NativeService::in_memory();
    let res = run_ok(
        &svc,
        "CALL drevo.memory.addMessage('s', 'user', 'bare call')",
    );
    assert_eq!(res.columns, vec!["node".to_string()]);
    assert!(matches!(res.rows[0][0], Value::Node(_)));
}

// ----- durability: the memory chain survives a reopen -----------------------

static NEXT_DIR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp_wal() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "drevo_memory_{}_{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("native.wal")
}

/// An agent session that spans a server restart: the turns and the reasoning
/// written before the restart are replayed after it, and the next turn
/// continues the same chain (seq and `:NEXT`) instead of starting over.
#[test]
fn memory_chain_survives_a_reopen_and_keeps_growing() {
    let wal = tmp_wal();
    {
        let svc = NativeService::open(&wal).unwrap();
        add_msg(&svc, "agent", "user", "remember the zqxepsilon port");
        add_msg(&svc, "agent", "assistant", "stored");
        run_ok(
            &svc,
            "CALL drevo.memory.recordReasoning('agent', 'saved port to memory', null, 'ok') \
             YIELD node RETURN node",
        );
    }
    let svc = NativeService::open(&wal).unwrap();
    let third = add_msg(&svc, "agent", "user", "what port?");
    assert_eq!(ints(&third, 0), vec![3], "seq continues after the reopen");
    assert_eq!(
        third.stats.relationships_created, 1,
        ":NEXT from the pre-restart turn"
    );

    let convo = run_ok(
        &svc,
        "CALL drevo.memory.getConversation('agent', 10) YIELD node RETURN node.text AS text",
    );
    assert_eq!(
        texts(&convo),
        vec!["remember the zqxepsilon port", "stored", "what port?"]
    );
    let recalled = run_ok(
        &svc,
        "CALL drevo.memory.recall('agent', 'zqxepsilon', 3) YIELD node RETURN node.text AS text",
    );
    assert_eq!(texts(&recalled), vec!["remember the zqxepsilon port"]);
    let trace = run_ok(
        &svc,
        "MATCH (t:ReasoningTrace {session: 'agent'})-[:INITIATED_BY]->(m:Message) \
         RETURN m.text AS text",
    );
    assert_eq!(texts(&trace), vec!["stored"]);
    let _ = std::fs::remove_dir_all(wal.parent().unwrap());
}
