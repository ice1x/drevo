//! End-to-end Cypher `isNaN()` numeric-predicate tests — Phase 10 follow-up
//! task `00162`.
//!
//! Neo4j 5 exposes `isNaN(input)`, the boolean predicate that tells a NaN
//! floating-point value apart from every other number. It is the natural
//! companion to the trigonometric / logarithmic library (task `00156`), whose
//! domain edges (`sqrt(-1)`, `log(-1)`, `asin(2)`, …) follow IEEE-754 and
//! produce exactly the `NaN` this predicate detects, and to float division
//! (`0.0/0.0` → `NaN`). Without it there is no way to test for NaN in Cypher,
//! because — per IEEE-754 — `NaN = NaN` is *false*, so an equality comparison
//! can never catch it.
//!
//! Semantics mirror Neo4j:
//!
//! * A `Float` that is NaN → `true`; any other `Float` (including `±Infinity`)
//!   → `false`.
//! * An `Integer` is never NaN → always `false`.
//! * `NULL` propagates → `NULL` (every built-in scalar except `coalesce` is
//!   NULL-propagating).
//! * A non-numeric argument (String, Bool, List, Map, …) is a recoverable
//!   `InvalidFunctionCall`, never a panic.
//! * Exactly one argument; any other arity is a recoverable error.
//!
//! These tests drive the real parser → executor pipeline, and exercise the
//! five drevo target scenario domains (CBT journal, story/book editor, IT task
//! manager, ERP, bug tracker).

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

// ---------------------------------------------------------------------------
// Core contract
// ---------------------------------------------------------------------------

#[test]
fn isnan_true_for_nan_producing_expressions() {
    let db = db();
    // Float division 0.0/0.0, and the IEEE-754 domain edges of the math
    // library (task `00156`) all yield NaN.
    for expr in [
        "0.0 / 0.0",
        "sqrt(-1.0)",
        "log(-1.0)",
        "asin(2.0)",
        "acos(2.0)",
    ] {
        let q = format!("RETURN isNaN({expr}) AS v");
        assert_eq!(
            one(&q, &db),
            Value::Bool(true),
            "isNaN({expr}) should be true"
        );
    }
}

#[test]
fn isnan_false_for_ordinary_floats() {
    let db = db();
    for expr in ["0.0", "3.14", "-2.5", "1.0e308", "sqrt(2.0)"] {
        let q = format!("RETURN isNaN({expr}) AS v");
        assert_eq!(
            one(&q, &db),
            Value::Bool(false),
            "isNaN({expr}) should be false"
        );
    }
}

#[test]
fn isnan_false_for_infinities() {
    let db = db();
    // ±Infinity are numbers, not NaN — Neo4j returns false for both.
    assert_eq!(one("RETURN isNaN(1.0 / 0.0) AS v", &db), Value::Bool(false));
    assert_eq!(
        one("RETURN isNaN(-1.0 / 0.0) AS v", &db),
        Value::Bool(false)
    );
}

#[test]
fn isnan_false_for_integers() {
    let db = db();
    // An Integer can never be NaN.
    for expr in ["0", "42", "-7", "9223372036854775807"] {
        let q = format!("RETURN isNaN({expr}) AS v");
        assert_eq!(
            one(&q, &db),
            Value::Bool(false),
            "isNaN({expr}) should be false (integers are never NaN)"
        );
    }
}

#[test]
fn isnan_null_propagates() {
    let db = db();
    assert_eq!(one("RETURN isNaN(null) AS v", &db), Value::Null);
}

#[test]
fn isnan_is_case_insensitive() {
    let db = db();
    for name in ["isNaN", "ISNAN", "isnan", "IsNaN"] {
        let q = format!("RETURN {name}(0.0 / 0.0) AS v");
        assert_eq!(one(&q, &db), Value::Bool(true), "name {name} should work");
    }
}

#[test]
fn isnan_rejects_non_numeric_argument() {
    let db = db();
    for expr in ["'hello'", "true", "[1, 2, 3]", "{a: 1}"] {
        let q = format!("RETURN isNaN({expr}) AS v");
        assert!(
            matches!(run_err(&q, &db), ExecError::InvalidFunctionCall { .. }),
            "isNaN({expr}) should be an InvalidFunctionCall"
        );
    }
}

#[test]
fn isnan_rejects_wrong_arity() {
    let db = db();
    assert!(matches!(
        run_err("RETURN isNaN() AS v", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
    assert!(matches!(
        run_err("RETURN isNaN(1.0, 2.0) AS v", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
}

#[test]
fn isnan_composes_in_predicates() {
    let db = db();
    // Used inside a boolean expression / negation.
    assert_eq!(one("RETURN NOT isNaN(1.5) AS v", &db), Value::Bool(true));
    assert_eq!(
        one("RETURN isNaN(0.0 / 0.0) AND true AS v", &db),
        Value::Bool(true)
    );
}

#[test]
fn isnan_in_where_filters_nan_rows() {
    let db = db();
    // UNWIND a mix of computed values; WHERE isNaN(x) keeps only the NaN ones.
    let rows = run(
        "UNWIND [sqrt(-1.0), sqrt(4.0), 0.0 / 0.0, 1.0 / 1.0] AS x \
         WITH x WHERE isNaN(x) RETURN count(*) AS c",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(2));
}

// ---------------------------------------------------------------------------
// Scenario-domain coverage (the five drevo target use cases)
// ---------------------------------------------------------------------------

#[test]
fn scenario_cbt_flags_undefined_mood_average() {
    // CBT journal: a day with zero entries has an undefined average mood
    // (sum 0.0 / count 0.0 = NaN). isNaN lets a report distinguish "no data"
    // from a genuine 0.0 average.
    let db = db();
    let v = one(
        "WITH 0.0 AS sum_mood, 0.0 AS entries \
         RETURN isNaN(sum_mood / entries) AS no_data",
        &db,
    );
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn scenario_story_word_count_ratio_is_finite() {
    // Story editor: chapters/words ratio for a real manuscript is a finite
    // number, so isNaN is false.
    let db = db();
    run("CREATE (c:Chapter {words: 3200, scenes: 8})", &db);
    let v = one(
        "MATCH (c:Chapter) RETURN isNaN(toFloat(c.words) / toFloat(c.scenes)) AS v",
        &db,
    );
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn scenario_task_manager_velocity_without_history() {
    // IT task manager: velocity = completed / sprints. A brand-new project
    // with 0 sprints yields NaN, which a dashboard should surface as "n/a".
    let db = db();
    run("CREATE (p:Project {completed: 0.0, sprints: 0.0})", &db);
    let v = one(
        "MATCH (p:Project) RETURN isNaN(p.completed / p.sprints) AS na",
        &db,
    );
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn scenario_erp_margin_per_unit_is_real_number() {
    // ERP: margin per unit for a shipped order is a real number.
    let db = db();
    run(
        "CREATE (o:Order {revenue: 5000.0, units: 25.0, cost: 3000.0})",
        &db,
    );
    let v = one(
        "MATCH (o:Order) RETURN isNaN((o.revenue - o.cost) / o.units) AS v",
        &db,
    );
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn scenario_bug_tracker_reopen_rate_guard() {
    // Bug tracker: reopen-rate = reopened / resolved. A component with nothing
    // resolved yet has an undefined rate; isNaN guards the division so the
    // report can show "—" instead of a misleading number.
    let db = db();
    run(
        "CREATE (:Component {name: 'auth', reopened: 0.0, resolved: 0.0}) \
         CREATE (:Component {name: 'ui', reopened: 2.0, resolved: 10.0})",
        &db,
    );
    let rows = run(
        "MATCH (c:Component) \
         RETURN c.name AS name, isNaN(c.reopened / c.resolved) AS undefined \
         ORDER BY name",
        &db,
    );
    assert_eq!(rows.len(), 2);
    // auth: 0/0 = NaN -> true
    assert_eq!(rows[0][0], Value::String("auth".into()));
    assert_eq!(rows[0][1], Value::Bool(true));
    // ui: 2/10 = 0.2 -> false
    assert_eq!(rows[1][0], Value::String("ui".into()));
    assert_eq!(rows[1][1], Value::Bool(false));
}
