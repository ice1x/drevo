//! `CALL drevo.rag.retrieve(query[, config]) YIELD node, seedScore, distance,
//! pprScore, rank, via, truncated` and `CALL drevo.rag.context(query[,
//! config]) YIELD text, tokens, format, nodeIds, relIds, truncated, mode,
//! traceId`: a question in, the part of the graph that answers it out — seeds
//! from the hybrid search, a k-hop expansion, a ranking, and (for `context`)
//! LLM-ready text within a token budget, in one statement.
//!
//! The fixture is a CBT journal: a person writes entries, an entry contains
//! automatic thoughts, a thought is an instance of a cognitive distortion.
//! The embedder is an in-process stub: text about panic or a meeting points
//! at `[1, 0, 0]`, calm text at `[0, 1, 0]`, anything else at `[0, 0, 1]`.

#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::embeddings::{EmbeddingsError, TextEmbedder};
use drevo::native_service::NativeService;

struct StubEmbedder;

impl TextEmbedder for StubEmbedder {
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingsError> {
        let text = text.to_lowercase();
        Ok(if text.contains("panic") || text.contains("meeting") {
            vec![1.0, 0.0, 0.0]
        } else if text.contains("calm") {
            vec![0.0, 1.0, 0.0]
        } else {
            vec![0.0, 0.0, 1.0]
        })
    }
}

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
                (monday:Entry {title: 'Monday shift', body: 'Panic before the team meeting, heart racing.', embedding: [1.0, 0.0, 0.0]}), \
                (sunday:Entry {title: 'Sunday walk', body: 'A calm walk by the river.', embedding: [0.0, 1.0, 0.0]}), \
                (thought:Thought {text: 'Everyone will see I am incompetent', embedding: [0.9, 0.1, 0.0]}), \
                (mind:Distortion {name: 'Mind reading', description: 'Assuming what others think'}), \
                (cata:Distortion {name: 'Catastrophizing', description: 'Expecting the worst outcome'}), \
                (msg:Message {text: 'chat: the meeting panic is back', role: 'user'}), \
                (mila)-[:WROTE]->(monday), (mila)-[:WROTE]->(sunday), \
                (monday)-[:CONTAINS]->(thought), (thought)-[:IS_A]->(mind), \
                (thought)-[:RELATED_TO {type: 'IS_A', valid_from: '2024-01-01', valid_until: '2025-01-01'}]->(cata), \
                (msg)-[:ABOUT]->(monday)",
    );
}

/// The ids of Mila, the Monday entry and the `WROTE` between them.
fn mila_wrote_monday(db: &NativeService) -> (i64, i64, i64) {
    let r = run(
        db,
        "MATCH (m:Person {name: 'Mila'})-[w:WROTE]->(e:Entry {title: 'Monday shift'}) \
         RETURN id(m), id(e), id(w)",
    );
    let id = |v: &Value| match v {
        Value::Integer(i) => *i,
        other => panic!("expected an id, got {other:?}"),
    };
    (id(&r.rows[0][0]), id(&r.rows[0][1]), id(&r.rows[0][2]))
}

fn with_embedder() -> NativeService {
    let db = NativeService::in_memory();
    assert!(db.set_embedder(Arc::new(StubEmbedder)));
    journal(&db);
    db
}

fn without_embedder() -> NativeService {
    let db = NativeService::in_memory();
    journal(&db);
    db
}

/// The text a node is known by in these tests.
fn label_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    }
}

/// One `drevo.rag.retrieve` row.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    name: String,
    seed_score: Option<f64>,
    distance: i64,
    ppr: Option<f64>,
    rank: i64,
    via: Value,
    truncated: bool,
}

fn opt_float(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        Value::Null => None,
        other => panic!("expected a Float or null, got {other:?}"),
    }
}

fn retrieve(db: &NativeService, query: &str, config: &str) -> Vec<Row> {
    run(
        db,
        &format!(
            "CALL drevo.rag.retrieve('{query}', {config}) \
             YIELD node, seedScore, distance, pprScore, rank, via, truncated \
             RETURN coalesce(node.name, node.title, node.text), seedScore, distance, pprScore, \
                    rank, via, truncated"
        ),
    )
    .rows
    .iter()
    .map(|r| Row {
        name: label_of(&r[0]),
        seed_score: opt_float(&r[1]),
        distance: match r[2] {
            Value::Integer(i) => i,
            ref other => panic!("distance: {other:?}"),
        },
        ppr: opt_float(&r[3]),
        rank: match r[4] {
            Value::Integer(i) => i,
            ref other => panic!("rank: {other:?}"),
        },
        via: r[5].clone(),
        truncated: match r[6] {
            Value::Bool(b) => b,
            ref other => panic!("truncated: {other:?}"),
        },
    })
    .collect()
}

fn names(rows: &[Row]) -> Vec<&str> {
    rows.iter().map(|r| r.name.as_str()).collect()
}

/// One `drevo.rag.context` row.
#[derive(Debug, Clone, PartialEq)]
struct Context {
    text: String,
    tokens: i64,
    format: String,
    node_ids: Vec<i64>,
    rel_ids: Vec<i64>,
    truncated: bool,
    mode: String,
    trace_id: Value,
}

fn ints(v: &Value) -> Vec<i64> {
    match v {
        Value::List(items) => items
            .iter()
            .map(|i| match i {
                Value::Integer(i) => *i,
                other => panic!("id: {other:?}"),
            })
            .collect(),
        other => panic!("expected a list, got {other:?}"),
    }
}

fn context(db: &NativeService, query: &str, config: &str) -> Context {
    let result = run(
        db,
        &format!(
            "CALL drevo.rag.context('{query}', {config}) \
             YIELD text, tokens, format, nodeIds, relIds, truncated, mode, traceId \
             RETURN text, tokens, format, nodeIds, relIds, truncated, mode, traceId"
        ),
    );
    assert_eq!(result.rows.len(), 1, "{result:?}");
    let r = &result.rows[0];
    Context {
        text: label_of(&r[0]),
        tokens: match r[1] {
            Value::Integer(i) => i,
            ref other => panic!("tokens: {other:?}"),
        },
        format: label_of(&r[2]),
        node_ids: ints(&r[3]),
        rel_ids: ints(&r[4]),
        truncated: match r[5] {
            Value::Bool(b) => b,
            ref other => panic!("truncated: {other:?}"),
        },
        mode: label_of(&r[6]),
        trace_id: r[7].clone(),
    }
}

fn node_count(db: &NativeService) -> Value {
    run(db, "MATCH (n) RETURN count(n)").rows[0][0].clone()
}

// ── retrieve ────────────────────────────────────────────────────────────────

#[test]
fn retrieve_returns_seeds_first_then_the_expansion() {
    let db = with_embedder();
    let rows = retrieve(&db, "team meeting panic", "{label: 'Entry'}");
    assert!(!rows.is_empty());
    assert_eq!(rows[0].name, "Monday shift");
    assert!(rows[0].seed_score.is_some_and(|s| s > 0.0));
    assert_eq!(rows[0].distance, 0);
    assert_eq!(rows[0].via, Value::Null);
    let seeds = rows.iter().take_while(|r| r.seed_score.is_some()).count();
    assert!(
        rows[seeds..].iter().all(|r| r.seed_score.is_none()),
        "{rows:?}"
    );
    assert!(rows.iter().all(|r| r.distance <= 2), "{rows:?}");
    assert!(rows[seeds..].iter().all(|r| r.distance >= 1), "{rows:?}");
    let ranks: Vec<i64> = rows.iter().map(|r| r.rank).collect();
    assert_eq!(ranks, (1..=rows.len() as i64).collect::<Vec<_>>());
    assert!(rows.iter().all(|r| !r.truncated));
    // Two hops from Monday: Mila and the thought, then Sunday and both
    // distortions.
    for name in ["Mila", "Everyone will see I am incompetent", "Mind reading"] {
        assert!(names(&rows).contains(&name), "{name}: {rows:?}");
    }
}

#[test]
fn ppr_scores_sum_to_one_over_an_untruncated_retrieval() {
    let db = with_embedder();
    let rows = retrieve(&db, "team meeting panic", "{label: 'Entry'}");
    let total: f64 = rows.iter().map(|r| r.ppr.expect("ppr score")).sum();
    assert!((total - 1.0).abs() < 1e-3, "{total}: {rows:?}");
    // After the seeds, best score first.
    let seeds = rows.iter().take_while(|r| r.seed_score.is_some()).count();
    let rest: Vec<f64> = rows[seeds..].iter().map(|r| r.ppr.unwrap_or(0.0)).collect();
    assert!(rest.windows(2).all(|w| w[0] >= w[1]), "{rest:?}");
}

#[test]
fn rank_modes_give_their_documented_orders() {
    let db = with_embedder();
    let config = |rank: &str| format!("{{label: 'Entry', mode: 'fts', k: 1, rank: '{rank}'}}");

    let by_distance = retrieve(&db, "team meeting", &config("distance"));
    assert!(by_distance.iter().all(|r| r.ppr.is_none()));
    let distances: Vec<i64> = by_distance.iter().map(|r| r.distance).collect();
    let mut sorted = distances.clone();
    sorted.sort_unstable();
    assert_eq!(distances, sorted);

    // 'none' keeps the walk's own order: the rows of drevo.graph.expand.
    let walked = run(
        &db,
        "MATCH (e:Entry {title: 'Monday shift'}) \
         CALL drevo.graph.expand(e, {hops: 2, excludeLabels: ['Message', 'ReasoningTrace', 'ContextTrace', 'IngestJob', 'IngestItem']}) \
         YIELD node RETURN coalesce(node.name, node.title, node.text)",
    );
    let walked: Vec<String> = walked.rows.iter().map(|r| label_of(&r[0])).collect();
    let as_walked = retrieve(&db, "team meeting", &config("none"));
    assert_eq!(names(&as_walked), walked);

    let by_ppr = retrieve(&db, "team meeting", &config("ppr"));
    assert!(by_ppr.iter().all(|r| r.ppr.is_some()));
    let mut same_set = names(&by_ppr);
    let mut walked_set: Vec<&str> = walked.iter().map(String::as_str).collect();
    same_set.sort_unstable();
    walked_set.sort_unstable();
    assert_eq!(same_set, walked_set);
}

#[test]
fn messages_stay_out_by_default_even_when_bm25_finds_them() {
    let db = without_embedder();
    // BM25 alone finds the chat message for this word.
    let hits = run(
        &db,
        "CALL fts.search('meeting', 10) YIELD node RETURN labels(node)[0]",
    );
    assert!(hits
        .rows
        .iter()
        .any(|r| r[0] == Value::String("Message".to_string())));

    let rows = retrieve(&db, "meeting", "{}");
    assert!(!rows.is_empty());
    assert!(
        !names(&rows).contains(&"chat: the meeting panic is back"),
        "{rows:?}"
    );
    // Asked for, it comes back.
    let rows = retrieve(&db, "meeting", "{excludeLabels: []}");
    assert!(
        names(&rows).contains(&"chat: the meeting panic is back"),
        "{rows:?}"
    );
}

#[test]
fn as_of_drops_a_superseded_fact() {
    let db = with_embedder();
    let then = retrieve(
        &db,
        "team meeting panic",
        "{label: 'Entry', asOf: '2024-06-01'}",
    );
    assert!(names(&then).contains(&"Catastrophizing"), "{then:?}");
    let now = retrieve(
        &db,
        "team meeting panic",
        "{label: 'Entry', asOf: '2025-06-01'}",
    );
    assert!(!names(&now).contains(&"Catastrophizing"), "{now:?}");

    let ctx = context(
        &db,
        "team meeting panic",
        "{label: 'Entry', asOf: '2025-06-01'}",
    );
    assert!(!ctx.text.contains("Catastrophizing"), "{}", ctx.text);
    let ctx = context(
        &db,
        "team meeting panic",
        "{label: 'Entry', asOf: '2024-06-01'}",
    );
    assert!(
        ctx.text.contains("valid 2024-01-01→2025-01-01"),
        "{}",
        ctx.text
    );
}

#[test]
fn no_seeds_give_no_rows_and_an_empty_context() {
    let db = with_embedder();
    assert!(retrieve(&db, "zzqqxx", "{}").is_empty());
    let ctx = context(&db, "zzqqxx", "{}");
    assert_eq!(ctx.text, "");
    assert_eq!(ctx.tokens, 0);
    assert_eq!(ctx.format, "markdown");
    assert!(ctx.node_ids.is_empty() && ctx.rel_ids.is_empty());
    assert!(!ctx.truncated);
    assert_eq!(ctx.mode, "fts");
    assert_eq!(ctx.trace_id, Value::Null);
}

#[test]
fn the_source_filter_applies_only_where_provenance_exists() {
    let db = without_embedder();
    run(
        &db,
        "CREATE (:Entry {title: 'Tuesday panic', source_id: 'diary-a'})-[:MENTIONS]->(:Person {name: 'Oleg'}), \
                (:Entry {title: 'Wednesday panic', source_id: 'diary-b'})",
    );
    let all = retrieve(&db, "panic", "{}");
    assert!(names(&all).contains(&"Wednesday panic"), "{all:?}");
    let only_a = retrieve(&db, "panic", "{source: ['diary-a']}");
    let kept = names(&only_a);
    assert!(kept.contains(&"Tuesday panic"), "{only_a:?}");
    assert!(!kept.contains(&"Wednesday panic"), "{only_a:?}");
    // Nodes without a source_id are kept.
    assert!(kept.contains(&"Oleg"), "{only_a:?}");
    assert!(kept.contains(&"Monday shift"), "{only_a:?}");
}

// ── context ─────────────────────────────────────────────────────────────────

#[test]
fn context_writes_markdown_by_default() {
    let db = with_embedder();
    let ctx = context(&db, "team meeting panic", "{label: 'Entry'}");
    assert_eq!(ctx.format, "markdown");
    assert_eq!(ctx.mode, "hybrid");
    let (mila, monday, wrote) = mila_wrote_monday(&db);
    assert!(
        ctx.text.starts_with(&format!(
            "## Seeds\n- Monday shift (kind=Entry, id={monday}): Panic before"
        )),
        "{}",
        ctx.text
    );
    assert!(ctx.text.contains("\n\n## Neighbours\n"), "{}", ctx.text);
    assert!(ctx.text.contains("\n\n## Edges\n"), "{}", ctx.text);
    assert!(
        ctx.text.contains(&format!(
            "- {mila} -[WROTE]-> {monday} (id={wrote}, weight=1.0)\n"
        )),
        "{}",
        ctx.text
    );
    assert!(!ctx.truncated);
    assert_eq!(ctx.trace_id, Value::Null);
    assert_eq!(ctx.tokens, drevo::rag::tokens::approx(&ctx.text) as i64);
    // The ids line up with the retrieval and the text.
    let rows = retrieve(&db, "team meeting panic", "{label: 'Entry'}");
    assert_eq!(ctx.node_ids.len(), rows.len());
    assert!(!ctx.rel_ids.is_empty());
}

#[test]
fn mode_degrades_to_fts_without_an_embedder_or_a_label() {
    let db = without_embedder();
    assert_eq!(
        context(&db, "team meeting panic", "{label: 'Entry'}").mode,
        "fts"
    );
    let db = with_embedder();
    assert_eq!(context(&db, "team meeting panic", "{}").mode, "fts");
    assert_eq!(
        context(&db, "team meeting panic", "{label: 'Entry'}").mode,
        "hybrid"
    );
    assert_eq!(
        context(
            &db,
            "team meeting panic",
            "{label: 'Entry', mode: 'vector'}"
        )
        .mode,
        "vector"
    );
}

#[test]
fn every_format_and_tokenizer_stays_within_budget() {
    let db = with_embedder();
    for format in ["markdown", "triples", "json"] {
        for tokenizer in ["approx", "words"] {
            for budget in [50, 80, 2000] {
                let ctx = context(
                    &db,
                    "team meeting panic",
                    &format!(
                        "{{label: 'Entry', format: '{format}', tokenizer: '{tokenizer}', tokenBudget: {budget}}}"
                    ),
                );
                assert_eq!(ctx.format, format);
                assert!(
                    ctx.tokens <= budget,
                    "{format}/{tokenizer}/{budget}: {ctx:?}"
                );
                let counted = match tokenizer {
                    "approx" => drevo::rag::tokens::approx(&ctx.text),
                    _ => drevo::rag::tokens::words(&ctx.text),
                };
                assert_eq!(ctx.tokens, counted as i64, "{format}/{tokenizer}/{budget}");
                if budget == 50 && tokenizer == "approx" {
                    assert!(ctx.truncated, "{format}/{tokenizer}: {ctx:?}");
                }
                if format == "json" && !ctx.text.is_empty() {
                    let parsed: serde_json::Value = serde_json::from_str(&ctx.text).unwrap();
                    assert!(parsed["nodes"].is_array());
                }
            }
        }
    }
}

#[test]
fn the_same_question_gives_the_same_bytes_even_after_an_unrelated_write() {
    let db = with_embedder();
    let config = "{label: 'Entry', format: 'json'}";
    let first = context(&db, "team meeting panic", config);
    assert_eq!(context(&db, "team meeting panic", config), first);
    run(&db, "CREATE (:Note {title: 'grocery list: apples, rice'})");
    assert_eq!(context(&db, "team meeting panic", config), first);
}

#[test]
fn context_writes_nothing() {
    let db = with_embedder();
    let before = node_count(&db);
    let ctx = context(&db, "team meeting panic", "{label: 'Entry'}");
    assert_eq!(ctx.trace_id, Value::Null);
    assert_eq!(node_count(&db), before);
    // A session records a trace (tests/rag_trace_tests.rs).
    let ctx = context(
        &db,
        "team meeting panic",
        "{label: 'Entry', session: 'chat-1'}",
    );
    assert!(matches!(ctx.trace_id, Value::String(_)), "{ctx:?}");
}

#[test]
fn a_small_max_body_chars_cuts_bodies_but_not_names() {
    let db = with_embedder();
    let ctx = context(
        &db,
        "team meeting panic",
        "{label: 'Entry', maxBodyChars: 5}",
    );
    let (mila, monday, wrote) = mila_wrote_monday(&db);
    for line in [
        format!("- Monday shift (kind=Entry, id={monday}): Panic…\n"),
        format!("- Mila (kind=Person, id={mila}): Keeps…\n"),
        format!("- {mila} -[WROTE]-> {monday} (id={wrote}, weight=1.0)\n"),
    ] {
        assert!(ctx.text.contains(&line), "{line}{}", ctx.text);
    }
    assert!(
        ctx.text.contains("- Mind reading (kind=Distortion, id="),
        "{}",
        ctx.text
    );
    assert!(ctx.text.contains("): Assum…\n"), "{}", ctx.text);
}

#[test]
fn a_tight_budget_keeps_the_seeds_and_flags_the_cut() {
    let db = with_embedder();
    let ctx = context(
        &db,
        "team meeting panic",
        "{label: 'Entry', k: 1, tokenBudget: 50}",
    );
    assert!(ctx.truncated);
    assert!(ctx.tokens <= 50);
    let monday =
        run(&db, "MATCH (e:Entry {title: 'Monday shift'}) RETURN id(e)").rows[0][0].clone();
    let Value::Integer(monday) = monday else {
        panic!("id: {monday:?}");
    };
    assert_eq!(ctx.node_ids.first(), Some(&monday), "{ctx:?}");
}

// ── errors ──────────────────────────────────────────────────────────────────

#[test]
fn every_error_is_named() {
    let db = with_embedder();
    let cases = [
        (
            "CALL drevo.rag.context('panic', {tokenBudget: 10})",
            "tokenBudget must be an integer ≥ 50, got 10",
        ),
        (
            "CALL drevo.rag.context('panic', {depth: 2})",
            "unknown config key `depth`; expected label, embeddingProperty, k",
        ),
        (
            "CALL drevo.rag.context('panic', {format: 'yaml'})",
            "format must be 'markdown', 'triples' or 'json', got 'yaml'",
        ),
        (
            "CALL drevo.rag.context(42)",
            "query must be a string, got Integer",
        ),
        (
            "CALL drevo.rag.retrieve(null)",
            "query must be a string, got Null",
        ),
        (
            "CALL drevo.rag.retrieve('panic', 'fast')",
            "the config must be a map, got String",
        ),
        (
            "CALL drevo.rag.context('panic', {session: 3})",
            "session must be a string or null, got 3",
        ),
        (
            "CALL drevo.rag.retrieve('the', {maxNodes: 1, k: 3})",
            "maxNodes 1 is less than the",
        ),
        (
            "CALL drevo.rag.context('panic', {mode: 'vector'})",
            "mode 'vector' needs a label to rank by embedding; pass one or use mode 'fts'",
        ),
    ];
    for (source, expected) in cases {
        let message = invalid_call(&db, source);
        assert!(
            message.starts_with(expected),
            "{source}: expected {expected:?}, got {message:?}"
        );
    }
}

#[test]
fn an_explicit_vector_mode_without_an_embedder_is_a_capability_error() {
    let db = without_embedder();
    let err = try_run(
        &db,
        "CALL drevo.rag.context('panic', {label: 'Entry', mode: 'hybrid'})",
    )
    .unwrap_err();
    assert!(matches!(err, ExecError::EngineCapability { .. }), "{err:?}");
}

#[test]
fn the_procedures_take_one_or_two_arguments() {
    let db = with_embedder();
    for source in [
        "CALL drevo.rag.context()",
        "CALL drevo.rag.context('a', {}, 3)",
        "CALL drevo.rag.retrieve()",
        "CALL drevo.rag.retrieve('a', {}, 3)",
    ] {
        let err = try_run(&db, source).unwrap_err();
        assert!(
            err.to_string().contains("expected 1 or 2 arguments"),
            "{source}: {err}"
        );
    }
    // One argument uses the defaults.
    run(&db, "CALL drevo.rag.context('panic')");
    run(&db, "CALL drevo.rag.retrieve('panic')");
}

/// A topic with `n` nights hanging off it: a retrieval large enough that a
/// debug build takes well over a millisecond to expand, rank and write it.
fn star(n: usize) -> NativeService {
    let db = NativeService::in_memory();
    run(
        &db,
        &format!(
            "CREATE (t:Topic {{name: 'insomnia'}}) WITH t \
             UNWIND range(1, {n}) AS i CREATE (t)-[:HAS]->(:Night {{title: 'night ' + toString(i)}})"
        ),
    );
    db
}

const STAR_CONTEXT: &str =
    "CALL drevo.rag.context('insomnia', {perNodeLimit: 5000, maxNodes: 5000, hops: 1})";

#[test]
fn the_statement_timeout_stops_the_pipeline() {
    let db = star(3000);
    try_run(&db, STAR_CONTEXT).unwrap();
    db.set_statement_timeout(Some(Duration::from_millis(1)));
    // A quick statement still runs under the same limit.
    try_run(&db, "RETURN 1").unwrap();
    let err = try_run(&db, STAR_CONTEXT).unwrap_err();
    assert!(matches!(err, ExecError::Timeout { limit_ms: 1 }), "{err:?}");
    db.set_statement_timeout(None);
    try_run(&db, STAR_CONTEXT).unwrap();
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
            _ => None,
        })
        .collect()
}

#[test]
fn bolt_round_trip_types() {
    let db = Arc::new(with_embedder());
    let rows = bolt_records(
        &db,
        "CALL drevo.rag.retrieve('team meeting panic', {label: 'Entry'}) \
         YIELD node, seedScore, distance, pprScore, rank, via, truncated \
         RETURN node, seedScore, distance, pprScore, rank, via, truncated",
    );
    assert!(rows.len() >= 2, "{rows:?}");
    let seed = &rows[0];
    assert!(matches!(seed[0], Bolt::Structure { .. }), "{:?}", seed[0]);
    assert!(matches!(seed[1], Bolt::Float(_)), "{:?}", seed[1]);
    assert_eq!(seed[2], Bolt::Integer(0));
    assert!(matches!(seed[3], Bolt::Float(_)), "{:?}", seed[3]);
    assert_eq!(seed[4], Bolt::Integer(1));
    assert_eq!(seed[5], Bolt::Null);
    assert_eq!(seed[6], Bolt::Boolean(false));
    let reached = rows.last().unwrap();
    assert_eq!(reached[1], Bolt::Null);
    assert!(
        matches!(reached[5], Bolt::Dictionary(_)),
        "{:?}",
        reached[5]
    );

    let rows = bolt_records(
        &db,
        "CALL drevo.rag.context('team meeting panic', {label: 'Entry'}) \
         YIELD text, tokens, format, nodeIds, relIds, truncated, mode, traceId \
         RETURN text, tokens, format, nodeIds, relIds, truncated, mode, traceId",
    );
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert!(
        matches!(&row[0], Bolt::String(t) if t.starts_with("## Seeds\n")),
        "{:?}",
        row[0]
    );
    assert!(matches!(row[1], Bolt::Integer(t) if t > 0), "{:?}", row[1]);
    assert_eq!(row[2], Bolt::String("markdown".to_string()));
    assert!(
        matches!(&row[3], Bolt::List(ids) if ids.iter().all(|i| matches!(i, Bolt::Integer(_)))),
        "{:?}",
        row[3]
    );
    assert!(matches!(&row[4], Bolt::List(_)), "{:?}", row[4]);
    assert_eq!(row[5], Bolt::Boolean(false));
    assert_eq!(row[6], Bolt::String("hybrid".to_string()));
    assert_eq!(row[7], Bolt::Null);
}
