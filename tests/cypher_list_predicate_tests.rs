//! End-to-end Cypher list-predicate tests — Phase 10 follow-up task `00147`.
//!
//! The list predicate functions `all(var IN list WHERE pred)`,
//! `any(...)`, `none(...)`, and `single(...)` evaluate `list`, bind each
//! element to `var` in a child scope, evaluate the mandatory `WHERE`
//! predicate under three-valued logic, and fold the per-element results into
//! a single boolean (or `NULL`) per the quantifier:
//!
//! * `all`    — every element satisfies the predicate (empty list → `true`),
//! * `any`    — some element satisfies it (empty list → `false`),
//! * `none`   — no element satisfies it (empty list → `true`),
//! * `single` — exactly one element satisfies it (empty list → `false`).
//!
//! They are the boolean siblings of the list comprehension (`00146`): where a
//! comprehension transforms a list into a list, a predicate collapses a list
//! into a `WHERE`-grade truth value, most often inside a `WHERE` clause that
//! filters rows by a property *collection*.
//!
//! These cases drive the real parser → executor pipeline across the
//! cross-cutting semantics (each quantifier, empty / null lists, three-valued
//! logic with null elements, type errors, outer-scope capture, nesting,
//! parameters) plus the five drevo target scenario domains (CBT journal,
//! story/book editor, IT task manager, ERP, bug tracker).

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

fn run_with_params(source: &str, drevo: &Drevo, params: HashMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, params).expect("execute").rows
}

fn run_err(source: &str, drevo: &Drevo) -> ExecError {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect_err("expected execution error")
}

/// The single scalar value of a one-row, one-column query.
fn cell(rows: &[Vec<Value>]) -> Value {
    assert_eq!(rows.len(), 1, "expected exactly one row");
    assert_eq!(rows[0].len(), 1, "expected exactly one column");
    rows[0][0].clone()
}

/// The string values of a one-column result, in row order.
fn col_strings(rows: &[Vec<Value>]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected string cell, got {other:?}"),
        })
        .collect()
}

// ===== Core semantics =======================================================

#[test]
fn all_is_true_only_when_every_element_satisfies() {
    let d = db();
    assert_eq!(
        cell(&run("RETURN all(x IN [2, 4, 6] WHERE x % 2 = 0)", &d)),
        Value::Bool(true)
    );
    assert_eq!(
        cell(&run("RETURN all(x IN [2, 3, 6] WHERE x % 2 = 0)", &d)),
        Value::Bool(false)
    );
}

#[test]
fn any_is_true_when_some_element_satisfies() {
    let d = db();
    assert_eq!(
        cell(&run("RETURN any(x IN [1, 3, 4] WHERE x % 2 = 0)", &d)),
        Value::Bool(true)
    );
    assert_eq!(
        cell(&run("RETURN any(x IN [1, 3, 5] WHERE x % 2 = 0)", &d)),
        Value::Bool(false)
    );
}

#[test]
fn none_is_the_negation_of_any() {
    let d = db();
    assert_eq!(
        cell(&run("RETURN none(x IN [1, 3, 5] WHERE x % 2 = 0)", &d)),
        Value::Bool(true)
    );
    assert_eq!(
        cell(&run("RETURN none(x IN [1, 2, 5] WHERE x % 2 = 0)", &d)),
        Value::Bool(false)
    );
}

#[test]
fn single_is_true_only_for_exactly_one_match() {
    let d = db();
    assert_eq!(
        cell(&run("RETURN single(x IN [1, 2, 3] WHERE x % 2 = 0)", &d)),
        Value::Bool(true)
    );
    // Two matches.
    assert_eq!(
        cell(&run("RETURN single(x IN [2, 3, 4] WHERE x % 2 = 0)", &d)),
        Value::Bool(false)
    );
    // Zero matches.
    assert_eq!(
        cell(&run("RETURN single(x IN [1, 3, 5] WHERE x % 2 = 0)", &d)),
        Value::Bool(false)
    );
}

#[test]
fn empty_list_uses_each_quantifier_identity() {
    let d = db();
    assert_eq!(
        cell(&run("RETURN all(x IN [] WHERE x > 0)", &d)),
        Value::Bool(true)
    );
    assert_eq!(
        cell(&run("RETURN any(x IN [] WHERE x > 0)", &d)),
        Value::Bool(false)
    );
    assert_eq!(
        cell(&run("RETURN none(x IN [] WHERE x > 0)", &d)),
        Value::Bool(true)
    );
    assert_eq!(
        cell(&run("RETURN single(x IN [] WHERE x > 0)", &d)),
        Value::Bool(false)
    );
}

#[test]
fn null_list_propagates_null_for_every_quantifier() {
    let d = db();
    for kw in ["all", "any", "none", "single"] {
        assert_eq!(
            cell(&run(&format!("RETURN {kw}(x IN null WHERE x > 0)"), &d)),
            Value::Null,
            "{kw} over a null list must be null"
        );
    }
}

#[test]
fn three_valued_logic_with_null_elements() {
    let d = db();
    // No definite false, but an unknown → all is unknown.
    assert_eq!(
        cell(&run("RETURN all(x IN [1, null, 3] WHERE x > 0)", &d)),
        Value::Null
    );
    // No definite true, but an unknown → any is unknown.
    assert_eq!(
        cell(&run("RETURN any(x IN [-1, null, -3] WHERE x > 0)", &d)),
        Value::Null
    );
    // A definite false short-circuits all regardless of the unknown.
    assert_eq!(
        cell(&run("RETURN all(x IN [-1, null, 3] WHERE x > 0)", &d)),
        Value::Bool(false)
    );
    // A definite true short-circuits any regardless of the unknown.
    assert_eq!(
        cell(&run("RETURN any(x IN [1, null, -3] WHERE x > 0)", &d)),
        Value::Bool(true)
    );
    // single: one definite true but an unknown could tip the count.
    assert_eq!(
        cell(&run("RETURN single(x IN [1, null, -3] WHERE x > 0)", &d)),
        Value::Null
    );
}

#[test]
fn predicate_captures_outer_row_variables() {
    let d = db();
    // The threshold comes from the outer scope, not the loop variable.
    let mut params = HashMap::new();
    params.insert("limit".to_string(), Value::Integer(5));
    let rows = run_with_params(
        "RETURN any(x IN [1, 4, 9] WHERE x > $limit) AS r",
        &d,
        params,
    );
    assert_eq!(cell(&rows), Value::Bool(true));
}

#[test]
fn nested_list_predicate_over_list_of_lists() {
    let d = db();
    // Every inner list must itself be all-positive.
    assert_eq!(
        cell(&run(
            "RETURN all(row IN [[1, 2], [3, 4]] WHERE all(c IN row WHERE c > 0))",
            &d
        )),
        Value::Bool(true)
    );
    assert_eq!(
        cell(&run(
            "RETURN all(row IN [[1, 2], [3, -4]] WHERE all(c IN row WHERE c > 0))",
            &d
        )),
        Value::Bool(false)
    );
}

#[test]
fn non_list_argument_is_a_type_error() {
    let d = db();
    assert!(matches!(
        run_err("RETURN all(x IN 7 WHERE x > 0)", &d),
        ExecError::TypeMismatch { .. }
    ));
}

#[test]
fn non_boolean_predicate_is_a_type_error() {
    let d = db();
    assert!(matches!(
        run_err("RETURN any(x IN [1, 2, 3] WHERE x + 1)", &d),
        ExecError::TypeMismatch { .. }
    ));
}

// ===== Scenario: CBT journal ===============================================

#[test]
fn cbt_thought_records_with_no_remaining_distortions() {
    let d = db();
    run(
        "CREATE (:Thought {summary: 'reframed', distortions: []})",
        &d,
    );
    run(
        "CREATE (:Thought {summary: 'work', distortions: ['catastrophising', 'mind-reading']})",
        &d,
    );
    // `none(... WHERE true)` over a non-empty list is false, over an empty
    // list is true: thoughts the patient has fully reframed.
    let rows = run(
        "MATCH (t:Thought) WHERE none(dz IN t.distortions WHERE true) RETURN t.summary AS s",
        &d,
    );
    assert_eq!(col_strings(&rows), vec!["reframed"]);
}

// ===== Scenario: story / book editor =======================================

#[test]
fn story_chapters_all_above_a_word_count_floor() {
    let d = db();
    run(
        "CREATE (:Book {title: 'Long', chapter_words: [1200, 1500, 1100]})",
        &d,
    );
    run(
        "CREATE (:Book {title: 'Uneven', chapter_words: [1200, 400, 1100]})",
        &d,
    );
    // Books whose every chapter clears 1000 words.
    let rows = run(
        "MATCH (b:Book) WHERE all(w IN b.chapter_words WHERE w >= 1000) RETURN b.title AS t",
        &d,
    );
    assert_eq!(col_strings(&rows), vec!["Long"]);
}

// ===== Scenario: IT task manager ============================================

#[test]
fn task_sprint_with_exactly_one_blocker() {
    let d = db();
    run(
        "CREATE (:Sprint {name: 'S1', blocked: [false, true, false]})",
        &d,
    );
    run(
        "CREATE (:Sprint {name: 'S2', blocked: [true, true, false]})",
        &d,
    );
    run(
        "CREATE (:Sprint {name: 'S3', blocked: [false, false, false]})",
        &d,
    );
    // Sprints with exactly one blocked task — the ones worth a single nudge.
    let rows = run(
        "MATCH (s:Sprint) WHERE single(b IN s.blocked WHERE b = true) RETURN s.name AS n ORDER BY n",
        &d,
    );
    assert_eq!(col_strings(&rows), vec!["S1"]);
}

// ===== Scenario: ERP ========================================================

#[test]
fn erp_orders_with_any_backordered_line() {
    let d = db();
    run("CREATE (:Order {ref: 'O-1', stock: [10, 0, 4]})", &d);
    run("CREATE (:Order {ref: 'O-2', stock: [10, 5, 4]})", &d);
    // Orders with at least one out-of-stock line item.
    let rows = run(
        "MATCH (o:Order) WHERE any(q IN o.stock WHERE q = 0) RETURN o.ref AS r",
        &d,
    );
    assert_eq!(col_strings(&rows), vec!["O-1"]);
}

// ===== Scenario: bug tracker ===============================================

#[test]
fn bug_reports_where_all_repro_steps_confirmed() {
    let d = db();
    run(
        "CREATE (:Bug {id: 'B-1', confirmed: [true, true, true]})",
        &d,
    );
    run(
        "CREATE (:Bug {id: 'B-2', confirmed: [true, false, true]})",
        &d,
    );
    // Bugs whose every reproduction step has been independently confirmed.
    let rows = run(
        "MATCH (b:Bug) WHERE all(c IN b.confirmed WHERE c = true) RETURN b.id AS id",
        &d,
    );
    assert_eq!(col_strings(&rows), vec!["B-1"]);
}

#[test]
fn list_predicate_composes_with_return_projection_and_aggregation() {
    let d = db();
    run("CREATE (:Order {ref: 'A', stock: [1, 2, 3]})", &d);
    run("CREATE (:Order {ref: 'B', stock: [0, 2, 3]})", &d);
    run("CREATE (:Order {ref: 'C', stock: [4, 5, 6]})", &d);
    // Count how many orders are fully in stock (every line > 0).
    let rows = run(
        "MATCH (o:Order) RETURN count(CASE WHEN all(q IN o.stock WHERE q > 0) THEN 1 END) AS ok",
        &d,
    );
    assert_eq!(cell(&rows), Value::Integer(2));
}
