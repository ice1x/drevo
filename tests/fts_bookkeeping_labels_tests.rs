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
