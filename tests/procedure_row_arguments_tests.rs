//! Procedure arguments that use the row's variables (#818).
//!
//! `drevo.community.query`, `drevo.events.post` and `drevo.extract.ingest`
//! evaluate their arguments against the current row; the search family —
//! `fts.search`, `drevo.rag.context`, `drevo.rag.retrieve`,
//! `drevo.memory.recall` — evaluates them with no row and fails with
//! `UnboundVariable`, so none of them can be called once per row.

use std::collections::HashMap;

use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
}

fn ok(svc: &NativeService, q: &str) -> ExecResult {
    run(svc, q).unwrap_or_else(|e| panic!("#818: `{q}` failed: {e:?}"))
}

fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    }
}

fn book() -> NativeService {
    let svc = NativeService::in_memory();
    ok(
        &svc,
        "CREATE (:Chapter {title: 'Prologue zqmarrow'})-[:NEXT]->\
         (:Chapter {title: 'Chapter one vexlight'})",
    );
    ok(
        &svc,
        "CALL drevo.memory.addMessage('s1', 'user', 'vexlight') YIELD node \
         RETURN node",
    );
    svc
}

#[test]
fn fts_search_takes_its_query_from_the_row() {
    let svc = book();
    let res = ok(
        &svc,
        "UNWIND ['zqmarrow', 'vexlight'] AS q \
         CALL fts.search(q, 5) YIELD node \
         WITH q, node WHERE node:Chapter RETURN q, node.title",
    );
    let rows: Vec<(String, String)> = res
        .rows
        .iter()
        .map(|r| (string(&r[0]), string(&r[1])))
        .collect();
    assert_eq!(
        rows,
        [
            ("zqmarrow".to_string(), "Prologue zqmarrow".to_string()),
            ("vexlight".to_string(), "Chapter one vexlight".to_string()),
        ]
    );
}

#[test]
fn rag_context_builds_one_context_per_row() {
    let svc = book();
    let res = ok(
        &svc,
        "UNWIND ['zqmarrow', 'vexlight'] AS q \
         CALL drevo.rag.context(q) YIELD text RETURN q, text",
    );
    assert_eq!(res.rows.len(), 2, "one context per question");
    assert!(string(&res.rows[0][1]).contains("Prologue zqmarrow"));
    assert!(string(&res.rows[1][1]).contains("Chapter one vexlight"));
}

#[test]
fn rag_retrieve_takes_its_query_from_a_matched_node() {
    let svc = book();
    // Retrieve for the session's last message.
    let res = ok(
        &svc,
        "MATCH (m:Message) WITH m.text AS q \
         CALL drevo.rag.retrieve(q) YIELD node RETURN node.title",
    );
    let titles: Vec<String> = res.rows.iter().map(|r| string(&r[0])).collect();
    assert!(
        titles.contains(&"Chapter one vexlight".to_string()),
        "{titles:?}"
    );
}

#[test]
fn memory_recall_takes_its_query_from_the_row() {
    let svc = book();
    let res = ok(
        &svc,
        "WITH 'vexlight' AS q \
         CALL drevo.memory.recall('s1', q, 5) YIELD node RETURN node.text",
    );
    assert_eq!(string(&res.rows[0][0]), "vexlight");
}

#[test]
fn a_row_argument_never_reads_as_an_unbound_variable() {
    let svc = book();
    for q in [
        "WITH 'zqmarrow' AS q CALL fts.search(q, 5) YIELD node RETURN node",
        "WITH 'zqmarrow' AS q CALL db.index.fulltext.queryNodes('any', q) YIELD node RETURN node",
        "WITH 'zqmarrow' AS q CALL drevo.rag.context(q) YIELD text RETURN text",
        "WITH 'zqmarrow' AS q CALL drevo.rag.retrieve(q) YIELD node RETURN node",
        "WITH 'zqmarrow' AS q CALL drevo.memory.recall('s1', q, 5) YIELD node RETURN node",
        "WITH 's1' AS s CALL drevo.memory.recall(s, 'zqmarrow', 5) YIELD node RETURN node",
        "WITH 5 AS k CALL fts.search('zqmarrow', k) YIELD node RETURN node",
    ] {
        if let Err(ExecError::UnboundVariable { name, .. }) = run(&svc, q) {
            panic!("#818: `{q}` does not see the row's `{name}`");
        }
    }
}
