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

/// Every procedure that takes an argument, called with one argument read
/// from the row (`x`). The other arguments only have to pass the arity
/// check: a procedure may refuse the call for its own reasons (no such
/// index, no embedder, no such job), never because it cannot see `x`.
const SWEEP: &[&str] = &[
    "WITH 'Chapter' AS x CALL drevo.vector.query(x, 'embedding', [0.1], 1)",
    "WITH 'zqmarrow' AS x CALL drevo.semantic.query('Chapter', 'embedding', x, 1)",
    "WITH 'zqmarrow' AS x CALL drevo.semantic.embed(x)",
    "WITH 'Chapter' AS x CALL drevo.semantic.reindex(x, 'embedding', 10)",
    "WITH 'zqmarrow' AS x CALL drevo.semantic.queryRel('NEXT', 'embedding', x, 1)",
    "WITH 'NEXT' AS x CALL drevo.semantic.reindexRel(x, 'embedding', 10)",
    "WITH 'Chapter' AS x CALL drevo.semantic.register(x, 'title', 'embedding', 'auto')",
    "WITH 'NEXT' AS x CALL drevo.semantic.registerRel(x, 'note', 'embedding', 'auto')",
    "WITH true AS x CALL drevo.indexes.setSecondary(x)",
    "WITH 'row_seq' AS x CALL drevo.sequence.create(x)",
    "WITH 'row_seq' AS x CALL drevo.sequence.drop(x)",
    "WITH 'chapter.read' AS x CALL drevo.events.post(x, {n: 1})",
    "WITH 'gid-1' AS x CALL drevo.tx.commitPrepared(x)",
    "WITH 'gid-1' AS x CALL drevo.tx.rollbackPrepared(x)",
    "WITH 'gid-1' AS x CALL drevo.tx.heuristicRollback(x)",
    "WITH 5 AS x CALL drevo.pagerank({maxIterations: x})",
    "MATCH (x:Chapter) CALL drevo.pagerank.personalized([x])",
    "WITH 'Chapter' AS x CALL drevo.stableMatching(x, 'Chapter', 'NEXT', 'rank')",
    "MATCH (x:Chapter) CALL drevo.path.weighted(x, x, {})",
    "MATCH (x:Chapter) CALL drevo.path.timetable([{node: x, cost: 0}], [{node: x, cost: 0}], {departAt: 0})",
    "MATCH (x:Chapter) CALL drevo.path.reachable(x, {})",
    "WITH 'Chapter' AS x CALL drevo.spatial.nearest(x, {lat: 1.0, lon: 1.0})",
    "WITH 'zqmarrow' AS x CALL drevo.text.fuzzy('titles', x, 1)",
    "WITH 'zqmarrow' AS x CALL fts.search(x, 5)",
    "WITH 'zqmarrow' AS x CALL fts.searchRelationships(x, 5)",
    "WITH 'zqmarrow' AS x CALL drevo.search.hybrid(x, 'Chapter', 'embedding', 5)",
    "WITH 'zqmarrow' AS x CALL drevo.search.hybridRelationships(x, 'NEXT', 'embedding', 5)",
    "MATCH (x:Chapter) CALL drevo.graph.expand([x])",
    "WITH 'zqmarrow' AS x CALL drevo.rag.retrieve(x)",
    "WITH 2 AS x CALL drevo.rag.retrieve('zqmarrow', {k: x})",
    "WITH 'zqmarrow' AS x CALL drevo.rag.context(x)",
    "WITH 's1' AS x CALL drevo.rag.traces(x, 5)",
    "WITH 'trace-1' AS x CALL drevo.rag.explain(x)",
    "WITH 'trace-1' AS x CALL drevo.rag.replay(x)",
    "WITH '2000-01-01T00:00:00Z' AS x CALL drevo.rag.pruneTraces(x)",
    "WITH 'zqmarrow' AS x CALL drevo.rag.evaluate([{query: x, expected: []}])",
    "WITH 'zqmarrow' AS x CALL drevo.rag.evaluateSummary([{query: x, expected: []}])",
    "WITH 'idx' AS x CALL db.index.vector.queryNodes(x, 1, [0.1])",
    "WITH 'zqmarrow' AS x CALL db.index.fulltext.queryNodes('any', x)",
    "WITH 'idx' AS x CALL db.index.vector.queryRelationships(x, 1, [0.1])",
    "WITH 'zqmarrow' AS x CALL drevo.memory.recall('s1', x, 5)",
    "WITH 's1' AS x CALL drevo.memory.recallVector(x, [0.1], 5)",
    "WITH 'zqmarrow' AS x CALL drevo.memory.recallSemantic('s1', x, 5)",
    "WITH 's1' AS x CALL drevo.memory.getConversation(x, 5)",
    "WITH 'row text' AS x CALL drevo.memory.addMessage('s1', 'user', x)",
    "WITH 'row step' AS x CALL drevo.memory.recordReasoning('s1', x, 'tool', 'ok')",
    "WITH 'Ada' AS x CALL drevo.memory.rememberEntity('s1', x, 'Person', 'a person')",
    "WITH 'Ada' AS x CALL drevo.memory.assertFact(x, 'KNOWS', 'Bob', false)",
    "WITH 'Ada' AS x CALL drevo.memory.retractFact(x, 'KNOWS', 'Bob')",
    "WITH 'Ada' AS x CALL drevo.memory.factsAt(x, '2030-01-01T00:00:00Z')",
    "WITH 'src-1' AS x CALL drevo.memory.addSource(x, 'file:///a', 'A', 'file')",
    "WITH 'row chunk' AS x CALL drevo.memory.addChunks('src-1', [{text: x}])",
    "WITH 'Ada' AS x CALL drevo.memory.evidence(x, 5)",
    "WITH 'Chapter' AS x CALL drevo.entity.candidates(x)",
    "MATCH (x:Chapter) WITH id(x) AS x LIMIT 1 CALL drevo.entity.merge(x, [])",
    "WITH 'Ada' AS x CALL drevo.entity.aliases(x)",
    "WITH 'extractive' AS x CALL drevo.community.summarize({mode: x})",
    "WITH 'zqmarrow' AS x CALL drevo.community.query(x, 3)",
    "MATCH (x:Chapter) CALL drevo.community.of(x)",
    "WITH 'how many chapters' AS x CALL drevo.cypher.fromText(x)",
    "WITH 'Ada knows Bob' AS x CALL drevo.extract.entities(x)",
    "WITH 'Ada knows Bob' AS x CALL drevo.extract.facts(x)",
    "WITH 'Ada knows Bob' AS x CALL drevo.extract.chunks(x)",
    "WITH 'Ada knows Bob' AS x CALL drevo.extract.ingest(x, {})",
    "WITH 'Ada knows Bob' AS x CALL drevo.ingest.submit([{text: x}], {})",
    "WITH 'job-1' AS x CALL drevo.ingest.status(x)",
    "WITH 'failed' AS x CALL drevo.ingest.list(x, 5)",
    "WITH 'job-1' AS x CALL drevo.ingest.cancel(x)",
    "WITH 'job-1' AS x CALL drevo.ingest.pause(x)",
    "WITH 'job-1' AS x CALL drevo.ingest.resume(x)",
    "WITH 'job-1' AS x CALL drevo.ingest.retryFailed(x)",
    "WITH 'job-1' AS x CALL drevo.ingest.errors(x, 5)",
    "WITH 'job-1' AS x CALL drevo.ingest.purge(x)",
    "WITH 'row_trigger' AS x CALL apoc.trigger.drop('drevo', x)",
    "WITH 'row_trigger' AS x CALL apoc.trigger.stop('drevo', x)",
    "WITH 'row_trigger' AS x CALL apoc.trigger.start('drevo', x)",
    "WITH 'row_trigger' AS x CALL apoc.trigger.remove(x)",
    "WITH 'row_trigger' AS x CALL apoc.trigger.pause(x)",
    "WITH 'row_trigger' AS x CALL apoc.trigger.resume(x)",
    "WITH 'drevo' AS x CALL apoc.trigger.dropAll(x)",
];

#[test]
fn every_procedure_reads_an_argument_from_the_row() {
    let svc = book();
    let mut blind = Vec::new();
    for q in SWEEP {
        let query = parse(q).unwrap_or_else(|e| panic!("`{q}` does not parse: {e:?}"));
        match svc.execute(&query, HashMap::new()) {
            Err(ExecError::UnboundVariable { name, .. }) => {
                blind.push(format!("`{q}` does not see the row's `{name}`"));
            }
            // The statement itself has to be a valid call, or it proves
            // nothing about the argument.
            Err(ExecError::InvalidProcedureCall { message, .. })
                if message.starts_with("expected ")
                    || message.contains("no such procedure")
                    || message.contains("does not yield") =>
            {
                panic!("`{q}` is not a valid call: {message}");
            }
            _ => {}
        }
    }
    assert!(blind.is_empty(), "#818:\n{}", blind.join("\n"));
}

fn messages(svc: &NativeService, session: &str) -> Vec<String> {
    ok(
        svc,
        &format!("MATCH (m:Message {{session: '{session}'}}) RETURN m.text ORDER BY m.seq"),
    )
    .rows
    .iter()
    .map(|r| string(&r[0]))
    .collect()
}

#[test]
fn a_writing_procedure_runs_once_per_row_in_row_order() {
    let svc = book();
    let res = ok(
        &svc,
        "UNWIND ['first zqone', 'second zqtwo', 'third zqthree'] AS t \
         CALL drevo.memory.addMessage('s2', 'user', t) YIELD node \
         RETURN t, node.text, node.seq",
    );
    let rows: Vec<(String, String)> = res
        .rows
        .iter()
        .map(|r| (string(&r[0]), string(&r[1])))
        .collect();
    assert_eq!(
        rows,
        [
            ("first zqone".to_string(), "first zqone".to_string()),
            ("second zqtwo".to_string(), "second zqtwo".to_string()),
            ("third zqthree".to_string(), "third zqthree".to_string()),
        ]
    );
    assert_eq!(
        messages(&svc, "s2"),
        ["first zqone", "second zqtwo", "third zqthree"]
    );
}

#[test]
fn no_rows_means_no_calls() {
    let svc = book();
    let res = ok(
        &svc,
        "MATCH (m:Missing) WITH m.text AS t \
         CALL drevo.memory.addMessage('s3', 'user', t) YIELD node RETURN node",
    );
    assert!(res.rows.is_empty());
    assert!(messages(&svc, "s3").is_empty(), "nothing was written");
}

#[test]
fn each_row_is_joined_with_its_own_results_only() {
    let svc = book();
    // `nothingmatches` finds nothing, so its row drops out, as with any
    // procedure that yields no rows for a row.
    let res = ok(
        &svc,
        "UNWIND ['vexlight', 'nothingmatches', 'zqmarrow'] AS q \
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
            ("vexlight".to_string(), "Chapter one vexlight".to_string()),
            ("zqmarrow".to_string(), "Prologue zqmarrow".to_string()),
        ]
    );
}

#[test]
fn a_trailing_call_without_yield_returns_every_rows_output() {
    let svc = book();
    let res = ok(
        &svc,
        "UNWIND ['zqmarrow', 'vexlight'] AS q CALL drevo.rag.context(q)",
    );
    assert_eq!(res.columns[0], "text");
    assert_eq!(res.rows.len(), 2);
    assert!(string(&res.rows[0][0]).contains("Prologue zqmarrow"));
    assert!(string(&res.rows[1][0]).contains("Chapter one vexlight"));
}

#[test]
fn row_arguments_mix_with_parameters_and_expressions() {
    let svc = book();
    let params = HashMap::from([("k".to_string(), Value::Integer(5))]);
    let query = parse(
        "UNWIND ['ZQMARROW'] AS q \
         CALL fts.search(toLower(q), $k) YIELD node \
         WITH node WHERE node:Chapter RETURN node.title",
    )
    .expect("parse");
    let res = svc
        .execute(&query, params)
        .expect("#818: an expression over the row");
    assert_eq!(string(&res.rows[0][0]), "Prologue zqmarrow");
}

#[test]
fn a_config_map_reads_the_row() {
    let svc = book();
    let res = ok(
        &svc,
        "UNWIND [1, 2] AS k \
         CALL drevo.rag.retrieve('zqmarrow vexlight chapter prologue', {k: k, hops: 0}) \
         YIELD node RETURN k, count(node) AS found ORDER BY k",
    );
    let found: Vec<i64> = res
        .rows
        .iter()
        .map(|r| match r[1] {
            Value::Integer(n) => n,
            ref other => panic!("{other:?}"),
        })
        .collect();
    assert!(found.iter().zip([1, 2]).all(|(n, k)| *n <= k), "{found:?}");
}

#[test]
fn a_wrong_typed_row_value_is_the_procedures_own_error() {
    let svc = book();
    let err = run(
        &svc,
        "WITH 42 AS q CALL drevo.rag.context(q) YIELD text RETURN text",
    )
    .expect_err("a number is not a question");
    assert!(
        matches!(&err, ExecError::InvalidProcedureCall { message, .. }
            if message.contains("query must be a string")),
        "{err:?}"
    );
}

#[test]
fn a_writing_procedure_runs_once_per_row_whatever_its_arguments() {
    let svc = book();
    let res = ok(
        &svc,
        "UNWIND [1, 2, 3] AS i \
         CALL drevo.memory.addMessage('s4', 'user', 'every row') YIELD node \
         RETURN i, node.seq",
    );
    assert_eq!(res.rows.len(), 3);
    assert_eq!(
        messages(&svc, "s4"),
        ["every row", "every row", "every row"]
    );

    // And not at all when there is no row.
    let res = ok(
        &svc,
        "MATCH (m:Missing) \
         CALL drevo.memory.addMessage('s5', 'user', 'no row') YIELD node \
         RETURN node",
    );
    assert!(res.rows.is_empty());
    assert!(messages(&svc, "s5").is_empty(), "nothing was written");
}

#[test]
fn a_search_with_row_free_arguments_is_joined_to_every_row() {
    let svc = book();
    let res = ok(
        &svc,
        "UNWIND [1, 2, 3] AS i \
         CALL fts.search('zqmarrow', 5) YIELD node \
         WITH i, node WHERE node:Chapter RETURN i, node.title",
    );
    let rows: Vec<(i64, String)> = res
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Integer(i) => (*i, string(&r[1])),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        rows,
        [
            (1, "Prologue zqmarrow".to_string()),
            (2, "Prologue zqmarrow".to_string()),
            (3, "Prologue zqmarrow".to_string()),
        ]
    );
}
