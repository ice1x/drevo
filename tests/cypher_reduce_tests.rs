//! End-to-end Cypher `reduce` tests — Phase 10 follow-up task `00148`.
//!
//! `reduce(accumulator = init, var IN list | expr)` is a left fold over a
//! list: the seed `init` primes the accumulator, then each element is bound to
//! `var` and the running total to `accumulator` in a child scope while `expr`
//! computes the next accumulator value. The final accumulator is the result.
//!
//! It is the third member of the list-expression family alongside the list
//! comprehension (`00146`, list → list) and the list predicates (`00147`,
//! list → boolean): `reduce` collapses a list into a single arbitrary value
//! (a sum, a product, a concatenation, a max, …).
//!
//! Semantics exercised here:
//!
//! * left-to-right fold, seed visible from the first iteration,
//! * empty list → the seed unchanged,
//! * `null` list → `null` (mirrors `UNWIND` / `IN` / the comprehension family),
//! * non-list → a recoverable `TypeMismatch`,
//! * outer-scope capture, nesting, parameters, and folding over a `collect`ed
//!   property list across the five drevo target scenario domains.

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

// ===== Core semantics =======================================================

#[test]
fn reduce_sums_a_list() {
    let d = db();
    assert_eq!(
        cell(&run("RETURN reduce(s = 0, x IN [1, 2, 3, 4] | s + x)", &d)),
        Value::Integer(10)
    );
}

#[test]
fn reduce_multiplies_with_a_nonzero_seed() {
    let d = db();
    assert_eq!(
        cell(&run("RETURN reduce(p = 1, x IN [1, 2, 3, 4] | p * x)", &d)),
        Value::Integer(24)
    );
}

#[test]
fn reduce_is_a_left_fold_order_matters() {
    let d = db();
    // String concatenation is non-commutative, so this pins down left-to-right
    // order: ((("" + "a") + "b") + "c").
    assert_eq!(
        cell(&run(
            "RETURN reduce(acc = '', w IN ['a', 'b', 'c'] | acc + w)",
            &d
        )),
        Value::String("abc".into())
    );
}

#[test]
fn reduce_over_empty_list_returns_the_seed() {
    let d = db();
    assert_eq!(
        cell(&run("RETURN reduce(s = 42, x IN [] | s + x)", &d)),
        Value::Integer(42)
    );
}

#[test]
fn reduce_seed_can_reference_a_bound_value() {
    let d = db();
    // The accumulator seed is evaluated in the outer scope before the fold.
    assert_eq!(
        cell(&run(
            "WITH 100 AS base RETURN reduce(s = base, x IN [1, 2, 3] | s + x)",
            &d
        )),
        Value::Integer(106)
    );
}

#[test]
fn reduce_can_compute_a_running_max() {
    let d = db();
    assert_eq!(
        cell(&run(
            "RETURN reduce(m = 0, x IN [3, 9, 2, 7] | CASE WHEN x > m THEN x ELSE m END)",
            &d
        )),
        Value::Integer(9)
    );
}

// ===== Null & type handling =================================================

#[test]
fn reduce_over_null_list_is_null() {
    let d = db();
    assert_eq!(
        cell(&run("RETURN reduce(s = 0, x IN null | s + x)", &d)),
        Value::Null
    );
}

#[test]
fn reduce_propagates_null_from_the_fold_expression() {
    let d = db();
    // Adding a null element makes the accumulator null, which then sticks.
    assert_eq!(
        cell(&run("RETURN reduce(s = 0, x IN [1, null, 3] | s + x)", &d)),
        Value::Null
    );
}

#[test]
fn reduce_over_non_list_is_a_type_error() {
    let d = db();
    assert!(matches!(
        run_err("RETURN reduce(s = 0, x IN 7 | s + x)", &d),
        ExecError::TypeMismatch { .. }
    ));
}

// ===== Scope, nesting, parameters ==========================================

#[test]
fn reduce_loop_variable_shadows_outer_binding() {
    let d = db();
    // `x` is bound outside; inside the fold it ranges over the list, and the
    // outer `x` is untouched afterwards.
    let rows = run(
        "WITH 99 AS x RETURN reduce(s = 0, x IN [1, 2, 3] | s + x) AS total, x AS outer",
        &d,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(6));
    assert_eq!(rows[0][1], Value::Integer(99));
}

#[test]
fn reduce_can_capture_an_outer_value_in_the_fold_expression() {
    let d = db();
    assert_eq!(
        cell(&run(
            "WITH 10 AS step RETURN reduce(s = 0, x IN [1, 2, 3] | s + x * step)",
            &d
        )),
        Value::Integer(60)
    );
}

#[test]
fn reduce_nested_inside_reduce() {
    let d = db();
    // Sum each inner pair, then sum the partial sums: (1+2)+(3+4) = 10.
    assert_eq!(
        cell(&run(
            "RETURN reduce(outer = 0, pair IN [[1, 2], [3, 4]] \
             | outer + reduce(inner = 0, y IN pair | inner + y))",
            &d
        )),
        Value::Integer(10)
    );
}

#[test]
fn reduce_list_can_come_from_a_parameter() {
    let d = db();
    let mut params = HashMap::new();
    params.insert(
        "xs".to_string(),
        Value::List(vec![
            Value::Integer(5),
            Value::Integer(10),
            Value::Integer(15),
        ]),
    );
    assert_eq!(
        cell(&run_with_params(
            "RETURN reduce(s = 0, x IN $xs | s + x)",
            &d,
            params
        )),
        Value::Integer(30)
    );
}

#[test]
fn reduce_over_a_range() {
    let d = db();
    assert_eq!(
        cell(&run("RETURN reduce(s = 0, x IN range(1, 5) | s + x)", &d)),
        Value::Integer(15)
    );
}

// ===== Scenario-domain workflows ===========================================

#[test]
fn erp_total_order_value_from_line_item_subtotals() {
    let d = db();
    // ERP: an order with several line items; fold the per-line subtotals into
    // an order total. Mirrors a real "sum the basket" report.
    run(
        "CREATE (o:Order {ref: 'SO-1'})
         CREATE (o)-[:HAS_LINE]->(:Line {subtotal: 1200})
         CREATE (o)-[:HAS_LINE]->(:Line {subtotal: 350})
         CREATE (o)-[:HAS_LINE]->(:Line {subtotal: 99})",
        &d,
    );
    let rows = run(
        "MATCH (o:Order {ref: 'SO-1'})-[:HAS_LINE]->(l:Line)
         WITH o, collect(l.subtotal) AS subtotals
         RETURN reduce(total = 0, s IN subtotals | total + s) AS order_total",
        &d,
    );
    assert_eq!(cell(&rows), Value::Integer(1649));
}

#[test]
fn task_manager_total_estimated_effort() {
    let d = db();
    // IT task manager: a sprint's stories each carry an estimate; fold them
    // into the sprint's total committed effort.
    run(
        "CREATE (sp:Sprint {name: 'S-7'})
         CREATE (sp)-[:CONTAINS]->(:Story {points: 3})
         CREATE (sp)-[:CONTAINS]->(:Story {points: 5})
         CREATE (sp)-[:CONTAINS]->(:Story {points: 8})",
        &d,
    );
    let rows = run(
        "MATCH (sp:Sprint {name: 'S-7'})-[:CONTAINS]->(st:Story)
         WITH collect(st.points) AS points
         RETURN reduce(sum = 0, p IN points | sum + p) AS committed",
        &d,
    );
    assert_eq!(cell(&rows), Value::Integer(16));
}

#[test]
fn story_editor_concatenates_chapter_titles() {
    let d = db();
    // Story/book editor: build a table-of-contents string by folding the
    // ordered chapter titles with a separator.
    let rows = run(
        "RETURN reduce(toc = 'Contents:', t IN ['Dawn', 'Noon', 'Dusk'] | toc + ' ' + t) AS toc",
        &d,
    );
    assert_eq!(
        cell(&rows),
        Value::String("Contents: Dawn Noon Dusk".into())
    );
}

#[test]
fn bug_tracker_counts_high_severity_via_fold() {
    let d = db();
    // Bug tracker: fold a severity list into a count of the high-severity ones
    // — a reduce standing in for a filtered count.
    assert_eq!(
        cell(&run(
            "RETURN reduce(n = 0, sev IN ['low', 'high', 'high', 'medium'] \
             | CASE WHEN sev = 'high' THEN n + 1 ELSE n END) AS high_count",
            &d
        )),
        Value::Integer(2)
    );
}

#[test]
fn cbt_journal_average_mood_via_reduce_and_size() {
    let d = db();
    // CBT journal: average a week's mood scores by folding the sum and
    // dividing by the count.
    run(
        "CREATE (:Mood {day: 1, score: 4})
         CREATE (:Mood {day: 2, score: 6})
         CREATE (:Mood {day: 3, score: 8})",
        &d,
    );
    let rows = run(
        "MATCH (m:Mood)
         WITH collect(m.score) AS scores
         RETURN reduce(s = 0, x IN scores | s + x) / size(scores) AS avg_mood",
        &d,
    );
    assert_eq!(cell(&rows), Value::Integer(6));
}
