//! `:ContextTrace` — what went into a graph-RAG context. `drevo.rag.context`
//! with a `session` (or `trace: true`) records the trace: the nodes written
//! into the text (`INCLUDED`), the ones retrieved but left out (`SKIPPED`),
//! and the session's newest message (`INITIATED_BY`). `drevo.rag.traces`,
//! `drevo.rag.explain`, `drevo.rag.replay` and `drevo.rag.pruneTraces` read,
//! explain, re-run and prune them; `DREVO_RAG_TRACE_RETENTION_DAYS` prunes
//! at startup.
//!
//! The fixture is a CBT journal: a person writes entries, an entry contains
//! automatic thoughts, a thought is an instance of a cognitive distortion.
//! There is no embedder, so the seeds come from BM25.

#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, NodeValue, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn try_run(db: &NativeService, source: &str) -> Result<ExecResult, ExecError> {
    let q = parse(source).unwrap_or_else(|e| panic!("parse `{source}`: {e}"));
    db.execute(&q, HashMap::new())
}

fn run(db: &NativeService, source: &str) -> ExecResult {
    try_run(db, source).unwrap_or_else(|e| panic!("`{source}`: {e}"))
}

/// The `InvalidProcedureCall` message `source` fails with.
fn invalid_call(db: &NativeService, source: &str) -> String {
    match try_run(db, source) {
        Err(ExecError::InvalidProcedureCall { name, message, .. }) => {
            assert!(name.starts_with("drevo.rag."), "{name}");
            message
        }
        other => panic!("`{source}`: expected InvalidProcedureCall, got {other:?}"),
    }
}

fn journal(db: &NativeService) {
    run(
        db,
        "CREATE (mila:Person {name: 'Mila', description: 'Keeps a mood journal'}), \
                (monday:Entry {title: 'Monday shift', body: 'Panic before the team meeting, heart racing.'}), \
                (sunday:Entry {title: 'Sunday walk', body: 'A calm walk by the river.'}), \
                (thought:Thought {text: 'Everyone will see I am incompetent at the meeting'}), \
                (mind:Distortion {name: 'Mind reading', description: 'Assuming what others think'}), \
                (cata:Distortion {name: 'Catastrophizing', description: 'Expecting the worst outcome'}), \
                (mila)-[:WROTE]->(monday), (mila)-[:WROTE]->(sunday), \
                (monday)-[:CONTAINS]->(thought), (thought)-[:IS_A]->(mind), \
                (thought)-[:IS_A]->(cata)",
    );
}

fn db() -> NativeService {
    let db = NativeService::in_memory();
    journal(&db);
    db
}

fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn ints(v: &Value) -> Vec<i64> {
    match v {
        Value::List(items) => items.iter().map(int).collect(),
        other => panic!("expected a list, got {other:?}"),
    }
}

/// One `drevo.rag.context` row.
#[derive(Debug, Clone, PartialEq)]
struct Context {
    text: String,
    tokens: i64,
    node_ids: Vec<i64>,
    rel_ids: Vec<i64>,
    truncated: bool,
    trace_id: Value,
}

fn context(db: &NativeService, query: &str, config: &str) -> Context {
    let result = run(
        db,
        &format!(
            "CALL drevo.rag.context('{query}', {config}) \
             YIELD text, tokens, nodeIds, relIds, truncated, traceId \
             RETURN text, tokens, nodeIds, relIds, truncated, traceId"
        ),
    );
    assert_eq!(result.rows.len(), 1, "{result:?}");
    let r = &result.rows[0];
    Context {
        text: string(&r[0]),
        tokens: int(&r[1]),
        node_ids: ints(&r[2]),
        rel_ids: ints(&r[3]),
        truncated: matches!(r[4], Value::Bool(true)),
        trace_id: r[5].clone(),
    }
}

/// The trace id of a context that recorded one.
fn traced(ctx: &Context) -> String {
    match &ctx.trace_id {
        Value::String(id) => id.clone(),
        other => panic!("expected a trace id, got {other:?}"),
    }
}

fn count(db: &NativeService, query: &str) -> i64 {
    int(&run(db, query).rows[0][0])
}

fn traces(db: &NativeService) -> i64 {
    count(db, "MATCH (t:ContextTrace) RETURN count(t)")
}

/// The trace node with `id`.
fn trace(db: &NativeService, id: &str) -> Arc<NodeValue> {
    let rows = run(
        db,
        &format!("MATCH (t:ContextTrace) WHERE t.id = '{id}' RETURN t"),
    )
    .rows;
    assert_eq!(rows.len(), 1, "{rows:?}");
    match &rows[0][0] {
        Value::Node(n) => Arc::clone(n),
        other => panic!("expected a node, got {other:?}"),
    }
}

fn prop<'a>(node: &'a NodeValue, key: &str) -> &'a Value {
    node.properties
        .get(key)
        .unwrap_or_else(|| panic!("no `{key}` on {node:?}"))
}

fn add_message(db: &NativeService, session: &str, text: &str) -> i64 {
    count(
        db,
        &format!(
            "CALL drevo.memory.addMessage('{session}', 'user', '{text}') \
             YIELD node RETURN id(node)"
        ),
    )
}

/// `k: 1` and the smallest budget: the seed fits, some of its neighbours
/// do not.
const TIGHT: &str = "k: 1, tokenBudget: 50";

// ── writing a trace ─────────────────────────────────────────────────────────

#[test]
fn a_session_records_one_trace_of_what_went_in() {
    let db = db();
    add_message(&db, "chat-1", "why the worry at work");
    let newest = add_message(&db, "chat-1", "the same worry again");
    add_message(&db, "chat-2", "another conversation");

    let ctx = context(
        &db,
        "team meeting",
        &format!("{{{TIGHT}, session: 'chat-1'}}"),
    );
    assert!(ctx.truncated, "{ctx:?}");
    let id = traced(&ctx);
    assert_eq!(traces(&db), 1);

    let t = trace(&db, &id);
    assert_eq!(t.labels, ["ContextTrace"]);
    assert_eq!(string(prop(&t, "session")), "chat-1");
    assert_eq!(string(prop(&t, "query")), "team meeting");
    assert!(string(prop(&t, "created_at")).ends_with('Z'));
    assert_eq!(string(prop(&t, "format")), "markdown");
    assert_eq!(string(prop(&t, "tokenizer")), "approx");
    assert_eq!(int(prop(&t, "tokens")), ctx.tokens);
    assert_eq!(int(prop(&t, "tokenBudget")), 50);
    assert_eq!(prop(&t, "truncated"), &Value::Bool(true));
    assert_eq!(string(prop(&t, "mode")), "fts");
    assert_eq!(int(prop(&t, "hops")), 2);
    assert_eq!(string(prop(&t, "rank")), "ppr");
    assert_eq!(int(prop(&t, "seedCount")), 1);
    assert_eq!(int(prop(&t, "nodeCount")), ctx.node_ids.len() as i64);
    assert_eq!(int(prop(&t, "relCount")), ctx.rel_ids.len() as i64);
    assert!(int(prop(&t, "durationMs")) >= 0);
    assert_eq!(string(prop(&t, "text")), ctx.text);
    let config: serde_json::Value = serde_json::from_str(&string(prop(&t, "config"))).unwrap();
    assert_eq!(config["tokenBudget"], 50);
    assert_eq!(config["k"], 1);
    assert!(config.get("session").is_none(), "{config}");

    // INCLUDED: the written nodes, ranked 1..n in the order written.
    let included = run(
        &db,
        &format!(
            "MATCH (t:ContextTrace)-[r:INCLUDED]->(n) WHERE t.id = '{id}' \
             RETURN id(n), r.rank, r.tokens, r.distance ORDER BY r.rank"
        ),
    )
    .rows;
    let ids: Vec<i64> = included.iter().map(|r| int(&r[0])).collect();
    assert_eq!(ids, ctx.node_ids);
    let ranks: Vec<i64> = included.iter().map(|r| int(&r[1])).collect();
    assert_eq!(ranks, (1..=ids.len() as i64).collect::<Vec<_>>());
    let tokens: i64 = included.iter().map(|r| int(&r[2])).sum();
    assert_eq!(tokens, ctx.tokens);
    assert_eq!(int(&included[0][3]), 0, "the seed comes first");

    // SKIPPED: the budget-cut nodes, after them.
    let skipped = run(
        &db,
        &format!(
            "MATCH (t:ContextTrace)-[r:SKIPPED]->(n) WHERE t.id = '{id}' \
             RETURN id(n), r.rank, r.reason ORDER BY r.rank"
        ),
    )
    .rows;
    assert!(!skipped.is_empty());
    for (i, row) in skipped.iter().enumerate() {
        assert!(!ctx.node_ids.contains(&int(&row[0])), "{row:?}");
        assert_eq!(int(&row[1]), (ids.len() + i + 1) as i64);
        assert_eq!(string(&row[2]), "budget");
    }

    // INITIATED_BY: the session's newest message, and only it.
    let initiated = run(
        &db,
        &format!(
            "MATCH (t:ContextTrace)-[:INITIATED_BY]->(m:Message) WHERE t.id = '{id}' \
             RETURN id(m)"
        ),
    )
    .rows;
    assert_eq!(initiated, vec![vec![Value::Integer(newest)]]);
}

#[test]
fn a_session_without_messages_records_a_trace_without_initiated_by() {
    let db = db();
    let id = traced(&context(&db, "team meeting", "{session: 'new-chat'}"));
    assert_eq!(traces(&db), 1);
    assert_eq!(
        count(
            &db,
            "MATCH (:ContextTrace)-[r:INITIATED_BY]->() RETURN count(r)"
        ),
        0
    );
    assert_eq!(string(prop(&trace(&db, &id), "session")), "new-chat");
}

#[test]
fn trace_true_records_a_trace_without_a_session() {
    let db = db();
    add_message(&db, "chat-1", "a message of another session");
    let id = traced(&context(&db, "team meeting", "{trace: true}"));
    let t = trace(&db, &id);
    assert!(
        matches!(t.properties.get("session"), None | Some(Value::Null)),
        "{t:?}"
    );
    assert_eq!(
        count(
            &db,
            "MATCH (:ContextTrace)-[r:INITIATED_BY]->() RETURN count(r)"
        ),
        0
    );
}

#[test]
fn without_a_session_or_trace_nothing_is_written() {
    let db = db();
    let nodes = count(&db, "MATCH (n) RETURN count(n)");
    let rels = count(&db, "MATCH ()-[r]->() RETURN count(r)");
    for config in ["{}", "null", "{trace: false}", "{session: null}"] {
        let ctx = context(&db, "team meeting", config);
        assert_eq!(ctx.trace_id, Value::Null, "{config}");
    }
    run(
        &db,
        "CALL drevo.rag.retrieve('team meeting', {session: 'chat-1', trace: true}) \
         YIELD node RETURN node",
    );
    assert_eq!(count(&db, "MATCH (n) RETURN count(n)"), nodes);
    assert_eq!(count(&db, "MATCH ()-[r]->() RETURN count(r)"), rels);
}

#[test]
fn an_empty_context_is_traced_too() {
    let db = db();
    let ctx = context(&db, "zebra quantum", "{session: 'chat-1'}");
    assert_eq!(ctx.text, "");
    let t = trace(&db, &traced(&ctx));
    assert_eq!(int(prop(&t, "seedCount")), 0);
    assert_eq!(int(prop(&t, "nodeCount")), 0);
    assert_eq!(
        count(
            &db,
            "MATCH (:ContextTrace)-[r]->() WHERE type(r) <> 'INITIATED_BY' RETURN count(r)"
        ),
        0
    );
}

#[test]
fn a_node_cut_by_max_nodes_is_skipped_with_that_reason() {
    let db = db();
    // Monday shift is the seed; maxNodes 2 takes one neighbour and stops at
    // the next.
    let ctx = context(&db, "team meeting", "{k: 1, maxNodes: 2, hops: 1}");
    assert!(ctx.truncated);
    let id = traced(&context(
        &db,
        "team meeting",
        "{k: 1, maxNodes: 2, hops: 1, trace: true}",
    ));
    let rows = run(
        &db,
        &format!(
            "MATCH (t:ContextTrace)-[r:SKIPPED]->(n) WHERE t.id = '{id}' \
             RETURN r.reason, r.rank"
        ),
    )
    .rows;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(string(&rows[0][0]), "maxNodes");
    assert_eq!(int(&rows[0][1]), 3);
}

#[test]
fn the_config_holds_no_secrets() {
    let db = db();
    let id = traced(&context(
        &db,
        "team meeting",
        "{session: 'chat-1', format: 'json', label: 'Entry'}",
    ));
    let config = string(prop(&trace(&db, &id), "config"));
    assert!(!config.contains("DREVO_"), "{config}");
    let parsed: serde_json::Value = serde_json::from_str(&config).unwrap();
    let mut keys: Vec<&str> = parsed
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "asOf",
            "candidates",
            "direction",
            "embeddingProperty",
            "excludeLabels",
            "format",
            "hops",
            "includeProvenance",
            "k",
            "label",
            "labels",
            "maxBodyChars",
            "maxNodes",
            "mode",
            "perNodeLimit",
            "rank",
            "relTypes",
            "rrfK",
            "source",
            "textProperties",
            "tokenBudget",
            "tokenizer",
        ]
    );
    for key in keys {
        let lower = key.to_lowercase();
        for word in ["secret", "password", "apikey", "api_key", "credential"] {
            assert!(!lower.contains(word), "{key}");
        }
    }
}

#[test]
fn traces_stay_out_of_retrieval() {
    let db = db();
    // A message and a pile of traces that all match the question better
    // than the entry: they are left out of the seeds without taking the
    // places of the `k` seeds.
    add_message(&db, "chat-1", "team meeting");
    for _ in 0..25 {
        let ctx = context(&db, "team meeting", "{session: 'chat-1', k: 1}");
        assert!(ctx.text.contains("Monday shift"), "{ctx:?}");
    }
    // The traces hold the question and the text, yet are never retrieved.
    let labels = run(
        &db,
        "CALL drevo.rag.retrieve('team meeting', {k: 20, hops: 3}) \
         YIELD node RETURN labels(node)",
    )
    .rows;
    assert!(!labels.is_empty());
    for row in &labels {
        assert!(
            !format!("{:?}", row[0]).contains("ContextTrace"),
            "{labels:?}"
        );
    }
    let ctx = context(&db, "team meeting", "{k: 20}");
    assert!(!ctx.text.contains("ContextTrace"), "{}", ctx.text);
}

// ── reading traces ──────────────────────────────────────────────────────────

/// One `drevo.rag.explain` row.
#[derive(Debug, Clone, PartialEq)]
struct Explained {
    node: i64,
    status: String,
    rank: i64,
    distance: Value,
    seed_score: Value,
    ppr_score: Value,
    tokens: Value,
    reason: Value,
}

fn explain(db: &NativeService, id: &str) -> Vec<Explained> {
    run(
        db,
        &format!(
            "CALL drevo.rag.explain('{id}') \
             YIELD node, status, rank, distance, seedScore, pprScore, tokens, reason \
             RETURN id(node), status, rank, distance, seedScore, pprScore, tokens, reason"
        ),
    )
    .rows
    .into_iter()
    .map(|r| Explained {
        node: int(&r[0]),
        status: string(&r[1]),
        rank: int(&r[2]),
        distance: r[3].clone(),
        seed_score: r[4].clone(),
        ppr_score: r[5].clone(),
        tokens: r[6].clone(),
        reason: r[7].clone(),
    })
    .collect()
}

#[test]
fn explain_lists_the_included_then_the_skipped_in_rank_order() {
    let db = db();
    let ctx = context(
        &db,
        "team meeting",
        &format!("{{{TIGHT}, session: 'chat-1'}}"),
    );
    let rows = explain(&db, &traced(&ctx));
    let ranks: Vec<i64> = rows.iter().map(|r| r.rank).collect();
    assert_eq!(ranks, (1..=rows.len() as i64).collect::<Vec<_>>());
    let (included, skipped): (Vec<&Explained>, Vec<&Explained>) =
        rows.iter().partition(|r| r.status == "included");
    assert_eq!(
        included.iter().map(|r| r.node).collect::<Vec<_>>(),
        ctx.node_ids
    );
    assert!(!skipped.is_empty());
    assert!(skipped.iter().all(|r| r.status == "skipped"));
    let tokens: i64 = included.iter().map(|r| int(&r.tokens)).sum();
    assert_eq!(tokens, ctx.tokens);
    let seed = included[0];
    assert_eq!(seed.distance, Value::Integer(0));
    assert!(matches!(seed.seed_score, Value::Float(_)), "{seed:?}");
    assert!(matches!(seed.ppr_score, Value::Float(_)), "{seed:?}");
    assert_eq!(seed.reason, Value::Null);
    for r in &included[1..] {
        assert_eq!(r.seed_score, Value::Null, "{r:?}");
        assert!(int(&r.distance) >= 1, "{r:?}");
    }
    for r in &skipped {
        assert_eq!(r.reason, Value::String("budget".to_string()), "{r:?}");
        assert_eq!(r.tokens, Value::Null, "{r:?}");
        assert_eq!(r.distance, Value::Null, "{r:?}");
    }
}

#[test]
fn traces_lists_the_newest_first_by_session_or_all() {
    let db = db();
    let a1 = traced(&context(&db, "team meeting", "{session: 'a'}"));
    let a2 = traced(&context(&db, "river walk", "{session: 'a'}"));
    let a3 = traced(&context(&db, "mind reading", "{session: 'a'}"));
    let b1 = traced(&context(&db, "journal", "{session: 'b'}"));
    let anonymous = traced(&context(&db, "journal", "{trace: true}"));
    let ids = |session: &str, limit: i64| -> Vec<String> {
        run(
            &db,
            &format!("CALL drevo.rag.traces({session}, {limit}) YIELD trace RETURN trace.id"),
        )
        .rows
        .iter()
        .map(|r| string(&r[0]))
        .collect()
    };
    assert_eq!(ids("'a'", 2), [a3.clone(), a2.clone()]);
    assert_eq!(ids("'a'", 10), [a3.clone(), a2.clone(), a1.clone()]);
    assert_eq!(ids("'b'", 10), std::slice::from_ref(&b1));
    assert_eq!(ids("'none'", 10), Vec::<String>::new());
    assert_eq!(ids("null", 10), [anonymous.clone(), b1, a3, a2, a1]);
    assert_eq!(ids("null", 1), [anonymous]);
    assert_eq!(ids("null", 0), Vec::<String>::new());
}

// ── replay ──────────────────────────────────────────────────────────────────

fn replay(db: &NativeService, id: &str) -> (String, i64, bool) {
    let rows = run(
        db,
        &format!("CALL drevo.rag.replay('{id}') YIELD text, tokens, changed RETURN text, tokens, changed"),
    )
    .rows;
    assert_eq!(rows.len(), 1, "{rows:?}");
    (
        string(&rows[0][0]),
        int(&rows[0][1]),
        matches!(rows[0][2], Value::Bool(true)),
    )
}

#[test]
fn replay_reruns_the_stored_config_without_writing_a_trace() {
    let db = db();
    add_message(&db, "chat-1", "the meeting");
    let ctx = context(
        &db,
        "team meeting",
        "{session: 'chat-1', format: 'triples', hops: 1}",
    );
    let id = traced(&ctx);
    let nodes = count(&db, "MATCH (n) RETURN count(n)");

    let (text, tokens, changed) = replay(&db, &id);
    assert_eq!(text, ctx.text);
    assert_eq!(tokens, ctx.tokens);
    assert!(!changed);
    assert_eq!(traces(&db), 1);

    // A new fact next to the seed enters the budget: the text changes.
    run(
        &db,
        "MATCH (e:Entry {title: 'Monday shift'}) \
         CREATE (e)-[:MENTIONS]->(:Person {name: 'Coach Ana'})",
    );
    let (text, _, changed) = replay(&db, &id);
    assert!(changed);
    assert!(text.contains("Coach Ana"), "{text}");
    assert_ne!(text, ctx.text);
    assert_eq!(traces(&db), 1, "replay never writes a trace");
    assert_eq!(count(&db, "MATCH (n) RETURN count(n)"), nodes + 1);
}

// ── pruning ─────────────────────────────────────────────────────────────────

#[test]
fn prune_deletes_only_older_traces_and_their_edges() {
    let db = db();
    let old_1 = traced(&context(&db, "team meeting", "{session: 'a'}"));
    let old_2 = traced(&context(&db, "river walk", "{session: 'a'}"));
    let kept = traced(&context(&db, "team meeting", "{session: 'a'}"));
    for id in [&old_1, &old_2] {
        run(
            &db,
            &format!(
                "MATCH (t:ContextTrace) WHERE t.id = '{id}' \
                 SET t.created_at = '2020-06-01T00:00:00.000Z'"
            ),
        );
    }
    let knowledge = count(&db, "MATCH (n) WHERE NOT n:ContextTrace RETURN count(n)");
    let kept_edges = count(
        &db,
        &format!("MATCH (t:ContextTrace)-[r]->() WHERE t.id = '{kept}' RETURN count(r)"),
    );
    assert!(kept_edges > 0);

    let deleted = count(
        &db,
        "CALL drevo.rag.pruneTraces('2021-01-01') YIELD deleted RETURN deleted",
    );
    assert_eq!(deleted, 2);
    let left: Vec<String> = run(&db, "MATCH (t:ContextTrace) RETURN t.id")
        .rows
        .iter()
        .map(|r| string(&r[0]))
        .collect();
    assert_eq!(left, std::slice::from_ref(&kept));
    assert_eq!(
        count(&db, "MATCH (t:ContextTrace)-[r]->() RETURN count(r)"),
        kept_edges,
        "the pruned traces' edges are gone"
    );
    assert_eq!(
        count(&db, "MATCH (n) WHERE NOT n:ContextTrace RETURN count(n)"),
        knowledge,
        "the nodes a trace pointed at stay"
    );
    // Nothing older is left.
    assert_eq!(
        count(
            &db,
            "CALL drevo.rag.pruneTraces('2021-01-01') YIELD deleted RETURN deleted"
        ),
        0
    );
}

// ── errors ──────────────────────────────────────────────────────────────────

#[test]
fn every_error_is_named() {
    let db = db();
    let id = traced(&context(&db, "team meeting", "{trace: true}"));
    let cases = [
        (
            "CALL drevo.rag.context('panic', {trace: 'yes'})".to_string(),
            "trace must be a boolean, got 'yes'".to_string(),
        ),
        (
            "CALL drevo.rag.explain(5)".to_string(),
            "traceId must be a string, got Integer".to_string(),
        ),
        (
            "CALL drevo.rag.explain('nope')".to_string(),
            "no context trace with id `nope`".to_string(),
        ),
        (
            "CALL drevo.rag.replay(null)".to_string(),
            "traceId must be a string, got Null".to_string(),
        ),
        (
            "CALL drevo.rag.replay('nope')".to_string(),
            "no context trace with id `nope`".to_string(),
        ),
        (
            "CALL drevo.rag.traces(5, 2)".to_string(),
            "session must be a string or null, got Integer".to_string(),
        ),
        (
            "CALL drevo.rag.traces(null, -1)".to_string(),
            "limit must be an integer ≥ 0, got -1".to_string(),
        ),
        (
            "CALL drevo.rag.traces(null, 'all')".to_string(),
            "limit must be an integer ≥ 0, got String".to_string(),
        ),
        (
            "CALL drevo.rag.pruneTraces('yesterday')".to_string(),
            "olderThan must be an ISO-8601 UTC date or timestamp \
             ('2025-01-01' or '2025-01-01T00:00:00Z'), got 'yesterday'"
                .to_string(),
        ),
        (
            "CALL drevo.rag.pruneTraces(5)".to_string(),
            "olderThan must be an ISO-8601 UTC date or timestamp \
             ('2025-01-01' or '2025-01-01T00:00:00Z'), got Integer"
                .to_string(),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(invalid_call(&db, &source), expected, "{source}");
    }

    // A trace whose stored config no longer reads cannot be replayed.
    run(
        &db,
        &format!("MATCH (t:ContextTrace) WHERE t.id = '{id}' SET t.config = '{{'"),
    );
    let message = invalid_call(&db, &format!("CALL drevo.rag.replay('{id}')"));
    assert!(
        message.starts_with(&format!("trace `{id}` has an unreadable config: ")),
        "{message}"
    );
    run(
        &db,
        &format!("MATCH (t:ContextTrace) WHERE t.id = '{id}' SET t.config = '{{\"depth\": 2}}'"),
    );
    let message = invalid_call(&db, &format!("CALL drevo.rag.replay('{id}')"));
    assert!(
        message.starts_with(&format!(
            "trace `{id}` has an unreadable config: unknown config key `depth`"
        )),
        "{message}"
    );
    run(
        &db,
        &format!("MATCH (t:ContextTrace) WHERE t.id = '{id}' REMOVE t.config"),
    );
    assert_eq!(
        invalid_call(&db, &format!("CALL drevo.rag.replay('{id}')")),
        format!("trace `{id}` has an unreadable config: it has no config")
    );
    let other = traced(&context(&db, "river walk", "{trace: true}"));
    run(
        &db,
        &format!("MATCH (t:ContextTrace) WHERE t.id = '{other}' REMOVE t.query"),
    );
    assert_eq!(
        invalid_call(&db, &format!("CALL drevo.rag.replay('{other}')")),
        format!("trace `{other}` has no query")
    );
}

#[test]
fn the_procedures_take_their_arguments() {
    let db = db();
    for (source, expected) in [
        ("CALL drevo.rag.traces(null)", "expected 2 arguments"),
        ("CALL drevo.rag.explain()", "expected 1 argument"),
        ("CALL drevo.rag.replay('a', 'b')", "expected 1 argument"),
        ("CALL drevo.rag.pruneTraces()", "expected 1 argument"),
    ] {
        let err = try_run(&db, source).unwrap_err();
        assert!(err.to_string().contains(expected), "{source}: {err}");
    }
}

// ── Bolt ────────────────────────────────────────────────────────────────────

fn bolt_records(db: &Arc<NativeService>, query: &str) -> Vec<Vec<Bolt>> {
    let mut s = Session::new_durable(Arc::clone(db));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    let mut replies = s.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    replies.extend(s.handle(ClientMessage::Pull {
        extra: BTreeMap::new(),
    }));
    replies
        .into_iter()
        .filter_map(|m| match m {
            ServerMessage::Record { fields } => Some(fields),
            ServerMessage::Failure { metadata } => panic!("{query}: {metadata:?}"),
            _ => None,
        })
        .collect()
}

#[test]
fn bolt_round_trip_types() {
    let db = Arc::new(db());
    let rows = bolt_records(
        &db,
        &format!(
            "CALL drevo.rag.context('team meeting', {{{TIGHT}, session: 'chat-1'}}) \
             YIELD traceId RETURN traceId"
        ),
    );
    let Bolt::String(id) = &rows[0][0] else {
        panic!("traceId: {:?}", rows[0][0]);
    };
    let rows = bolt_records(
        &db,
        &format!(
            "CALL drevo.rag.explain('{id}') \
             YIELD node, status, rank, distance, seedScore, pprScore, tokens, reason \
             RETURN node, status, rank, distance, seedScore, pprScore, tokens, reason"
        ),
    );
    let first = &rows[0];
    assert!(matches!(first[0], Bolt::Structure { .. }), "{:?}", first[0]);
    assert_eq!(first[1], Bolt::String("included".to_string()));
    assert_eq!(first[2], Bolt::Integer(1));
    assert_eq!(first[3], Bolt::Integer(0));
    assert!(matches!(first[4], Bolt::Float(_)), "{:?}", first[4]);
    assert!(matches!(first[5], Bolt::Float(_)), "{:?}", first[5]);
    assert!(
        matches!(first[6], Bolt::Integer(t) if t > 0),
        "{:?}",
        first[6]
    );
    assert_eq!(first[7], Bolt::Null);
    let last = rows.last().unwrap();
    assert_eq!(last[1], Bolt::String("skipped".to_string()));
    assert_eq!(last[7], Bolt::String("budget".to_string()));

    let rows = bolt_records(
        &db,
        "CALL drevo.rag.traces('chat-1', 5) YIELD trace RETURN trace",
    );
    assert_eq!(rows.len(), 1);
    assert!(
        matches!(rows[0][0], Bolt::Structure { .. }),
        "{:?}",
        rows[0][0]
    );
    let rows = bolt_records(
        &db,
        &format!("CALL drevo.rag.replay('{id}') YIELD text, tokens, changed RETURN text, tokens, changed"),
    );
    assert!(matches!(rows[0][0], Bolt::String(_)), "{:?}", rows[0][0]);
    assert!(matches!(rows[0][1], Bolt::Integer(_)), "{:?}", rows[0][1]);
    assert_eq!(rows[0][2], Bolt::Boolean(false));
    let rows = bolt_records(
        &db,
        "CALL drevo.rag.pruneTraces('2000-01-01') YIELD deleted RETURN deleted",
    );
    assert_eq!(rows[0][0], Bolt::Integer(0));
}

// ── retention ───────────────────────────────────────────────────────────────

fn config_from(entries: &[(&'static str, &str)]) -> Result<drevo::server::Config, String> {
    let env: HashMap<&'static str, String> = entries
        .iter()
        .map(|(k, v)| (*k, (*v).to_string()))
        .collect();
    drevo::server::Config::from_env(move |k| env.get(k).cloned()).map_err(|e| e.to_string())
}

#[test]
fn retention_days_default_to_keeping_traces_forever() {
    assert_eq!(config_from(&[]).unwrap().rag_trace_retention_days, 0);
    assert_eq!(
        config_from(&[("DREVO_RAG_TRACE_RETENTION_DAYS", " 30 ")])
            .unwrap()
            .rag_trace_retention_days,
        30
    );
    for bad in ["-1", "a week", "1.5"] {
        assert_eq!(
            config_from(&[("DREVO_RAG_TRACE_RETENTION_DAYS", bad)]).unwrap_err(),
            format!(
                "invalid DREVO_RAG_TRACE_RETENTION_DAYS value `{bad}`: expected a whole \
                 number of days (0 = keep traces forever)"
            )
        );
    }
}

/// `POST /cypher` on a running server; the response's JSON body.
async fn post_cypher(port: u16, query: &str) -> serde_json::Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = serde_json::json!({ "query": query }).to_string();
    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let request = format!(
        "POST /cypher HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    tcp.write_all(request.as_bytes()).await.unwrap();
    let mut reply = Vec::new();
    let _ = tcp.read_to_end(&mut reply).await;
    let reply = String::from_utf8_lossy(&reply).to_string();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    let json = reply.split("\r\n\r\n").nth(1).unwrap_or("");
    serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}: {reply}"))
}

#[tokio::test]
async fn the_server_prunes_traces_past_the_retention_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = NativeService::open(dir.path().join("native.wal")).unwrap();
        journal(&db);
        let old = traced(&context(&db, "team meeting", "{session: 'chat-1'}"));
        traced(&context(&db, "river walk", "{session: 'chat-1'}"));
        run(
            &db,
            &format!(
                "MATCH (t:ContextTrace) WHERE t.id = '{old}' \
                 SET t.created_at = '2020-01-01T00:00:00.000Z'"
            ),
        );
        assert_eq!(traces(&db), 2);
    }
    let data_dir = dir.path().to_string_lossy().to_string();
    let mut cfg = config_from(&[
        ("DREVO_HOST", "127.0.0.1"),
        ("DREVO_DATA_DIR", data_dir.as_str()),
        ("DREVO_AUTH", "none"),
        ("DREVO_RAG_TRACE_RETENTION_DAYS", "1"),
    ])
    .unwrap();
    cfg.port = 0;
    let (listening, announced) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let _ = drevo::server::run_announcing(cfg, listening).await;
    });
    let port = tokio::time::timeout(std::time::Duration::from_secs(30), announced)
        .await
        .expect("the server started in time")
        .expect("the server started")
        .http
        .port();
    let body = post_cypher(
        port,
        "MATCH (t:ContextTrace) RETURN count(t), min(t.created_at) > '2021'",
    )
    .await;
    assert_eq!(body["rows"], serde_json::json!([[1, true]]), "{body}");
    server.abort();
}
