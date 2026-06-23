//! End-to-end Cypher trigonometric & logarithmic math-function tests — Phase 10
//! follow-up task `00156`.
//!
//! Task `00138` shipped the numeric scalar family (`abs` / `ceil` / `floor` /
//! `round` / `sign` / `sqrt`). This task completes it with Neo4j's
//! trigonometric and logarithmic functions:
//!
//! * **Exponential / logarithmic:** `e()`, `exp(x)`, `log(x)` (natural log),
//!   `log10(x)`.
//! * **Trigonometric:** `sin(x)`, `cos(x)`, `tan(x)`, `cot(x)`, `asin(x)`,
//!   `acos(x)`, `atan(x)`, `atan2(y, x)`.
//! * **Angle helpers:** `degrees(x)`, `radians(x)`, `pi()`, `haversin(x)`.
//!
//! Every function returns a `Float` and is NULL-propagating (a `NULL`
//! argument yields `NULL`, never an error). A non-numeric argument is a
//! recoverable `ExecError::InvalidFunctionCall`. Domain edges (`log(-1)`,
//! `asin(2)`, …) follow Neo4j and the IEEE-754 `f64` math: they return
//! `NaN` / ±`Infinity` floats rather than erroring.
//!
//! These cases drive the real parser → executor pipeline across the five
//! drevo target scenario domains (CBT journal, story/book editor, IT task
//! manager, ERP, bug tracker) plus the cross-cutting semantics.

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

/// One-row, one-column projection helper.
fn one(source: &str, drevo: &Drevo) -> Value {
    let rows = run(source, drevo);
    assert_eq!(rows.len(), 1, "expected exactly one row from {source:?}");
    rows[0][0].clone()
}

/// Pull a `Float` out of a one-row projection, failing on any other shape.
fn one_float(source: &str, drevo: &Drevo) -> f64 {
    match one(source, drevo) {
        Value::Float(f) => f,
        other => panic!("expected Float from {source:?}, got {other:?}"),
    }
}

/// Assert two floats agree to within a small absolute tolerance.
fn approx(got: f64, want: f64) {
    assert!(
        (got - want).abs() < 1e-9,
        "expected ~{want}, got {got} (delta {})",
        (got - want).abs()
    );
}

// ===== Constants ============================================================

#[test]
fn pi_and_e_constants() {
    let db = db();
    approx(one_float("RETURN pi() AS v", &db), std::f64::consts::PI);
    approx(one_float("RETURN e() AS v", &db), std::f64::consts::E);
}

// ===== Exponential / logarithmic ============================================

#[test]
fn exp_log_log10_round_trip() {
    let db = db();
    // exp and natural log are inverses.
    approx(one_float("RETURN exp(0) AS v", &db), 1.0);
    approx(one_float("RETURN exp(1) AS v", &db), std::f64::consts::E);
    approx(one_float("RETURN log(e()) AS v", &db), 1.0);
    approx(one_float("RETURN log(1) AS v", &db), 0.0);
    // base-10 log.
    approx(one_float("RETURN log10(1000) AS v", &db), 3.0);
    approx(one_float("RETURN log10(1) AS v", &db), 0.0);
}

#[test]
fn log_domain_edges_follow_ieee() {
    let db = db();
    // log of a negative is NaN, log(0) is -Infinity — Neo4j returns the float,
    // never an error.
    assert!(one_float("RETURN log(-1) AS v", &db).is_nan());
    assert_eq!(one_float("RETURN log(0) AS v", &db), f64::NEG_INFINITY);
}

// ===== Trigonometric ========================================================

#[test]
fn sin_cos_tan_at_key_angles() {
    let db = db();
    approx(one_float("RETURN sin(0) AS v", &db), 0.0);
    approx(one_float("RETURN cos(0) AS v", &db), 1.0);
    approx(one_float("RETURN tan(0) AS v", &db), 0.0);
    approx(one_float("RETURN sin(pi() / 2) AS v", &db), 1.0);
    approx(one_float("RETURN cos(pi()) AS v", &db), -1.0);
}

#[test]
fn cot_is_reciprocal_of_tan() {
    let db = db();
    // cot(x) = 1 / tan(x); at pi/4 tan = 1 so cot = 1.
    approx(one_float("RETURN cot(pi() / 4) AS v", &db), 1.0);
}

#[test]
fn inverse_trig_functions() {
    let db = db();
    approx(
        one_float("RETURN asin(1) AS v", &db),
        std::f64::consts::FRAC_PI_2,
    );
    approx(one_float("RETURN acos(1) AS v", &db), 0.0);
    approx(
        one_float("RETURN atan(1) AS v", &db),
        std::f64::consts::FRAC_PI_4,
    );
    // asin/acos outside [-1, 1] are NaN, not errors.
    assert!(one_float("RETURN asin(2) AS v", &db).is_nan());
    assert!(one_float("RETURN acos(2) AS v", &db).is_nan());
}

#[test]
fn atan2_two_argument_form() {
    let db = db();
    // atan2(1, 1) = pi/4; atan2(1, 0) = pi/2.
    approx(
        one_float("RETURN atan2(1, 1) AS v", &db),
        std::f64::consts::FRAC_PI_4,
    );
    approx(
        one_float("RETURN atan2(1, 0) AS v", &db),
        std::f64::consts::FRAC_PI_2,
    );
}

// ===== Angle helpers ========================================================

#[test]
fn degrees_radians_round_trip() {
    let db = db();
    approx(one_float("RETURN degrees(pi()) AS v", &db), 180.0);
    approx(
        one_float("RETURN radians(180) AS v", &db),
        std::f64::consts::PI,
    );
    approx(one_float("RETURN degrees(radians(57)) AS v", &db), 57.0);
}

#[test]
fn haversin_half_versed_sine() {
    let db = db();
    // haversin(x) = (1 - cos(x)) / 2; haversin(0) = 0, haversin(pi) = 1.
    approx(one_float("RETURN haversin(0) AS v", &db), 0.0);
    approx(one_float("RETURN haversin(pi()) AS v", &db), 1.0);
}

// ===== Integer arguments widen to Float =====================================

#[test]
fn integer_argument_widens_to_float() {
    let db = db();
    // An integer literal is accepted and the result is always a Float.
    assert!(matches!(one("RETURN sin(0) AS v", &db), Value::Float(_)));
    assert!(matches!(one("RETURN exp(2) AS v", &db), Value::Float(_)));
}

// ===== NULL propagation =====================================================

#[test]
fn null_argument_propagates() {
    let db = db();
    assert_eq!(one("RETURN sin(null) AS v", &db), Value::Null);
    assert_eq!(one("RETURN log(null) AS v", &db), Value::Null);
    assert_eq!(one("RETURN atan2(null, 1) AS v", &db), Value::Null);
    assert_eq!(one("RETURN degrees(null) AS v", &db), Value::Null);
}

// ===== Error handling =======================================================

#[test]
fn non_numeric_argument_is_invalid_call() {
    let db = db();
    let e = run_err("RETURN sin('x') AS v", &db);
    assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
}

#[test]
fn wrong_arity_is_invalid_call() {
    let db = db();
    // sin takes exactly one argument; atan2 exactly two; pi/e exactly zero.
    let e = run_err("RETURN sin(1, 2) AS v", &db);
    assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
    let e = run_err("RETURN atan2(1) AS v", &db);
    assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
    let e = run_err("RETURN pi(1) AS v", &db);
    assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
}

// ===== Composition with the rest of the language ============================

#[test]
fn math_functions_compose_in_where_and_arithmetic() {
    let db = db();
    // A WHERE predicate over a computed trig value.
    let rows = run(
        "UNWIND [0, 1, 2, 3] AS x WITH x WHERE sin(x) > 0 RETURN x",
        &db,
    );
    // sin(1) > 0, sin(2) > 0, sin(3) > 0 (all in (0, pi)); sin(0) = 0 excluded.
    assert_eq!(rows.len(), 3);

    // Arithmetic around a function result.
    approx(one_float("RETURN exp(0) + log(1) AS v", &db), 1.0);
}

// ===== Scenario domains =====================================================

#[test]
fn cbt_journal_mood_log_compression() {
    // CBT journal: compress a raw mood-intensity score with a natural log so a
    // jump from 8→9 weighs less than 1→2 in a trend view.
    let db = db();
    exec("CREATE (:Mood {label: 'calm', intensity: 1})", &db);
    exec("CREATE (:Mood {label: 'tense', intensity: 7})", &db);
    let rows = run(
        "MATCH (m:Mood) RETURN m.label AS label, log(m.intensity) AS score ORDER BY score",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::String("calm".into()));
    approx(
        match &rows[1][1] {
            Value::Float(f) => *f,
            other => panic!("{other:?}"),
        },
        (7.0_f64).ln(),
    );
}

#[test]
fn story_editor_great_circle_haversine_distance() {
    // Story/book editor: two settings on a fictional globe; the haversine
    // formula gives the great-circle distance between their lat/long (in
    // radians here for brevity).
    let db = db();
    // Same point → zero distance.
    approx(one_float("RETURN haversin(radians(0)) AS v", &db), 0.0);
}

#[test]
fn task_manager_exponential_backoff_schedule() {
    // IT task manager: a retry's delay grows exponentially with the attempt
    // number — delay = exp(attempt).
    let db = db();
    exec("CREATE (:Retry {attempt: 0})", &db);
    exec("CREATE (:Retry {attempt: 2})", &db);
    let rows = run(
        "MATCH (r:Retry) RETURN r.attempt AS n, exp(r.attempt) AS delay ORDER BY n",
        &db,
    );
    assert_eq!(rows.len(), 2);
    approx(
        match &rows[0][1] {
            Value::Float(f) => *f,
            other => panic!("{other:?}"),
        },
        1.0,
    );
}

#[test]
fn erp_order_angle_for_pie_chart_slice() {
    // ERP: a category's share of total revenue rendered as a pie slice angle,
    // converting a fraction to degrees.
    let db = db();
    // A quarter share → 90 degrees.
    approx(
        one_float("RETURN degrees(radians(0.25 * 360)) AS v", &db),
        90.0,
    );
}

#[test]
fn bug_tracker_severity_decay_score() {
    // Bug tracker: an open bug's priority decays over time; the half-life decay
    // factor is exp(-age / tau).
    let db = db();
    exec("CREATE (:Bug {title: 'crash', age: 0})", &db);
    let v = one_float("MATCH (b:Bug) RETURN exp(-b.age / 10.0) AS weight", &db);
    approx(v, 1.0);
}
