//! `CALL drevo.rag.evaluate(questions[, config]) YIELD question, strategy,
//! hit, recall, precision, mrr, tokens, durationMs, nodeIds, recallRetrieved`
//! and `CALL drevo.rag.evaluateSummary(questions[, config]) YIELD strategy,
//! questions, hitRate, meanRecall, meanPrecision, mrr, p50Ms, p95Ms,
//! meanTokens`: run each retrieval strategy over a gold set with one token
//! budget and score the context it would hand to the model.
//!
//! The fixture is a bug tracker. The answer to "login fails on iOS"
//! needs the ticket (search finds it), its crash log and the commit that
//! fixes it — which share no words with the question and carry no
//! embedding, so only a walk from the ticket reaches them. Four comments
//! hang off the ticket too; they sort before the fix by id, so a
//! nearest-first ranking spends the budget on them, while Personalized
//! PageRank ranks the crash log and the fix (linked to each other as well
//! as to the ticket) above them.
//!
//! The embedder is an in-process stub: text about login or iOS points at
//! `[1, 0, 0]`, anything else at `[0, 0, 1]`.

#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

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
        Ok(if text.contains("login") || text.contains("ios") {
            vec![1.0, 0.0, 0.0]
        } else {
            vec![0.0, 0.0, 1.0]
        })
    }
}

const QUESTION: &str = "login fails on iOS";

/// Fits the ticket and two more nodes, not three: the ticket's block plus
/// the crash log's and the fix's (with their triples) under the default
/// markdown format.
const BUDGET: i64 = 120;

fn try_call(
    db: &NativeService,
    source: &str,
    params: &[(&str, Value)],
) -> Result<ExecResult, ExecError> {
    let q = parse(source).unwrap_or_else(|e| panic!("parse `{source}`: {e}"));
    let params: HashMap<String, Value> = params
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect();
    db.execute(&q, params)
}

fn run(db: &NativeService, source: &str) -> ExecResult {
    try_call(db, source, &[]).unwrap_or_else(|e| panic!("`{source}`: {e}"))
}

fn s(text: &str) -> Value {
    Value::String(text.to_string())
}

fn map(entries: &[(&str, Value)]) -> Value {
    Value::Map(
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn list(items: Vec<Value>) -> Value {
    Value::List(items)
}

fn named(name: &str, label: &str) -> Value {
    map(&[("name", s(name)), ("label", s(label))])
}

fn bug_tracker(db: &NativeService) {
    run(
        db,
        "CREATE (t:Ticket {name: 'Login fails on iOS 17', description: 'Users cannot log in after the iOS update', embedding: [1.0, 0.0, 0.0]}), \
                (c:CrashLog {name: 'EXC_BAD_ACCESS crash', description: 'KeychainStore.read on launch'}), \
                (c1:Comment {name: 'Comment one', description: 'Seen on my phone too'}), \
                (c2:Comment {name: 'Comment two', description: 'Same here since Monday'}), \
                (c3:Comment {name: 'Comment three', description: 'Any news on this one'}), \
                (c4:Comment {name: 'Comment four', description: 'Bumping for visibility'}), \
                (f:Commit {name: 'Guard nil keychain item', description: 'Check the item before decoding'}), \
                (a:Ticket {name: 'Dark mode colours wrong on Android', description: 'Buttons unreadable', embedding: [0.0, 1.0, 0.0]}), \
                (t)-[:HAS_CRASH]->(c), (f)-[:FIXES]->(t), (f)-[:FIXES_CRASH]->(c), \
                (c1)-[:ON]->(t), (c2)-[:ON]->(t), (c3)-[:ON]->(t), (c4)-[:ON]->(t)",
    );
}

fn with_embedder() -> NativeService {
    let db = NativeService::in_memory();
    assert!(db.set_embedder(Arc::new(StubEmbedder)));
    bug_tracker(&db);
    db
}

fn without_embedder() -> NativeService {
    let db = NativeService::in_memory();
    bug_tracker(&db);
    db
}

/// The gold set: the ticket, its crash log and the fix.
fn gold() -> Value {
    list(vec![map(&[
        ("query", s(QUESTION)),
        (
            "expected",
            list(vec![
                named("Login fails on iOS 17", "Ticket"),
                named("EXC_BAD_ACCESS crash", "CrashLog"),
                named("Guard nil keychain item", "Commit"),
            ]),
        ),
    ])])
}

/// The shared knobs every comparison here runs with.
fn config(strategies: &[&str]) -> Value {
    map(&[
        (
            "strategies",
            list(strategies.iter().map(|n| s(n)).collect()),
        ),
        ("label", s("Ticket")),
        ("k", Value::Integer(1)),
        ("tokenBudget", Value::Integer(BUDGET)),
    ])
}

fn id_of(db: &NativeService, name: &str) -> i64 {
    let rows = try_call(
        db,
        "MATCH (n) WHERE n.name = $name RETURN id(n)",
        &[("name", s(name))],
    )
    .unwrap()
    .rows;
    int(&rows[0][0])
}

fn expected_ids(db: &NativeService) -> HashSet<i64> {
    [
        "Login fails on iOS 17",
        "EXC_BAD_ACCESS crash",
        "Guard nil keychain item",
    ]
    .iter()
    .map(|n| id_of(db, n))
    .collect()
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn float(v: &Value) -> f64 {
    match v {
        Value::Float(f) => *f,
        other => panic!("expected a float, got {other:?}"),
    }
}

fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    }
}

fn ints(v: &Value) -> Vec<i64> {
    match v {
        Value::List(items) => items.iter().map(int).collect(),
        other => panic!("expected a list, got {other:?}"),
    }
}

/// One `drevo.rag.evaluate` row.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    question: String,
    strategy: String,
    hit: bool,
    recall: f64,
    precision: f64,
    mrr: f64,
    tokens: i64,
    duration_ms: i64,
    node_ids: Vec<i64>,
    recall_retrieved: f64,
}

const EVALUATE: &str = "CALL drevo.rag.evaluate($questions, $config) \
     YIELD question, strategy, hit, recall, precision, mrr, tokens, durationMs, nodeIds, \
     recallRetrieved \
     RETURN question, strategy, hit, recall, precision, mrr, tokens, durationMs, nodeIds, \
     recallRetrieved";

const SUMMARY: &str = "CALL drevo.rag.evaluateSummary($questions, $config) \
     YIELD strategy, questions, hitRate, meanRecall, meanPrecision, mrr, p50Ms, p95Ms, \
     meanTokens \
     RETURN strategy, questions, hitRate, meanRecall, meanPrecision, mrr, p50Ms, p95Ms, \
     meanTokens";

fn try_evaluate(
    db: &NativeService,
    questions: Value,
    config: Value,
) -> Result<Vec<Row>, ExecError> {
    let result = try_call(
        db,
        EVALUATE,
        &[("questions", questions), ("config", config)],
    )?;
    Ok(result
        .rows
        .iter()
        .map(|r| Row {
            question: string(&r[0]),
            strategy: string(&r[1]),
            hit: matches!(r[2], Value::Bool(true)),
            recall: float(&r[3]),
            precision: float(&r[4]),
            mrr: float(&r[5]),
            tokens: int(&r[6]),
            duration_ms: int(&r[7]),
            node_ids: ints(&r[8]),
            recall_retrieved: float(&r[9]),
        })
        .collect())
}

fn evaluate(db: &NativeService, questions: Value, config: Value) -> Vec<Row> {
    try_evaluate(db, questions, config).unwrap_or_else(|e| panic!("evaluate: {e}"))
}

/// One `drevo.rag.evaluateSummary` row.
#[derive(Debug, Clone, PartialEq)]
struct Summary {
    strategy: String,
    questions: i64,
    hit_rate: f64,
    mean_recall: f64,
    mean_precision: f64,
    mrr: f64,
    p50_ms: f64,
    p95_ms: f64,
    mean_tokens: f64,
}

fn summary(db: &NativeService, questions: Value, config: Value) -> Vec<Summary> {
    let result = try_call(db, SUMMARY, &[("questions", questions), ("config", config)])
        .unwrap_or_else(|e| panic!("summary: {e}"));
    result
        .rows
        .iter()
        .map(|r| Summary {
            strategy: string(&r[0]),
            questions: int(&r[1]),
            hit_rate: float(&r[2]),
            mean_recall: float(&r[3]),
            mean_precision: float(&r[4]),
            mrr: float(&r[5]),
            p50_ms: float(&r[6]),
            p95_ms: float(&r[7]),
            mean_tokens: float(&r[8]),
        })
        .collect()
}

fn by_strategy<'r>(rows: &'r [Row], strategy: &str) -> &'r Row {
    rows.iter()
        .find(|r| r.strategy == strategy)
        .unwrap_or_else(|| panic!("no {strategy} row in {rows:?}"))
}

/// The `InvalidProcedureCall` message a call fails with.
fn invalid(result: Result<Vec<Row>, ExecError>) -> String {
    match result {
        Err(ExecError::InvalidProcedureCall { name, message, .. }) => {
            assert_eq!(name, "drevo.rag.evaluate");
            message
        }
        other => panic!("expected InvalidProcedureCall, got {other:?}"),
    }
}

// ── The point of the exercise ───────────────────────────────────────────────

#[test]
fn the_graph_finds_what_plain_vector_search_misses() {
    let db = with_embedder();
    let rows = evaluate(
        &db,
        gold(),
        config(&["vector", "fts", "hybrid", "hybrid+expand", "rag"]),
    );
    assert_eq!(rows.len(), 5, "{rows:?}");
    let recall = |strategy: &str| by_strategy(&rows, strategy).recall;
    // Search alone finds the ticket only.
    for strategy in ["vector", "fts", "hybrid"] {
        assert_eq!(recall(strategy), 1.0 / 3.0, "{strategy}: {rows:?}");
    }
    // Nearest-first expansion reaches the crash log but spends the rest of
    // the budget on comments; PageRank ranks the fix above them.
    assert!(recall("vector") < recall("hybrid+expand"), "{rows:?}");
    assert!(recall("hybrid+expand") < recall("rag"), "{rows:?}");
    assert_eq!(recall("hybrid+expand"), 2.0 / 3.0, "{rows:?}");
    assert_eq!(recall("rag"), 1.0, "{rows:?}");
    // Both expansions retrieved all three; only the budget cut the fix.
    assert_eq!(by_strategy(&rows, "hybrid+expand").recall_retrieved, 1.0);
    assert_eq!(by_strategy(&rows, "rag").recall_retrieved, 1.0);
    assert_eq!(by_strategy(&rows, "vector").recall_retrieved, 1.0 / 3.0);
}

#[test]
fn every_row_is_consistent_with_its_node_ids() {
    let db = with_embedder();
    let expected = expected_ids(&db);
    let rows = evaluate(
        &db,
        gold(),
        config(&["vector", "fts", "hybrid", "hybrid+expand", "rag"]),
    );
    for row in &rows {
        assert_eq!(row.question, QUESTION);
        assert!(row.tokens > 0 && row.tokens <= BUDGET, "{row:?}");
        assert!(row.recall_retrieved >= row.recall, "{row:?}");
        assert!(row.duration_ms >= 0, "{row:?}");
        let found = row
            .node_ids
            .iter()
            .filter(|id| expected.contains(id))
            .count();
        assert_eq!(row.hit, found > 0, "{row:?}");
        assert_eq!(row.recall, found as f64 / 3.0, "{row:?}");
        assert_eq!(
            row.precision,
            found as f64 / row.node_ids.len() as f64,
            "{row:?}"
        );
        let first = row
            .node_ids
            .iter()
            .position(|id| expected.contains(id))
            .map_or(0.0, |i| 1.0 / (i + 1) as f64);
        assert_eq!(row.mrr, first, "{row:?}");
    }
    // The ticket is the first thing every strategy hands over.
    let ticket = id_of(&db, "Login fails on iOS 17");
    assert!(rows.iter().all(|r| r.node_ids[0] == ticket), "{rows:?}");
}

#[test]
fn node_ids_and_names_are_the_same_gold() {
    let db = with_embedder();
    let ids = list(
        [
            "Login fails on iOS 17",
            "EXC_BAD_ACCESS crash",
            "Guard nil keychain item",
        ]
        .iter()
        .map(|n| Value::Integer(id_of(&db, n)))
        .collect(),
    );
    let by_id = evaluate(
        &db,
        list(vec![map(&[("query", s(QUESTION)), ("expected", ids)])]),
        config(&["rag", "vector"]),
    );
    let by_name = evaluate(&db, gold(), config(&["rag", "vector"]));
    let scores = |rows: &[Row]| {
        rows.iter()
            .map(|r| {
                (
                    r.strategy.clone(),
                    r.recall,
                    r.precision,
                    r.mrr,
                    r.node_ids.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(scores(&by_id), scores(&by_name));
}

#[test]
fn rows_come_per_question_in_the_strategies_order() {
    let db = with_embedder();
    let questions = list(vec![
        map(&[
            ("query", s(QUESTION)),
            (
                "expected",
                list(vec![named("Login fails on iOS 17", "Ticket")]),
            ),
        ]),
        map(&[
            ("query", s("dark mode")),
            (
                "expected",
                list(vec![named("Dark mode colours wrong on Android", "Ticket")]),
            ),
        ]),
    ]);
    let rows = evaluate(&db, questions, config(&["rag", "fts"]));
    let order: Vec<(&str, &str)> = rows
        .iter()
        .map(|r| (r.question.as_str(), r.strategy.as_str()))
        .collect();
    assert_eq!(
        order,
        [
            (QUESTION, "rag"),
            (QUESTION, "fts"),
            ("dark mode", "rag"),
            ("dark mode", "fts"),
        ]
    );
    assert!(rows.iter().all(|r| r.hit), "{rows:?}");
}

#[test]
fn the_default_strategies_are_those_this_server_can_run() {
    let db = with_embedder();
    let knobs = map(&[("label", s("Ticket")), ("k", Value::Integer(1))]);
    let strategies = |rows: Vec<Row>| rows.into_iter().map(|r| r.strategy).collect::<Vec<_>>();
    assert_eq!(
        strategies(evaluate(&db, gold(), knobs.clone())),
        ["vector", "fts", "hybrid", "hybrid+expand", "rag"]
    );
    // Without a label there is nothing for 'vector' to rank.
    assert_eq!(
        strategies(evaluate(&db, gold(), Value::Null)),
        ["fts", "hybrid", "hybrid+expand", "rag"]
    );
    // Without an embedder neither.
    assert_eq!(
        strategies(evaluate(&without_embedder(), gold(), knobs)),
        ["fts", "hybrid", "hybrid+expand", "rag"]
    );
}

#[test]
fn without_an_embedder_the_graph_strategies_still_run_on_bm25() {
    let db = without_embedder();
    let rows = evaluate(&db, gold(), config(&["fts", "hybrid+expand", "rag"]));
    assert_eq!(by_strategy(&rows, "fts").recall, 1.0 / 3.0);
    assert_eq!(by_strategy(&rows, "rag").recall, 1.0);
}

#[test]
fn no_questions_give_no_rows() {
    let db = with_embedder();
    assert!(evaluate(&db, list(Vec::new()), Value::Null).is_empty());
    assert!(summary(&db, list(Vec::new()), Value::Null).is_empty());
}

#[test]
fn evaluating_writes_nothing() {
    let db = with_embedder();
    let count = |db: &NativeService| int(&run(db, "MATCH (n) RETURN count(n)").rows[0][0]);
    let before = count(&db);
    evaluate(
        &db,
        gold(),
        config(&["vector", "fts", "hybrid", "hybrid+expand", "rag"]),
    );
    summary(&db, gold(), config(&["rag"]));
    assert_eq!(count(&db), before);
    assert_eq!(
        int(&run(&db, "MATCH (t:ContextTrace) RETURN count(t)").rows[0][0]),
        0
    );
}

// ── Questions scored against a stored trace ────────────────────────────────

/// Record a context for the question with the comparison's knobs and return
/// its trace id.
fn record_trace(db: &NativeService, budget: i64) -> String {
    let result = try_call(
        db,
        "CALL drevo.rag.context($q, $config) YIELD traceId RETURN traceId",
        &[
            ("q", s(QUESTION)),
            (
                "config",
                map(&[
                    ("label", s("Ticket")),
                    ("k", Value::Integer(1)),
                    ("tokenBudget", Value::Integer(budget)),
                    ("session", s("triage-1")),
                ]),
            ),
        ],
    )
    .unwrap();
    string(&result.rows[0][0])
}

#[test]
fn a_trace_question_is_scored_against_what_the_trace_included() {
    let db = with_embedder();
    let id = record_trace(&db, BUDGET);
    // The same config again includes exactly what the trace did.
    let rows = evaluate(
        &db,
        list(vec![map(&[("query", s(QUESTION)), ("traceId", s(&id))])]),
        config(&["rag", "hybrid+expand", "vector"]),
    );
    let rag = by_strategy(&rows, "rag");
    assert_eq!(
        (rag.recall, rag.precision, rag.mrr),
        (1.0, 1.0, 1.0),
        "{rows:?}"
    );
    // The traced context held three nodes; nearest-first keeps two of them,
    // vector search one.
    assert_eq!(by_strategy(&rows, "hybrid+expand").recall, 2.0 / 3.0);
    assert_eq!(by_strategy(&rows, "vector").recall, 1.0 / 3.0);
    // Without a query the trace's own question is asked.
    let rows = evaluate(
        &db,
        list(vec![map(&[("traceId", s(&id))])]),
        config(&["rag"]),
    );
    assert_eq!(rows[0].question, QUESTION);
    assert_eq!(rows[0].recall, 1.0);
}

#[test]
fn a_bigger_budget_than_the_trace_had_lowers_precision_not_recall() {
    let db = with_embedder();
    let id = record_trace(&db, BUDGET);
    let rows = evaluate(
        &db,
        list(vec![map(&[("traceId", s(&id))])]),
        map(&[
            ("strategies", list(vec![s("rag")])),
            ("label", s("Ticket")),
            ("k", Value::Integer(1)),
            ("tokenBudget", Value::Integer(4000)),
        ]),
    );
    assert_eq!(rows[0].recall, 1.0);
    assert!(rows[0].precision < 1.0, "{rows:?}");
}

// ── evaluateSummary ─────────────────────────────────────────────────────────

/// `n` questions: the bug-tracker one and the Android one, alternating.
fn questions(n: usize) -> Value {
    list(
        (0..n)
            .map(|i| {
                if i % 2 == 0 {
                    gold().as_list()[0].clone()
                } else {
                    map(&[
                        ("query", s("dark mode")),
                        (
                            "expected",
                            list(vec![named("Dark mode colours wrong on Android", "Ticket")]),
                        ),
                    ])
                }
            })
            .collect(),
    )
}

trait AsList {
    fn as_list(&self) -> &[Value];
}

impl AsList for Value {
    fn as_list(&self) -> &[Value] {
        match self {
            Value::List(items) => items,
            other => panic!("expected a list, got {other:?}"),
        }
    }
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let v: Vec<f64> = values.collect();
    v.iter().sum::<f64>() / v.len() as f64
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn the_summary_averages_the_rows_for_odd_and_even_counts() {
    let db = with_embedder();
    for n in [3, 4] {
        let strategies = ["vector", "hybrid+expand", "rag"];
        let rows = evaluate(&db, questions(n), config(&strategies));
        let summaries = summary(&db, questions(n), config(&strategies));
        assert_eq!(
            summaries
                .iter()
                .map(|s| s.strategy.as_str())
                .collect::<Vec<_>>(),
            strategies
        );
        for sum in &summaries {
            let mine: Vec<&Row> = rows.iter().filter(|r| r.strategy == sum.strategy).collect();
            assert_eq!(sum.questions, n as i64);
            let hits = mine.iter().map(|r| if r.hit { 1.0 } else { 0.0 });
            assert!(close(sum.hit_rate, mean(hits)), "{sum:?}");
            assert!(
                close(sum.mean_recall, mean(mine.iter().map(|r| r.recall))),
                "{sum:?}"
            );
            assert!(
                close(sum.mean_precision, mean(mine.iter().map(|r| r.precision))),
                "{sum:?}"
            );
            assert!(close(sum.mrr, mean(mine.iter().map(|r| r.mrr))), "{sum:?}");
            assert!(
                close(sum.mean_tokens, mean(mine.iter().map(|r| r.tokens as f64))),
                "{sum:?}"
            );
            // Timings differ between the two calls; their shape does not.
            assert!(sum.p50_ms >= 0.0 && sum.p50_ms <= sum.p95_ms, "{sum:?}");
        }
    }
}

// ── Errors ──────────────────────────────────────────────────────────────────

#[test]
fn unresolved_expected_nodes_are_listed() {
    let db = with_embedder();
    let questions = list(vec![
        map(&[
            ("query", s(QUESTION)),
            (
                "expected",
                list(vec![
                    named("Login fails on iOS 17", "Ticket"),
                    named("Login fails on iOS 17", "Commit"),
                    Value::Integer(987_654),
                ]),
            ),
        ]),
        map(&[
            ("query", s("x")),
            ("expected", list(vec![map(&[("name", s("Nobody"))])])),
        ]),
    ]);
    assert_eq!(
        invalid(try_evaluate(&db, questions, config(&["rag"]))),
        "3 expected nodes not found: questions[0].expected[1] {name: 'Login fails on iOS 17', \
         label: 'Commit'}, questions[0].expected[2] id 987654, questions[1].expected[0] \
         {name: 'Nobody'}"
    );
}

#[test]
fn an_ambiguous_name_is_refused() {
    let db = with_embedder();
    run(&db, "CREATE (:Commit {name: 'Comment one'})");
    let comment_one = run(
        &db,
        "MATCH (n) WHERE n.name = 'Comment one' RETURN id(n) ORDER BY id(n)",
    )
    .rows
    .iter()
    .map(|r| int(&r[0]).to_string())
    .collect::<Vec<_>>()
    .join(", ");
    let questions = list(vec![map(&[
        ("query", s(QUESTION)),
        ("expected", list(vec![map(&[("name", s("Comment one"))])])),
    ])]);
    assert_eq!(
        invalid(try_evaluate(&db, questions, config(&["rag"]))),
        format!(
            "questions[0].expected[0] {{name: 'Comment one'}} matches 2 nodes (ids \
             {comment_one}); add a label"
        )
    );
    // A label tells them apart.
    let questions = list(vec![map(&[
        ("query", s(QUESTION)),
        ("expected", list(vec![named("Comment one", "Comment")])),
    ])]);
    assert!(try_evaluate(&db, questions, config(&["rag"])).is_ok());
}

#[test]
fn a_node_with_only_a_title_resolves_like_a_name() {
    let db = with_embedder();
    run(
        &db,
        "MATCH (t:Ticket) WHERE t.name = 'Login fails on iOS 17' \
         CREATE (:Commit {title: 'Rotate keychain key'})-[:FIXES]->(t)",
    );
    let rows = evaluate(
        &db,
        list(vec![map(&[
            ("query", s("Rotate keychain key")),
            (
                "expected",
                list(vec![named("Rotate keychain key", "Commit")]),
            ),
        ])]),
        map(&[("strategies", list(vec![s("fts")]))]),
    );
    let commit = int(&run(
        &db,
        "MATCH (c:Commit) WHERE c.title = 'Rotate keychain key' RETURN id(c)",
    )
    .rows[0][0]);
    assert_eq!(rows[0].node_ids[0], commit, "{rows:?}");
    assert!(rows[0].hit && rows[0].recall == 1.0, "{rows:?}");
}

#[test]
fn a_name_on_one_node_and_a_title_on_another_is_ambiguous() {
    let db = with_embedder();
    // 'Comment one' is the `name` of a comment; now also the `title` of
    // another.
    run(&db, "CREATE (:Comment {title: 'Comment one'})");
    let ids = run(
        &db,
        "MATCH (n:Comment) WHERE n.name = 'Comment one' OR n.title = 'Comment one' \
         RETURN id(n) ORDER BY id(n)",
    )
    .rows
    .iter()
    .map(|r| int(&r[0]).to_string())
    .collect::<Vec<_>>()
    .join(", ");
    let questions = list(vec![map(&[
        ("query", s(QUESTION)),
        ("expected", list(vec![named("Comment one", "Comment")])),
    ])]);
    assert_eq!(
        invalid(try_evaluate(&db, questions, config(&["rag"]))),
        format!(
            "questions[0].expected[0] {{name: 'Comment one', label: 'Comment'}} matches 2 \
             nodes (ids {ids}); add a label"
        )
    );
    // One node whose name and title are both the name is one match.
    run(&db, "CREATE (:Release {name: 'v2.1', title: 'v2.1'})");
    let questions = list(vec![map(&[
        ("query", s(QUESTION)),
        ("expected", list(vec![named("v2.1", "Release")])),
    ])]);
    assert!(try_evaluate(&db, questions, config(&["rag"])).is_ok());
}

#[test]
fn every_strategy_error_is_named() {
    let db = with_embedder();
    assert_eq!(
        invalid(try_evaluate(&db, gold(), config(&["graph"]))),
        "unknown strategy 'graph'; expected 'vector', 'fts', 'hybrid', 'hybrid+expand', \
         'rag' or 'rag-global'"
    );
    assert_eq!(
        invalid(try_evaluate(&db, gold(), config(&["rag", "rag-global"]))),
        "strategy 'rag-global' needs community summaries, which this server does not \
         build; leave it out of `strategies`"
    );
    assert_eq!(
        invalid(try_evaluate(
            &db,
            gold(),
            map(&[("strategies", list(vec![s("vector")]))])
        )),
        "strategy 'vector' needs a `label` whose `embeddingProperty` vectors it ranks"
    );
    assert_eq!(
        invalid(try_evaluate(
            &without_embedder(),
            gold(),
            config(&["fts", "vector"])
        )),
        "strategy 'vector' needs a server-side embedder (DREVO_EMBEDDINGS_UPSTREAM), and \
         this server has none; leave it out of `strategies`"
    );
}

#[test]
fn every_config_and_question_error_is_named() {
    let db = with_embedder();
    assert_eq!(
        invalid(try_evaluate(
            &db,
            gold(),
            map(&[("tokenBudget", Value::Integer(10))])
        )),
        "tokenBudget must be an integer ≥ 50, got 10"
    );
    assert_eq!(
        invalid(try_evaluate(&db, gold(), map(&[("session", s("chat-1"))]))),
        "an evaluation never records a context trace; remove `session`"
    );
    assert_eq!(
        invalid(try_evaluate(&db, s("why"), Value::Null)),
        "questions must be a list of maps, got String"
    );
    assert_eq!(
        invalid(try_evaluate(
            &db,
            list(vec![map(&[("traceId", s("no-such-trace"))])]),
            Value::Null
        )),
        "questions[0]: no context trace with id `no-such-trace`"
    );
}

#[test]
fn a_trace_without_a_question_or_included_nodes_is_refused() {
    let db = with_embedder();
    // A context for a question nothing matches includes nothing.
    let result = run(
        &db,
        "CALL drevo.rag.context('zzqqxx', {trace: true}) YIELD traceId RETURN traceId",
    );
    let empty = string(&result.rows[0][0]);
    assert_eq!(
        invalid(try_evaluate(
            &db,
            list(vec![map(&[("traceId", s(&empty))])]),
            Value::Null
        )),
        format!("questions[0]: context trace `{empty}` included no nodes")
    );
    // A trace written by hand, without its question, needs one given.
    run(
        &db,
        "MATCH (t:Ticket) WHERE t.name = 'Login fails on iOS 17' \
         CREATE (:ContextTrace {id: 'bare'})-[:INCLUDED]->(t)",
    );
    assert_eq!(
        invalid(try_evaluate(
            &db,
            list(vec![map(&[("traceId", s("bare"))])]),
            Value::Null
        )),
        "questions[0]: context trace `bare` has no query; give one"
    );
    let rows = evaluate(
        &db,
        list(vec![map(&[("query", s(QUESTION)), ("traceId", s("bare"))])]),
        config(&["fts"]),
    );
    assert_eq!(rows[0].recall, 1.0);
}

#[test]
fn the_procedures_take_one_or_two_arguments() {
    let db = with_embedder();
    for source in [
        "CALL drevo.rag.evaluate()",
        "CALL drevo.rag.evaluate([], {}, 3)",
        "CALL drevo.rag.evaluateSummary()",
        "CALL drevo.rag.evaluateSummary([], {}, 3)",
    ] {
        let err = try_call(&db, source, &[]).unwrap_err();
        assert!(
            err.to_string().contains("expected 1 or 2 arguments"),
            "{source}: {err}"
        );
    }
    run(&db, "CALL drevo.rag.evaluate([])");
    run(&db, "CALL drevo.rag.evaluateSummary([])");
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

const BOLT_QUESTIONS: &str = "[{query: 'login fails on iOS', \
     expected: [{name: 'Login fails on iOS 17', label: 'Ticket'}, \
     {name: 'EXC_BAD_ACCESS crash', label: 'CrashLog'}]}]";

#[test]
fn bolt_round_trip_types() {
    let db = Arc::new(with_embedder());
    let rows = bolt_records(
        &db,
        &format!(
            "CALL drevo.rag.evaluate({BOLT_QUESTIONS}, {{strategies: ['rag'], label: 'Ticket', k: 1}}) \
             YIELD question, strategy, hit, recall, precision, mrr, tokens, durationMs, nodeIds, \
             recallRetrieved \
             RETURN question, strategy, hit, recall, precision, mrr, tokens, durationMs, nodeIds, \
             recallRetrieved"
        ),
    );
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row[0], Bolt::String(QUESTION.to_string()));
    assert_eq!(row[1], Bolt::String("rag".to_string()));
    assert_eq!(row[2], Bolt::Boolean(true));
    assert_eq!(row[3], Bolt::Float(1.0));
    assert!(matches!(row[4], Bolt::Float(_)), "{:?}", row[4]);
    assert_eq!(row[5], Bolt::Float(1.0));
    assert!(matches!(row[6], Bolt::Integer(t) if t > 0), "{:?}", row[6]);
    assert!(matches!(row[7], Bolt::Integer(t) if t >= 0), "{:?}", row[7]);
    assert!(
        matches!(&row[8], Bolt::List(ids) if !ids.is_empty() && ids.iter().all(|i| matches!(i, Bolt::Integer(_)))),
        "{:?}",
        row[8]
    );
    assert_eq!(row[9], Bolt::Float(1.0));

    let rows = bolt_records(
        &db,
        &format!(
            "CALL drevo.rag.evaluateSummary({BOLT_QUESTIONS}, {{strategies: ['rag', 'fts'], label: 'Ticket', k: 1}}) \
             YIELD strategy, questions, hitRate, meanRecall, meanPrecision, mrr, p50Ms, p95Ms, \
             meanTokens \
             RETURN strategy, questions, hitRate, meanRecall, meanPrecision, mrr, p50Ms, p95Ms, \
             meanTokens"
        ),
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row[0], Bolt::String("rag".to_string()));
    assert_eq!(row[1], Bolt::Integer(1));
    assert_eq!(row[2], Bolt::Float(1.0));
    assert_eq!(row[3], Bolt::Float(1.0));
    for value in &row[4..] {
        assert!(matches!(value, Bolt::Float(_)), "{value:?}");
    }
    assert_eq!(rows[1][0], Bolt::String("fts".to_string()));
    assert_eq!(rows[1][3], Bolt::Float(0.5));
}

// ── rag-global over community summaries (#807) ──────────────────────────────

/// An IT task manager with two teams, densely linked inside and bridged
/// once (the fixture of `tests/community_tests.rs`): `drevo.community.summarize`
/// cuts it into a backend and a mobile community.
fn it_task_manager(db: &NativeService) {
    run(
        db,
        "CREATE (alice:Person {name: 'Alice', description: 'Backend lead'}), \
                (bob:Person {name: 'Bob', description: 'Backend engineer'}), \
                (carol:Person {name: 'Carol', description: 'Database engineer'}), \
                (be101:Ticket {title: 'BE-101 API rate limiter', body: 'Throttle the public API gateway'}), \
                (be142:Ticket {title: 'BE-142 Login token expires early', body: 'Session tokens on the auth server expire after five minutes'}), \
                (be150:Ticket {title: 'BE-150 Database migration', body: 'Move the orders table to the new server'}), \
                (dave:Person {name: 'Dave', description: 'Mobile lead'}), \
                (erin:Person {name: 'Erin', description: 'iOS developer'}), \
                (frank:Person {name: 'Frank', description: 'Android developer'}), \
                (mo7:Ticket {title: 'MO-7 iOS release build', body: 'Ship the mobile release to the app store'}), \
                (mo9:Ticket {title: 'MO-9 Android crash on startup', body: 'The mobile app crashes before the release screen'}), \
                (mo12:Ticket {title: 'MO-12 Mobile release notes', body: 'Write the mobile release notes for the app store'}), \
                (alice)-[:WORKS_ON]->(be101), (alice)-[:WORKS_ON]->(be142), (alice)-[:WORKS_ON]->(be150), \
                (bob)-[:WORKS_ON]->(be101), (bob)-[:WORKS_ON]->(be142), \
                (carol)-[:WORKS_ON]->(be150), (carol)-[:WORKS_ON]->(be142), \
                (alice)-[:MENTORS]->(bob), (alice)-[:MENTORS]->(carol), (be101)-[:BLOCKS]->(be150), \
                (dave)-[:WORKS_ON]->(mo7), (dave)-[:WORKS_ON]->(mo9), (dave)-[:WORKS_ON]->(mo12), \
                (erin)-[:WORKS_ON]->(mo7), (erin)-[:WORKS_ON]->(mo9), \
                (frank)-[:WORKS_ON]->(mo12), (frank)-[:WORKS_ON]->(mo9), \
                (dave)-[:MENTORS]->(erin), (dave)-[:MENTORS]->(frank), (mo9)-[:BLOCKS]->(mo7), \
                (mo9)-[:DEPENDS_ON]->(be142)",
    );
}

/// "What is the mobile team shipping?": the mobile release tickets.
fn mobile_gold() -> Value {
    list(vec![map(&[
        ("query", s("mobile release")),
        (
            "expected",
            list(vec![
                named("MO-7 iOS release build", "Ticket"),
                named("MO-12 Mobile release notes", "Ticket"),
            ]),
        ),
    ])])
}

fn global_config() -> Value {
    map(&[
        ("strategies", list(vec![s("rag"), s("rag-global")])),
        ("label", s("Ticket")),
        ("k", Value::Integer(2)),
        ("tokenBudget", Value::Integer(2000)),
    ])
}

#[test]
fn rag_global_strategy_runs_over_community_summaries() {
    let db = NativeService::in_memory();
    it_task_manager(&db);
    let summarized = run(
        &db,
        "CALL drevo.community.summarize({mode: 'extractive'}) YIELD community \
         RETURN count(community)",
    );
    assert!(
        int(&summarized.rows[0][0]) >= 2,
        "the fixture must have communities: {summarized:?}"
    );
    let rows = try_evaluate(&db, mobile_gold(), global_config()).unwrap_or_else(|e| {
        panic!(
            "#807: drevo.rag.evaluate refuses 'rag-global' although the graph has \
             summarised :Community nodes and drevo.rag.context supports scope 'global': {e}"
        )
    });
    assert_eq!(
        rows.iter().map(|r| r.strategy.as_str()).collect::<Vec<_>>(),
        ["rag", "rag-global"],
        "{rows:?}"
    );
    let global = by_strategy(&rows, "rag-global");
    assert!(
        (0.0..=1.0).contains(&global.recall) && global.recall > 0.0,
        "rag-global retrieves the mobile community's members, so it finds a mobile \
         release ticket: {global:?}"
    );
    assert!(!global.node_ids.is_empty(), "{global:?}");
}

#[test]
fn rag_global_without_communities_says_to_summarize_first() {
    let db = NativeService::in_memory();
    it_task_manager(&db);
    let message = invalid(try_evaluate(&db, mobile_gold(), global_config()));
    assert!(
        message.contains("drevo.community.summarize"),
        "#807: with no :Community nodes the 'rag-global' refusal must say to run \
         `drevo.community.summarize` first; got the stale message: {message}"
    );
}
