//! Full-text search and the server's own bookkeeping nodes (#819).
//!
//! A traced `drevo.rag.context` stores the question on a `:ContextTrace`, and
//! `drevo.ingest.submit` stores every chunk's text on an `:IngestItem`.
//! Full text indexes every string property, so `fts.search` returns those
//! nodes as hits: asking the same question twice finds the first question's
//! trace, and a submitted book is found once as its queue rows before it is
//! found as knowledge.

use std::collections::{BTreeMap, HashMap};

use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run_with(
    svc: &NativeService,
    q: &str,
    params: HashMap<String, Value>,
) -> Result<ExecResult, ExecError> {
    svc.execute(&parse(q).expect("parse"), params)
}

fn ok(svc: &NativeService, q: &str) -> ExecResult {
    run_with(svc, q, HashMap::new()).unwrap_or_else(|e| panic!("#819: `{q}` failed: {e:?}"))
}

/// The labels of every hit of `fts.search(term)`, sorted.
fn hit_labels(svc: &NativeService, term: &str) -> Vec<String> {
    let res = ok(
        svc,
        &format!("CALL fts.search('{term}', 20) YIELD node RETURN labels(node)[0] AS l ORDER BY l"),
    );
    res.rows
        .iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected a label, got {other:?}"),
        })
        .collect()
}

fn map(pairs: &[(&str, Value)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

#[test]
fn a_context_trace_is_not_a_full_text_hit() {
    let svc = NativeService::in_memory();
    ok(
        &svc,
        "CREATE (:Chapter {title: 'Prologue zqmarrow'})-[:NEXT]->\
         (:Chapter {title: 'Chapter one zqmarrow'})",
    );
    let before = hit_labels(&svc, "zqmarrow");
    assert_eq!(before, ["Chapter", "Chapter"]);
    ok(
        &svc,
        "CALL drevo.rag.context('zqmarrow', {trace: true}) YIELD traceId RETURN traceId",
    );
    assert_eq!(
        hit_labels(&svc, "zqmarrow"),
        before,
        "#819: the trace of a question is returned as a hit for the question"
    );
}

#[test]
fn a_queued_ingest_item_is_not_a_full_text_hit() {
    let svc = NativeService::in_memory();
    ok(&svc, "CREATE (:Chapter {title: 'The xylograph ledger'})");
    let params = HashMap::from([
        (
            "items".to_string(),
            Value::List(vec![map(&[(
                "text",
                Value::String("The xylograph ledger was hidden in the cellar.".into()),
            )])]),
        ),
        (
            "config".to_string(),
            map(&[("source", map(&[("id", Value::String("book-1".into()))]))]),
        ),
    ]);
    run_with(
        &svc,
        "CALL drevo.ingest.submit($items, $config) YIELD jobId RETURN jobId",
        params,
    )
    .expect("submit");
    assert_eq!(
        hit_labels(&svc, "xylograph"),
        ["Chapter"],
        "#819: the job queue's copy of the text is returned as a hit"
    );
}

#[test]
fn conversation_memory_stays_searchable() {
    // Messages are what `drevo.memory.recall` is for: they stay indexed.
    let svc = NativeService::in_memory();
    ok(
        &svc,
        "CALL drevo.memory.addMessage('s1', 'user', 'remember the zqmarrow port') YIELD node \
         RETURN node",
    );
    let res = ok(
        &svc,
        "CALL drevo.memory.recall('s1', 'zqmarrow', 5) YIELD node RETURN node.text",
    );
    assert_eq!(res.rows.len(), 1);
}

fn traced_book() -> (NativeService, String) {
    let svc = NativeService::in_memory();
    ok(
        &svc,
        "CREATE (:Chapter {title: 'Prologue zqmarrow'})-[:NEXT]->\
         (:Chapter {title: 'Chapter one zqmarrow'})",
    );
    let res = ok(
        &svc,
        "CALL drevo.rag.context('zqmarrow', {trace: true}) YIELD traceId RETURN traceId",
    );
    let trace = match &res.rows[0][0] {
        Value::String(id) => id.clone(),
        other => panic!("expected a trace id, got {other:?}"),
    };
    (svc, trace)
}

#[test]
fn a_trace_is_still_read_by_id_and_listed() {
    let (svc, trace) = traced_book();
    let explained = ok(
        &svc,
        &format!("CALL drevo.rag.explain('{trace}') YIELD node RETURN count(node)"),
    );
    assert_eq!(explained.rows[0][0], Value::Integer(2), "both chapters");
    let stored = ok(&svc, "MATCH (t:ContextTrace) RETURN t.query");
    assert_eq!(stored.rows.len(), 1);
    assert_eq!(stored.rows[0][0], Value::String("zqmarrow".into()));
}

#[test]
fn the_other_full_text_procedures_leave_bookkeeping_out_too() {
    let (svc, _) = traced_book();
    for call in [
        "CALL db.index.fulltext.queryNodes('any', 'zqmarrow') YIELD node",
        "CALL drevo.search.hybrid('zqmarrow', null, 'embedding', 20) YIELD node",
    ] {
        let res = ok(
            &svc,
            &format!("{call} RETURN labels(node)[0] AS l ORDER BY l"),
        );
        let labels: Vec<&Value> = res.rows.iter().map(|r| &r[0]).collect();
        assert_eq!(
            labels,
            [
                &Value::String("Chapter".into()),
                &Value::String("Chapter".into())
            ],
            "{call}"
        );
    }
}

#[test]
fn a_trace_written_by_the_statement_is_not_a_hit_in_that_statement() {
    let svc = NativeService::in_memory();
    ok(&svc, "CREATE (:Chapter {title: 'Prologue zqmarrow'})");
    // The search runs after the trace was written, in the same transaction.
    let res = ok(
        &svc,
        "CALL drevo.rag.context('zqmarrow', {trace: true}) YIELD traceId \
         CALL fts.search('zqmarrow', 20) YIELD node \
         RETURN labels(node)[0] AS l",
    );
    let labels: Vec<&Value> = res.rows.iter().map(|r| &r[0]).collect();
    assert_eq!(labels, [&Value::String("Chapter".into())]);
}

#[test]
fn bookkeeping_text_does_not_shift_the_scores_of_knowledge() {
    let score = |svc: &NativeService| {
        let res = ok(
            svc,
            "CALL fts.search('zqmarrow', 20) YIELD node, score \
             RETURN node.title, score ORDER BY node.title",
        );
        res.rows
    };
    let (traced, _) = traced_book();
    let plain = NativeService::in_memory();
    ok(
        &plain,
        "CREATE (:Chapter {title: 'Prologue zqmarrow'})-[:NEXT]->\
         (:Chapter {title: 'Chapter one zqmarrow'})",
    );
    assert_eq!(score(&traced), score(&plain));
}
