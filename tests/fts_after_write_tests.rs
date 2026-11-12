//! Full-text search after a write in the same statement or transaction
//! (#817).
//!
//! The full-text index covers committed data only, and a statement or an
//! explicit transaction that has written is refused with
//! `ExecError::IndexBehindWrites`. The graph-RAG procedures search full text,
//! so the ordinary agent turn — store the message, then build the context,
//! in one unit of work — fails, and so does a second traced
//! `drevo.rag.context` (the first one wrote its `:ContextTrace`).
//!
//! These tests state the behaviour #817 asks for: full text sees the
//! transaction's own writes, as the label / property / text indexes do.

use std::collections::HashMap;

use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native::NativeTxId;
use drevo::native_service::NativeService;

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
}

fn ok(svc: &NativeService, q: &str) -> ExecResult {
    run(svc, q).unwrap_or_else(|e| panic!("#817: `{q}` failed: {e:?}"))
}

fn in_tx(svc: &NativeService, tx: NativeTxId, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute_in_tx(tx, &parse(q).expect("parse"), HashMap::new())
}

fn ok_in_tx(svc: &NativeService, tx: NativeTxId, q: &str) -> ExecResult {
    in_tx(svc, tx, q).unwrap_or_else(|e| panic!("#817: `{q}` in a transaction failed: {e:?}"))
}

fn strings(result: &ExecResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        })
        .collect()
}

/// Two chapters that both mention `zqmarrow`.
fn book() -> NativeService {
    let svc = NativeService::in_memory();
    ok(
        &svc,
        "CREATE (:Chapter {title: 'Prologue zqmarrow'})-[:NEXT]->\
         (:Chapter {title: 'Chapter one zqmarrow'})",
    );
    svc
}

const ADD_MESSAGE: &str = "CALL drevo.memory.addMessage('s1', 'user', 'what about zqmarrow?') \
                           YIELD node RETURN node";

// ── one statement ────────────────────────────────────────────────────────

#[test]
fn two_traced_contexts_in_one_statement() {
    let svc = book();
    let res = ok(
        &svc,
        "CALL drevo.rag.context('zqmarrow', {trace: true}) YIELD traceId AS a \
         CALL drevo.rag.context('zqmarrow', {trace: true}) YIELD traceId AS b RETURN a, b",
    );
    assert_eq!(res.rows.len(), 1);
    assert_ne!(res.rows[0][0], res.rows[0][1], "two traces, two ids");
}

#[test]
fn a_context_after_add_message_in_one_statement() {
    let svc = book();
    let res = ok(
        &svc,
        "CALL drevo.memory.addMessage('s1', 'user', 'what about zqmarrow?') YIELD node \
         CALL drevo.rag.context('zqmarrow') YIELD text RETURN text",
    );
    let text = strings(&res).remove(0);
    assert!(text.contains("Prologue zqmarrow"), "{text}");
}

#[test]
fn full_text_search_finds_a_node_created_earlier_in_the_statement() {
    let svc = NativeService::in_memory();
    ok(&svc, "CREATE (:Doc {title: 'sourdough bread'})");
    let res = ok(
        &svc,
        "CREATE (:Doc {title: 'rye bread'}) WITH 1 AS one \
         CALL fts.search('bread', 5) YIELD node RETURN node.title ORDER BY node.title",
    );
    assert_eq!(strings(&res), ["rye bread", "sourdough bread"]);
}

// ── an explicit transaction (the drivers' `execute_write`) ───────────────

#[test]
fn a_context_after_add_message_in_a_transaction() {
    let svc = book();
    let tx = svc.begin_tx();
    ok_in_tx(&svc, tx, ADD_MESSAGE);
    let res = ok_in_tx(
        &svc,
        tx,
        "CALL drevo.rag.context('zqmarrow') YIELD text RETURN text",
    );
    let text = strings(&res).remove(0);
    assert!(text.contains("Prologue zqmarrow"), "{text}");
    svc.rollback_tx(tx);
}

#[test]
fn a_retrieve_after_add_message_in_a_transaction() {
    let svc = book();
    let tx = svc.begin_tx();
    ok_in_tx(&svc, tx, ADD_MESSAGE);
    let res = ok_in_tx(
        &svc,
        tx,
        "CALL drevo.rag.retrieve('zqmarrow') YIELD node RETURN node.title ORDER BY node.title",
    );
    assert_eq!(strings(&res), ["Chapter one zqmarrow", "Prologue zqmarrow"]);
    svc.rollback_tx(tx);
}

#[test]
fn recall_finds_the_message_added_earlier_in_the_transaction() {
    let svc = book();
    let tx = svc.begin_tx();
    ok_in_tx(&svc, tx, ADD_MESSAGE);
    let res = ok_in_tx(
        &svc,
        tx,
        "CALL drevo.memory.recall('s1', 'zqmarrow', 5) YIELD node RETURN node.text",
    );
    assert_eq!(strings(&res), ["what about zqmarrow?"]);
    svc.rollback_tx(tx);
}

#[test]
fn a_second_traced_context_in_a_transaction() {
    let svc = book();
    let tx = svc.begin_tx();
    let traced = "CALL drevo.rag.context('zqmarrow', {trace: true}) YIELD traceId RETURN traceId";
    let first = ok_in_tx(&svc, tx, traced);
    let second = ok_in_tx(&svc, tx, traced);
    assert_ne!(first.rows[0][0], second.rows[0][0], "two traces, two ids");
    svc.rollback_tx(tx);
}

#[test]
fn full_text_search_in_a_transaction_reads_its_own_writes() {
    let svc = NativeService::in_memory();
    ok(&svc, "CREATE (:Doc {title: 'sourdough bread'})");
    ok(&svc, "CREATE (:Doc {title: 'stale bread'})");
    ok(&svc, "CREATE (:Doc {title: 'white bread'})");
    let tx = svc.begin_tx();
    ok_in_tx(&svc, tx, "CREATE (:Doc {title: 'rye bread'})");
    ok_in_tx(&svc, tx, "MATCH (d:Doc {title: 'stale bread'}) DELETE d");
    ok_in_tx(
        &svc,
        tx,
        "MATCH (d:Doc {title: 'white bread'}) SET d.title = 'white loaf'",
    );
    let search = "CALL fts.search('bread', 10) YIELD node RETURN node.title ORDER BY node.title";
    // Created: found. Deleted: gone. Retitled: not under its old text.
    assert_eq!(
        strings(&ok_in_tx(&svc, tx, search)),
        ["rye bread", "sourdough bread"]
    );
    assert_eq!(
        strings(&ok_in_tx(
            &svc,
            tx,
            "CALL fts.search('loaf', 10) YIELD node RETURN node.title"
        )),
        ["white loaf"]
    );
    svc.rollback_tx(tx);
    // Rolled back: the committed view is untouched.
    assert_eq!(
        strings(&ok(&svc, search)),
        ["sourdough bread", "stale bread", "white bread"]
    );
}
