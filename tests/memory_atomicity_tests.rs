//! A writing memory procedure is one transaction in plain auto-commit (no
//! triggers, no restricted user): when it fails after its first write,
//! nothing it wrote stays (#796).
//!
//! Each test provokes a failure after the procedure has already written,
//! with the pattern of
//! `entity_resolution_tests::a_merge_failing_after_its_first_writes_leaves_nothing`:
//! a unique constraint plus a pre-existing node that a later write of the
//! procedure collides with. Then it checks that the node and relationship
//! counts (and, for property updates, the properties) are what they were
//! before the call.
//!
//! Where a procedure's later writes are relationships only (no relationship
//! constraints exist, so they cannot be made to fail) the failure comes from
//! a second `CALL` in the same statement instead: the statement still only
//! calls writing procedures, so it is the same auto-commit path. Those tests
//! say so.
//!
//! The scenario is a book editor's memory: manuscripts split into chunks,
//! characters and facts read from them, and an editing session.

use std::collections::HashMap;

use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
}

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    run(svc, q).unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

fn int(svc: &NativeService, q: &str) -> i64 {
    match run_ok(svc, q).rows.first().and_then(|r| r.first()) {
        Some(Value::Integer(n)) => *n,
        other => panic!("`{q}`: expected an integer, got {other:?}"),
    }
}

/// `(nodes, relationships)` in the whole graph.
fn counts(svc: &NativeService) -> (i64, i64) {
    (
        int(svc, "MATCH (n) RETURN count(n)"),
        int(svc, "MATCH ()-[r]->() RETURN count(r)"),
    )
}

/// Run `q`, which must fail, and assert the graph has the counts it had
/// before: the procedure's earlier writes must not survive the failure.
fn assert_failure_leaves_nothing(svc: &NativeService, q: &str, proc: &str) {
    let before = counts(svc);
    let failed = run(svc, q);
    assert!(
        failed.is_err(),
        "the setup must make `{q}` fail after its first write, got {failed:?}"
    );
    let after = counts(svc);
    assert_eq!(
        after, before,
        "#796: {proc} failed ({failed:?}) but its earlier writes stayed — \
         (nodes, relationships) went from {before:?} to {after:?}; in plain auto-commit \
         the call is not one transaction"
    );
}

const CH0: &str = "Dana opened the manuscript; Dana read till dawn.";
const CH1: &str = "By noon Dana had joined Acme as an editor.";

/// `book-1` with one chunk, and the entities Dana / Acme.
fn manuscript(svc: &NativeService) {
    run_ok(
        svc,
        "CALL drevo.memory.addSource('book-1', 'file:///books/one.md', 'Book One', 'document') \
         YIELD node RETURN node",
    );
    run_ok(
        svc,
        &format!(
            "CALL drevo.memory.addChunks('book-1', [{{order: 0, text: '{CH0}'}}]) \
             YIELD node RETURN node"
        ),
    );
    for (name, ty) in [("Dana", "PERSON"), ("Acme", "ORGANIZATION")] {
        run_ok(
            svc,
            &format!(
                "CALL drevo.memory.rememberEntity(null, '{name}', '{ty}', null) YIELD node \
                 RETURN node"
            ),
        );
    }
}

/// A unique constraint on `Chunk.text` and a loose chunk (of no source)
/// holding `text`: creating another chunk with that text fails.
fn chunk_text_taken(svc: &NativeService, text: &str) {
    run_ok(
        svc,
        "CREATE CONSTRAINT chunk_text FOR (c:Chunk) REQUIRE c.text IS UNIQUE",
    );
    run_ok(
        svc,
        &format!("CREATE (:Chunk {{id: 'loose', text: '{text}'}})"),
    );
}

// ----- in-procedure failures ---------------------------------------------------

#[test]
fn remember_entity_failing_after_its_first_writes_leaves_nothing() {
    let svc = NativeService::in_memory();
    chunk_text_taken(&svc, CH1);
    // Creates the entity Mira, then the source book-2, then fails on the
    // chunk whose text is taken.
    assert_failure_leaves_nothing(
        &svc,
        &format!(
            "CALL drevo.memory.rememberEntity(null, 'Mira', 'PERSON', null, {{\
               source: {{id: 'book-2', title: 'Book Two'}}, \
               chunk: {{id: 'book-2#0', order: 0, text: '{CH1}'}}, \
               surface: 'Dana'}}) YIELD node RETURN node"
        ),
        "drevo.memory.rememberEntity",
    );
    assert_eq!(
        int(&svc, "MATCH (e:Entity {name: 'Mira'}) RETURN count(e)"),
        0,
        "#796: the failed rememberEntity left its entity behind"
    );
}

#[test]
fn assert_fact_failing_after_its_first_writes_leaves_nothing() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    chunk_text_taken(&svc, CH1);
    // Upserts the source book-2, then fails on the chunk whose text is taken.
    assert_failure_leaves_nothing(
        &svc,
        &format!(
            "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', true, {{\
               source: {{id: 'book-2', title: 'Book Two'}}, \
               chunk: {{id: 'book-2#1', order: 1, text: '{CH1}'}}, \
               quote: 'joined Acme'}}) YIELD rel RETURN rel"
        ),
        "drevo.memory.assertFact",
    );
    assert_eq!(
        int(&svc, "MATCH (s:Source {id: 'book-2'}) RETURN count(s)"),
        0,
        "#796: the failed assertFact left its source behind"
    );
}

#[test]
fn add_source_failing_after_its_first_write_changes_nothing() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    run_ok(
        &svc,
        "CALL drevo.memory.addSource('book-2', 'file:///books/two.md', 'Book Two', 'document') \
         YIELD node RETURN node",
    );
    run_ok(
        &svc,
        "CREATE CONSTRAINT source_title FOR (s:Source) REQUIRE s.title IS UNIQUE",
    );
    // An upsert of book-1: sets `uri`, then fails setting the taken `title`.
    let before = counts(&svc);
    let failed = run(
        &svc,
        "CALL drevo.memory.addSource('book-1', 'file:///books/renamed.md', 'Book Two', null) \
         YIELD node RETURN node",
    );
    assert!(failed.is_err(), "the title must collide: {failed:?}");
    assert_eq!(counts(&svc), before);
    let uri = run_ok(&svc, "MATCH (s:Source {id: 'book-1'}) RETURN s.uri");
    assert_eq!(
        uri.rows[0][0],
        Value::String("file:///books/one.md".into()),
        "#796: addSource failed ({failed:?}) but the `uri` it set before failing stayed"
    );
}

#[test]
fn add_chunks_failing_after_its_first_writes_leaves_nothing() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    chunk_text_taken(&svc, CH1);
    // Creates chunk book-1#1 and its FROM edge, then fails on book-1#2.
    assert_failure_leaves_nothing(
        &svc,
        &format!(
            "CALL drevo.memory.addChunks('book-1', [\
               {{order: 1, text: 'A second, fresh passage.'}}, \
               {{order: 2, text: '{CH1}'}}]) YIELD node RETURN node"
        ),
        "drevo.memory.addChunks",
    );
}

// ----- failures from a later CALL in the same statement -------------------------
//
// addMessage, recordReasoning and the trace of rag.context write one node and
// then relationships; retractFact only sets properties on relationships.
// Relationships cannot carry constraints, so none of these can be made to
// fail after its own first write. Instead a second CALL in the same
// statement fails after the first CALL has written.

#[test]
fn add_message_then_a_failing_call_leaves_nothing() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CALL drevo.memory.addMessage('edit-1', 'user', 'Tighten chapter one') YIELD node \
         RETURN node",
    );
    run_ok(
        &svc,
        "CREATE CONSTRAINT message_text FOR (m:Message) REQUIRE m.text IS UNIQUE",
    );
    // The first CALL adds a message and its NEXT edge; the second adds the
    // same text, which the constraint refuses.
    assert_failure_leaves_nothing(
        &svc,
        "CALL drevo.memory.addMessage('edit-1', 'assistant', 'Done: chapter one is shorter') \
         YIELD node WITH node AS first \
         CALL drevo.memory.addMessage('edit-1', 'user', 'Done: chapter one is shorter') \
         YIELD node RETURN first, node",
        "drevo.memory.addMessage",
    );
}

#[test]
fn record_reasoning_then_a_failing_call_leaves_nothing() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CALL drevo.memory.addMessage('edit-1', 'user', 'Tighten chapter one') YIELD node \
         RETURN node",
    );
    run_ok(
        &svc,
        "CREATE CONSTRAINT trace_step FOR (t:ReasoningTrace) REQUIRE t.step IS UNIQUE",
    );
    // The first CALL adds a trace and its INITIATED_BY edge; the second one
    // repeats the step, which the constraint refuses.
    assert_failure_leaves_nothing(
        &svc,
        "CALL drevo.memory.recordReasoning('edit-1', 'cut the prologue', 'editor', 'ok') \
         YIELD node WITH node AS first \
         CALL drevo.memory.recordReasoning('edit-1', 'cut the prologue', 'editor', 'ok') \
         YIELD node RETURN first, node",
        "drevo.memory.recordReasoning",
    );
}

#[test]
fn retract_fact_then_a_failing_call_changes_nothing() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    run_ok(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false) YIELD rel RETURN rel",
    );
    // The first CALL closes the fact (valid_until = now); the second names an
    // entity that does not exist.
    let before = counts(&svc);
    let failed = run(
        &svc,
        "CALL drevo.memory.retractFact('Dana', 'WORKS_AT', 'Acme') YIELD rel \
         WITH count(rel) AS closed \
         CALL drevo.memory.retractFact('Dana', 'WORKS_AT', 'Nobody') YIELD rel \
         RETURN closed, rel",
    );
    assert!(failed.is_err(), "the second retract must fail: {failed:?}");
    assert_eq!(counts(&svc), before);
    assert_eq!(
        int(
            &svc,
            "MATCH (:Entity {name: 'Dana'})-[f:RELATED_TO]->(:Entity {name: 'Acme'}) \
             WHERE f.valid_until IS NULL RETURN count(f)"
        ),
        1,
        "#796: the statement failed ({failed:?}) but the retractFact before the failure \
         stayed — the fact is closed"
    );
}

#[cfg(feature = "http")]
#[test]
fn rag_context_trace_then_a_failing_call_leaves_nothing() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE (:Chapter {title: 'Prologue zqmarrow'})-[:NEXT]->(:Chapter {title: 'Chapter one'})",
    );
    run_ok(
        &svc,
        "CALL drevo.memory.addMessage('edit-1', 'user', 'What is in the prologue?') YIELD node \
         RETURN node",
    );
    run_ok(
        &svc,
        "CREATE CONSTRAINT message_text FOR (m:Message) REQUIRE m.text IS UNIQUE",
    );
    // The first CALL records a :ContextTrace with INCLUDED edges; the second
    // adds a message whose text the constraint refuses. (A second traced
    // rag.context would fail earlier, on its full-text search, with
    // IndexBehindWrites — not the failure this test is about.)
    assert_failure_leaves_nothing(
        &svc,
        "CALL drevo.rag.context('zqmarrow', {trace: true}) YIELD traceId \
         WITH traceId AS first \
         CALL drevo.memory.addMessage('edit-1', 'user', 'What is in the prologue?') \
         YIELD node RETURN first, node",
        "drevo.rag.context (trace: true)",
    );
    assert_eq!(
        int(&svc, "MATCH (t:ContextTrace) RETURN count(t)"),
        0,
        "#796: the failed statement left a :ContextTrace behind"
    );
}
