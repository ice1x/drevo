//! End-to-end Cypher `round()` precision & rounding-mode tests — Phase 10
//! follow-up task `00160`.
//!
//! Neo4j 5 exposes three overloads of `round()`:
//!
//! * `round(value)` — round to the nearest integer, ties away from zero
//!   (`HALF_UP`). This is the long-standing one-argument form.
//! * `round(value, precision)` — round to `precision` decimal places, still
//!   `HALF_UP`. A negative `precision` rounds to the left of the decimal point
//!   (`round(1234.5, -2) = 1200.0`).
//! * `round(value, precision, mode)` — round to `precision` decimal places
//!   using `mode`, one of `UP`, `DOWN`, `CEILING`, `FLOOR`, `HALF_UP`,
//!   `HALF_DOWN`, `HALF_EVEN` (case-insensitive), mirroring Java's
//!   `RoundingMode`.
//!
//! Semantics mirror Neo4j:
//!
//! * The result is always a `Float`.
//! * A `NULL` in any argument yields `NULL` (NULL-propagating like every other
//!   scalar built-in).
//! * A non-numeric `value`, non-Integer `precision`, non-String `mode`, an
//!   unknown `mode` string, or wrong arity is a recoverable
//!   `ExecError::InvalidFunctionCall`, never a panic or a wrong answer.
//! * A non-finite `value` (`NaN` / ±`Infinity`) is returned unchanged.
//!
//! These cases drive the real parser → executor pipeline across the five drevo
//! target scenario domains (CBT journal, story/book editor, IT task manager,
//! ERP, bug tracker) plus the cross-cutting numeric semantics.

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

/// Extract the `f64` out of a `Value::Float`, failing loudly otherwise.
fn float_of(v: Value) -> f64 {
    match v {
        Value::Float(f) => f,
        other => panic!("expected Float, got {other:?}"),
    }
}

/// Assert a query returns a `Float` within a tiny tolerance of `want`. Decimal
/// rounding results (e.g. `3.14`) are not exactly representable in binary, so
/// an exact `==` would be brittle.
fn assert_close(source: &str, drevo: &Drevo, want: f64) {
    let got = float_of(one(source, drevo));
    assert!(
        (got - want).abs() < 1e-9,
        "{source:?}: expected ~{want}, got {got}"
    );
}

// ===== One-argument form (backwards compatible) =============================

#[test]
fn round_single_arg_rounds_to_nearest_integer() {
    let db = db();
    assert_close("RETURN round(3.4) AS v", &db, 3.0);
    assert_close("RETURN round(3.6) AS v", &db, 4.0);
    // Result is always a Float, never an Integer.
    assert!(matches!(
        one("RETURN round(3.4) AS v", &db),
        Value::Float(_)
    ));
}

#[test]
fn round_single_arg_ties_away_from_zero() {
    let db = db();
    // HALF_UP: ties round away from zero.
    assert_close("RETURN round(2.5) AS v", &db, 3.0);
    assert_close("RETURN round(-2.5) AS v", &db, -3.0);
    assert_close("RETURN round(0.5) AS v", &db, 1.0);
    assert_close("RETURN round(-0.5) AS v", &db, -1.0);
}

#[test]
fn round_single_arg_widens_integer_input() {
    let db = db();
    // An Integer argument widens to Float; the result keeps Float type.
    assert_close("RETURN round(7) AS v", &db, 7.0);
}

// ===== Two-argument form: precision ========================================

#[test]
fn round_precision_decimal_places() {
    let db = db();
    assert_close("RETURN round(2.34567, 2) AS v", &db, 2.35);
    assert_close("RETURN round(2.34567, 3) AS v", &db, 2.346);
    assert_close("RETURN round(2.34567, 0) AS v", &db, 2.0);
}

#[test]
fn round_precision_ties_half_up() {
    let db = db();
    // Default mode at a given precision is HALF_UP (ties away from zero).
    assert_close("RETURN round(1.255, 2) AS v", &db, 1.26);
    assert_close("RETURN round(-1.255, 2) AS v", &db, -1.26);
}

#[test]
fn round_negative_precision_rounds_left_of_point() {
    let db = db();
    assert_close("RETURN round(1234.5, -2) AS v", &db, 1200.0);
    assert_close("RETURN round(1250.0, -2) AS v", &db, 1300.0);
    assert_close("RETURN round(1234.5, -1) AS v", &db, 1230.0);
}

// ===== Three-argument form: rounding mode ==================================

#[test]
fn round_mode_up_rounds_away_from_zero() {
    let db = db();
    assert_close("RETURN round(1.21, 1, 'UP') AS v", &db, 1.3);
    assert_close("RETURN round(-1.21, 1, 'UP') AS v", &db, -1.3);
    // An already-exact value is unchanged.
    assert_close("RETURN round(1.2, 1, 'UP') AS v", &db, 1.2);
}

#[test]
fn round_mode_down_truncates_toward_zero() {
    let db = db();
    assert_close("RETURN round(1.29, 1, 'DOWN') AS v", &db, 1.2);
    assert_close("RETURN round(-1.29, 1, 'DOWN') AS v", &db, -1.2);
}

#[test]
fn round_mode_ceiling_rounds_toward_positive_infinity() {
    let db = db();
    assert_close("RETURN round(1.21, 1, 'CEILING') AS v", &db, 1.3);
    assert_close("RETURN round(-1.29, 1, 'CEILING') AS v", &db, -1.2);
}

#[test]
fn round_mode_floor_rounds_toward_negative_infinity() {
    let db = db();
    assert_close("RETURN round(1.29, 1, 'FLOOR') AS v", &db, 1.2);
    assert_close("RETURN round(-1.21, 1, 'FLOOR') AS v", &db, -1.3);
}

#[test]
fn round_mode_half_up_ties_away_from_zero() {
    let db = db();
    assert_close("RETURN round(1.25, 1, 'HALF_UP') AS v", &db, 1.3);
    assert_close("RETURN round(-1.25, 1, 'HALF_UP') AS v", &db, -1.3);
}

#[test]
fn round_mode_half_down_ties_toward_zero() {
    let db = db();
    assert_close("RETURN round(1.25, 1, 'HALF_DOWN') AS v", &db, 1.2);
    assert_close("RETURN round(-1.25, 1, 'HALF_DOWN') AS v", &db, -1.2);
    // A non-tie still rounds to the nearest.
    assert_close("RETURN round(1.26, 1, 'HALF_DOWN') AS v", &db, 1.3);
}

#[test]
fn round_mode_half_even_ties_to_even() {
    let db = db();
    // Banker's rounding: ties go to the nearest even digit.
    assert_close("RETURN round(2.5, 0, 'HALF_EVEN') AS v", &db, 2.0);
    assert_close("RETURN round(3.5, 0, 'HALF_EVEN') AS v", &db, 4.0);
    assert_close("RETURN round(-2.5, 0, 'HALF_EVEN') AS v", &db, -2.0);
    assert_close("RETURN round(-3.5, 0, 'HALF_EVEN') AS v", &db, -4.0);
}

#[test]
fn round_mode_is_case_insensitive() {
    let db = db();
    assert_close("RETURN round(1.21, 1, 'up') AS v", &db, 1.3);
    assert_close("RETURN round(1.29, 1, 'Down') AS v", &db, 1.2);
    assert_close("RETURN round(2.5, 0, 'half_even') AS v", &db, 2.0);
}

// ===== NULL propagation ====================================================

#[test]
fn round_null_value_propagates() {
    let db = db();
    assert_eq!(one("RETURN round(null) AS v", &db), Value::Null);
    assert_eq!(one("RETURN round(null, 2) AS v", &db), Value::Null);
    assert_eq!(one("RETURN round(null, 2, 'UP') AS v", &db), Value::Null);
}

#[test]
fn round_null_precision_or_mode_propagates() {
    let db = db();
    assert_eq!(one("RETURN round(3.14159, null) AS v", &db), Value::Null);
    assert_eq!(one("RETURN round(3.14159, 2, null) AS v", &db), Value::Null);
}

// ===== Non-finite passthrough ==============================================

#[test]
fn round_non_finite_value_passes_through() {
    let db = db();
    // sqrt(-1) is NaN; rounding it leaves NaN.
    assert!(float_of(one("RETURN round(sqrt(-1.0), 2) AS v", &db)).is_nan());
    // log(0) is -Infinity.
    let neg_inf = float_of(one("RETURN round(log(0.0), 3) AS v", &db));
    assert!(neg_inf.is_infinite() && neg_inf < 0.0);
}

// ===== Error cases =========================================================

#[test]
fn round_no_arguments_is_invalid() {
    let db = db();
    assert!(matches!(
        run_err("RETURN round() AS v", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
}

#[test]
fn round_too_many_arguments_is_invalid() {
    let db = db();
    assert!(matches!(
        run_err("RETURN round(1.0, 2, 'UP', 4) AS v", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
}

#[test]
fn round_non_numeric_value_is_invalid() {
    let db = db();
    assert!(matches!(
        run_err("RETURN round('nope') AS v", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
}

#[test]
fn round_non_integer_precision_is_invalid() {
    let db = db();
    assert!(matches!(
        run_err("RETURN round(3.14, 1.5) AS v", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
}

#[test]
fn round_non_string_mode_is_invalid() {
    let db = db();
    assert!(matches!(
        run_err("RETURN round(3.14, 2, 7) AS v", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
}

#[test]
fn round_unknown_mode_is_invalid() {
    let db = db();
    assert!(matches!(
        run_err("RETURN round(3.14, 2, 'SIDEWAYS') AS v", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
}

// ===== Scenario-domain coverage ============================================

#[test]
fn erp_round_line_item_total_to_cents() {
    // ERP: a unit price times a quantity rounded to two decimal places.
    let db = db();
    db_create(
        &db,
        "CREATE (:LineItem {sku: 'WIDGET-1', unit_price: 19.99, qty: 3})",
    );
    assert_close(
        "MATCH (l:LineItem) RETURN round(l.unit_price * l.qty, 2) AS total",
        &db,
        59.97,
    );
}

#[test]
fn erp_round_tax_half_even_for_fair_banker_rounding() {
    // ERP: bankers' rounding on a tax amount that lands exactly on a half-cent.
    let db = db();
    db_create(&db, "CREATE (:Invoice {tax: 2.125})");
    assert_close(
        "MATCH (i:Invoice) RETURN round(i.tax, 2, 'HALF_EVEN') AS tax",
        &db,
        2.12,
    );
}

#[test]
fn task_manager_round_progress_percentage() {
    // IT task manager: a fractional completion ratio rendered as a 1-dp percent.
    let db = db();
    db_create(&db, "CREATE (:Sprint {done: 7.0, total: 9.0})");
    assert_close(
        "MATCH (s:Sprint) RETURN round(100.0 * s.done / s.total, 1) AS pct",
        &db,
        77.8,
    );
}

#[test]
fn story_editor_round_average_chapter_length() {
    // Story editor: an average word count rounded to the nearest hundred words.
    let db = db();
    db_create(&db, "CREATE (:Book {avg_words: 3274.0})");
    assert_close(
        "MATCH (b:Book) RETURN round(b.avg_words, -2) AS rounded",
        &db,
        3300.0,
    );
}

/// Run a mutating statement, discarding the (empty) result.
fn db_create(drevo: &Drevo, source: &str) {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute create");
}
