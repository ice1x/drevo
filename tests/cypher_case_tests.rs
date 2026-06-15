//! End-to-end Cypher `CASE` expression tests — Phase 10 follow-up task `00137`.
//!
//! `CASE` is Cypher's conditional expression. Two forms:
//!
//! * **Generic** (`CASE WHEN cond THEN val [WHEN …] [ELSE val] END`) —
//!   evaluates each boolean `WHEN` condition in order and returns the `THEN`
//!   value of the first that is `true`. A `NULL` or `false` condition is
//!   skipped; a non-boolean condition is a type error.
//! * **Simple** (`CASE x WHEN v THEN val [WHEN …] [ELSE val] END`) —
//!   evaluates the scrutinee `x` once and returns the `THEN` value of the
//!   first `WHEN` value equal to it. `NULL` never matches (Cypher's
//!   three-valued equality), so a `NULL` scrutinee falls through to `ELSE`.
//!
//! When no arm matches and there is no `ELSE`, the result is `NULL`.
//!
//! These cases drive the real parser → executor pipeline across the five
//! drevo target scenario domains (CBT journal, story/book editor, IT task
//! manager, ERP, bug tracker) plus the cross-cutting semantics (ordering,
//! null handling, projection / WHERE / WITH placement, type errors).

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

fn exec(source: &str, drevo: &Drevo) {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute");
}

fn int(rows: &[Vec<Value>], r: usize, c: usize) -> i64 {
    match &rows[r][c] {
        Value::Integer(i) => *i,
        other => panic!("expected integer at ({r},{c}), got {other:?}"),
    }
}

fn string(rows: &[Vec<Value>], r: usize, c: usize) -> String {
    match &rows[r][c] {
        Value::String(s) => s.clone(),
        other => panic!("expected string at ({r},{c}), got {other:?}"),
    }
}

// ===== Generic (searched) form ==============================================

#[test]
fn generic_case_returns_first_true_arm() {
    let db = db();
    let rows = run(
        "RETURN CASE WHEN false THEN 'a' WHEN true THEN 'b' ELSE 'c' END AS r",
        &db,
    );
    assert_eq!(string(&rows, 0, 0), "b");
}

#[test]
fn generic_case_takes_else_when_no_arm_matches() {
    let db = db();
    let rows = run(
        "RETURN CASE WHEN false THEN 'a' WHEN false THEN 'b' ELSE 'c' END AS r",
        &db,
    );
    assert_eq!(string(&rows, 0, 0), "c");
}

#[test]
fn generic_case_without_else_yields_null() {
    let db = db();
    let rows = run("RETURN CASE WHEN false THEN 'a' END AS r", &db);
    assert_eq!(rows[0][0], Value::Null);
}

#[test]
fn generic_case_null_condition_is_skipped() {
    let db = db();
    // `null` is neither true nor false — it does not select the arm.
    let rows = run(
        "RETURN CASE WHEN null THEN 'a' WHEN true THEN 'b' ELSE 'c' END AS r",
        &db,
    );
    assert_eq!(string(&rows, 0, 0), "b");
}

#[test]
fn generic_case_returns_first_match_not_later_one() {
    let db = db();
    let rows = run(
        "RETURN CASE WHEN true THEN 1 WHEN true THEN 2 ELSE 3 END AS r",
        &db,
    );
    assert_eq!(int(&rows, 0, 0), 1);
}

#[test]
fn generic_case_evaluates_comparison_conditions() {
    let db = db();
    let rows = run(
        "WITH 7 AS x RETURN CASE WHEN x < 5 THEN 'low' WHEN x < 10 THEN 'mid' ELSE 'high' END AS bucket",
        &db,
    );
    assert_eq!(string(&rows, 0, 0), "mid");
}

#[test]
fn generic_case_non_boolean_condition_is_type_error() {
    let db = db();
    let err = run_err("RETURN CASE WHEN 1 THEN 'a' ELSE 'b' END AS r", &db);
    assert!(
        matches!(err, ExecError::TypeMismatch { .. }),
        "expected TypeMismatch, got {err:?}"
    );
}

// ===== Simple form ==========================================================

#[test]
fn simple_case_matches_scrutinee_by_equality() {
    let db = db();
    let rows = run(
        "WITH 2 AS x RETURN CASE x WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'many' END AS r",
        &db,
    );
    assert_eq!(string(&rows, 0, 0), "two");
}

#[test]
fn simple_case_falls_to_else_when_unmatched() {
    let db = db();
    let rows = run(
        "WITH 9 AS x RETURN CASE x WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'many' END AS r",
        &db,
    );
    assert_eq!(string(&rows, 0, 0), "many");
}

#[test]
fn simple_case_without_else_yields_null_when_unmatched() {
    let db = db();
    let rows = run("WITH 9 AS x RETURN CASE x WHEN 1 THEN 'one' END AS r", &db);
    assert_eq!(rows[0][0], Value::Null);
}

#[test]
fn simple_case_null_scrutinee_never_matches() {
    let db = db();
    // `null = null` is `null`, not `true`, so a null scrutinee falls to ELSE
    // even against a `WHEN null` arm.
    let rows = run(
        "WITH null AS x RETURN CASE x WHEN null THEN 'isnull' ELSE 'other' END AS r",
        &db,
    );
    assert_eq!(string(&rows, 0, 0), "other");
}

#[test]
fn simple_case_matches_string_scrutinee() {
    let db = db();
    let rows = run(
        "WITH 'g' AS grade RETURN CASE grade WHEN 'a' THEN 4 WHEN 'g' THEN 0 ELSE -1 END AS gpa",
        &db,
    );
    assert_eq!(int(&rows, 0, 0), 0);
}

// ===== Placement: WHERE / WITH / nesting ====================================

#[test]
fn case_usable_in_where_predicate() {
    let db = db();
    exec("CREATE (:Item {title: 'a', n: 1})", &db);
    exec("CREATE (:Item {title: 'b', n: 2})", &db);
    exec("CREATE (:Item {title: 'c', n: 3})", &db);
    let rows = run(
        "MATCH (i:Item) WHERE (CASE WHEN i.n > 1 THEN true ELSE false END) \
         RETURN i.title AS t ORDER BY t",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0), "b");
    assert_eq!(string(&rows, 1, 0), "c");
}

#[test]
fn case_usable_in_with_projection() {
    let db = db();
    let rows = run(
        "WITH 3 AS n WITH CASE WHEN n % 2 = 0 THEN 'even' ELSE 'odd' END AS parity \
         RETURN parity",
        &db,
    );
    assert_eq!(string(&rows, 0, 0), "odd");
}

#[test]
fn case_arms_can_contain_arithmetic() {
    let db = db();
    let rows = run(
        "WITH 4 AS x RETURN CASE WHEN x > 0 THEN x * 10 ELSE 0 - x END AS r",
        &db,
    );
    assert_eq!(int(&rows, 0, 0), 40);
}

#[test]
fn case_can_nest_in_then_branch() {
    let db = db();
    let rows = run(
        "WITH 2 AS x, 5 AS y \
         RETURN CASE WHEN x > 0 THEN CASE WHEN y > 3 THEN 'pos-big' ELSE 'pos-small' END \
                     ELSE 'neg' END AS r",
        &db,
    );
    assert_eq!(string(&rows, 0, 0), "pos-big");
}

// ===== Scenario domains =====================================================

#[test]
fn cbt_distortion_severity_bucketing() {
    // CBT journal: bucket a thought record's intensity into a severity band.
    let db = db();
    exec(
        "CREATE (:Thought {title: 'I always fail', intensity: 90})",
        &db,
    );
    exec(
        "CREATE (:Thought {title: 'Could be better', intensity: 40})",
        &db,
    );
    let rows = run(
        "MATCH (t:Thought) \
         RETURN t.title AS thought, \
                CASE WHEN t.intensity >= 70 THEN 'severe' \
                     WHEN t.intensity >= 30 THEN 'moderate' \
                     ELSE 'mild' END AS band \
         ORDER BY thought",
        &db,
    );
    assert_eq!(rows.len(), 2);
    // 'Could be better' (40 -> moderate), 'I always fail' (90 -> severe)
    assert_eq!(string(&rows, 0, 1), "moderate");
    assert_eq!(string(&rows, 1, 1), "severe");
}

#[test]
fn story_chapter_status_label() {
    // Story editor: map a chapter's word count to a drafting status.
    let db = db();
    exec("CREATE (:Chapter {title: 'Prologue', words: 0})", &db);
    exec("CREATE (:Chapter {title: 'Rising', words: 2500})", &db);
    let rows = run(
        "MATCH (c:Chapter) \
         RETURN c.title AS chapter, \
                CASE c.words WHEN 0 THEN 'empty' ELSE 'drafted' END AS status \
         ORDER BY chapter",
        &db,
    );
    // Ordered by title: 'Prologue' (0 -> empty), 'Rising' (2500 -> drafted).
    assert_eq!(string(&rows, 0, 1), "empty");
    assert_eq!(string(&rows, 1, 1), "drafted");
}

#[test]
fn task_priority_to_sla_hours() {
    // IT task manager: translate a textual priority into an SLA in hours.
    let db = db();
    exec("CREATE (:Task {title: 'Outage', priority: 'P1'})", &db);
    exec("CREATE (:Task {title: 'Typo', priority: 'P3'})", &db);
    let rows = run(
        "MATCH (t:Task) \
         RETURN t.title AS task, \
                CASE t.priority WHEN 'P1' THEN 1 WHEN 'P2' THEN 8 ELSE 72 END AS sla \
         ORDER BY task",
        &db,
    );
    // 'Outage' (P1 -> 1), 'Typo' (P3 -> 72)
    assert_eq!(int(&rows, 0, 1), 1);
    assert_eq!(int(&rows, 1, 1), 72);
}

#[test]
fn erp_order_discount_tier() {
    // ERP: assign a discount tier from an order total.
    let db = db();
    exec("CREATE (:Order {title: 'PO-1', total: 12000})", &db);
    exec("CREATE (:Order {title: 'PO-2', total: 500})", &db);
    let rows = run(
        "MATCH (o:Order) \
         RETURN o.title AS po, \
                CASE WHEN o.total >= 10000 THEN 'gold' \
                     WHEN o.total >= 1000 THEN 'silver' \
                     ELSE 'standard' END AS tier \
         ORDER BY po",
        &db,
    );
    assert_eq!(string(&rows, 0, 1), "gold");
    assert_eq!(string(&rows, 1, 1), "standard");
}

#[test]
fn bug_state_to_open_flag() {
    // Bug tracker: collapse several states into an is-open boolean.
    let db = db();
    exec("CREATE (:Bug {title: 'B-1', state: 'open'})", &db);
    exec("CREATE (:Bug {title: 'B-2', state: 'closed'})", &db);
    exec("CREATE (:Bug {title: 'B-3', state: 'in_progress'})", &db);
    let rows = run(
        "MATCH (b:Bug) \
         RETURN b.title AS bug, \
                CASE b.state WHEN 'closed' THEN false ELSE true END AS is_open \
         ORDER BY bug",
        &db,
    );
    assert_eq!(rows[0][1], Value::Bool(true));
    assert_eq!(rows[1][1], Value::Bool(false));
    assert_eq!(rows[2][1], Value::Bool(true));
}
