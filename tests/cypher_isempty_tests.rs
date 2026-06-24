//! End-to-end Cypher `isEmpty()` predicate-function tests — Phase 10
//! follow-up task `00159`.
//!
//! Neo4j 5 exposes `isEmpty(x)` as a single predicate over the three
//! *container* types — a String, a List, or a Map — returning a Boolean that
//! is `true` exactly when the container holds no elements (no characters / no
//! items / no entries). It fills a real gap left by `size`: `size(x) = 0`
//! works for Strings and Lists but `size` rejects a Map, so before this task
//! there was no first-class "is this map empty?" predicate.
//!
//! Semantics mirror Neo4j:
//!
//! * `isEmpty('')` / `isEmpty([])` / `isEmpty({})` → `true`.
//! * Any non-empty String / List / Map → `false`.
//! * A `NULL` argument yields `NULL` (NULL-propagating on the single argument,
//!   like every other scalar built-in).
//! * Any *non-container* argument — an Integer, Float, Boolean, node, … — is a
//!   recoverable `ExecError::InvalidFunctionCall`, never a panic or wrong
//!   answer.
//! * Wrong arity is likewise an `ExecError::InvalidFunctionCall`.
//!
//! These cases drive the real parser → executor pipeline across the five drevo
//! target scenario domains (CBT journal, story/book editor, IT task manager,
//! ERP, bug tracker) plus the cross-cutting semantics.

use std::collections::HashMap;

use drevo::cypher::executor::{execute, ExecError, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

fn run(source: &str, drevo: &Drevo) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").rows
}

fn run_err(source: &str, drevo: &Drevo) -> ExecError {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect_err("expected execution error")
}

/// One-row, one-column projection helper.
fn one(source: &str, drevo: &Drevo) -> Value {
    let rows = run(source, drevo);
    assert_eq!(rows.len(), 1, "expected exactly one row from {source:?}");
    rows[0][0].clone()
}

// ===== Strings ==============================================================

#[test]
fn is_empty_string_empty_is_true() {
    let db = db();
    assert_eq!(one("RETURN isEmpty('') AS v", &db), Value::Bool(true));
}

#[test]
fn is_empty_string_non_empty_is_false() {
    let db = db();
    assert_eq!(one("RETURN isEmpty('hello') AS v", &db), Value::Bool(false));
    // A single space is a character — not empty.
    assert_eq!(one("RETURN isEmpty(' ') AS v", &db), Value::Bool(false));
}

// ===== Lists ================================================================

#[test]
fn is_empty_list_empty_is_true() {
    let db = db();
    assert_eq!(one("RETURN isEmpty([]) AS v", &db), Value::Bool(true));
}

#[test]
fn is_empty_list_non_empty_is_false() {
    let db = db();
    assert_eq!(
        one("RETURN isEmpty([1, 2, 3]) AS v", &db),
        Value::Bool(false)
    );
    // A list whose sole element is itself empty is still non-empty.
    assert_eq!(one("RETURN isEmpty([[]]) AS v", &db), Value::Bool(false));
    // A list holding a single NULL has one element.
    assert_eq!(one("RETURN isEmpty([null]) AS v", &db), Value::Bool(false));
}

// ===== Maps =================================================================

#[test]
fn is_empty_map_empty_is_true() {
    let db = db();
    assert_eq!(one("RETURN isEmpty({}) AS v", &db), Value::Bool(true));
}

#[test]
fn is_empty_map_non_empty_is_false() {
    let db = db();
    assert_eq!(
        one("RETURN isEmpty({a: 1, b: 2}) AS v", &db),
        Value::Bool(false)
    );
    // A single entry whose value is NULL still counts as an entry.
    assert_eq!(
        one("RETURN isEmpty({a: null}) AS v", &db),
        Value::Bool(false)
    );
}

// ===== NULL propagation =====================================================

#[test]
fn is_empty_null_argument_propagates_null() {
    let db = db();
    assert_eq!(one("RETURN isEmpty(null) AS v", &db), Value::Null);
}

// ===== Type errors ==========================================================

#[test]
fn is_empty_rejects_integer() {
    let db = db();
    match run_err("RETURN isEmpty(5) AS v", &db) {
        ExecError::InvalidFunctionCall { name, .. } => assert_eq!(name, "isEmpty"),
        other => panic!("expected InvalidFunctionCall, got {other:?}"),
    }
}

#[test]
fn is_empty_rejects_float_and_bool() {
    let db = db();
    for src in ["RETURN isEmpty(1.5) AS v", "RETURN isEmpty(true) AS v"] {
        match run_err(src, &db) {
            ExecError::InvalidFunctionCall { name, .. } => assert_eq!(name, "isEmpty"),
            other => panic!("expected InvalidFunctionCall from {src:?}, got {other:?}"),
        }
    }
}

#[test]
fn is_empty_rejects_node() {
    let db = db();
    run("CREATE (:Person {name: 'Ada'})", &db);
    match run_err("MATCH (n:Person) RETURN isEmpty(n) AS v", &db) {
        ExecError::InvalidFunctionCall { name, .. } => assert_eq!(name, "isEmpty"),
        other => panic!("expected InvalidFunctionCall, got {other:?}"),
    }
}

// ===== Arity ================================================================

#[test]
fn is_empty_wrong_arity_is_invalid_call() {
    let db = db();
    for src in ["RETURN isEmpty() AS v", "RETURN isEmpty('a', 'b') AS v"] {
        match run_err(src, &db) {
            ExecError::InvalidFunctionCall { name, .. } => assert_eq!(name, "isEmpty"),
            other => panic!("expected InvalidFunctionCall from {src:?}, got {other:?}"),
        }
    }
}

// ===== Composition ==========================================================

#[test]
fn is_empty_composes_in_where() {
    let db = db();
    run(
        "CREATE (:Task {title: 'A', tags: ['urgent']}),
                (:Task {title: 'B', tags: []})",
        &db,
    );
    // Only the task with no tags survives the filter.
    let rows = run(
        "MATCH (t:Task) WHERE isEmpty(t.tags) RETURN t.title AS title",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("B".into()));
}

#[test]
fn is_empty_negates_with_not() {
    let db = db();
    run(
        "CREATE (:Task {title: 'A', tags: ['urgent']}),
                (:Task {title: 'B', tags: []})",
        &db,
    );
    let rows = run(
        "MATCH (t:Task) WHERE NOT isEmpty(t.tags) RETURN t.title AS title",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("A".into()));
}

// ===== Scenario domains =====================================================

#[test]
fn cbt_journal_flags_entries_without_distortions() {
    // CBT journal: an entry with no recorded cognitive distortions is "clean".
    let db = db();
    run(
        "CREATE (:Entry {mood: 'anxious', distortions: ['catastrophizing']}),
                (:Entry {mood: 'calm', distortions: []})",
        &db,
    );
    let rows = run(
        "MATCH (e:Entry)
         RETURN e.mood AS mood, isEmpty(e.distortions) AS clean
         ORDER BY mood",
        &db,
    );
    assert_eq!(rows.len(), 2);
    // anxious -> has distortions -> not clean
    assert_eq!(rows[0][0], Value::String("anxious".into()));
    assert_eq!(rows[0][1], Value::Bool(false));
    // calm -> no distortions -> clean
    assert_eq!(rows[1][0], Value::String("calm".into()));
    assert_eq!(rows[1][1], Value::Bool(true));
}

#[test]
fn story_editor_detects_chapter_with_no_text() {
    // Story/book editor: a chapter draft whose synopsis string is empty. (Note
    // `body` is a reserved model field that drops empty strings, so this uses a
    // plain user property to exercise the empty-String path through storage.)
    let db = db();
    run(
        "CREATE (:Chapter {title: 'Prologue', synopsis: ''}),
                (:Chapter {title: 'One', synopsis: 'It was a dark night.'})",
        &db,
    );
    let rows = run(
        "MATCH (c:Chapter) WHERE isEmpty(c.synopsis) RETURN c.title AS title",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("Prologue".into()));
}

#[test]
fn task_manager_counts_tasks_without_blockers() {
    // IT task manager: how many tasks have an empty blockers list?
    let db = db();
    run(
        "CREATE (:Ticket {id: 1, blockers: []}),
                (:Ticket {id: 2, blockers: [1]}),
                (:Ticket {id: 3, blockers: []})",
        &db,
    );
    let rows = run(
        "MATCH (t:Ticket) WHERE isEmpty(t.blockers) RETURN count(*) AS unblocked",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(2));
}

#[test]
fn erp_flags_orders_with_no_line_items() {
    // ERP: an order with no line items is a draft.
    let db = db();
    run(
        "CREATE (:Order {ref: 'PO-1', items: ['widget']}),
                (:Order {ref: 'PO-2', items: []})",
        &db,
    );
    let rows = run(
        "MATCH (o:Order)
         RETURN o.ref AS ref, isEmpty(o.items) AS draft
         ORDER BY ref",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Value::Bool(false));
    assert_eq!(rows[1][1], Value::Bool(true));
}

#[test]
fn bug_tracker_finds_reports_without_labels() {
    // Bug tracker: triage reports that carry no labels yet.
    let db = db();
    run(
        "CREATE (:Bug {key: 'D-1', labels: ['regression']}),
                (:Bug {key: 'D-2', labels: []})",
        &db,
    );
    let rows = run(
        "MATCH (b:Bug) WHERE isEmpty(b.labels) RETURN b.key AS key",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("D-2".into()));
}
