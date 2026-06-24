//! End-to-end Cypher fully-lenient (`*OrNull`) conversion function tests —
//! Phase 10 follow-up task `00158`.
//!
//! Task `00138` shipped the scalar conversion family (`toInteger` / `toFloat` /
//! `toBoolean` / `toString`) and task `00157` the element-wise list variants.
//! This task adds the Neo4j 5 *fully-lenient* siblings, which return `NULL` for
//! any value they cannot convert instead of erroring:
//!
//! * `toIntegerOrNull(x)`
//! * `toFloatOrNull(x)`
//! * `toBooleanOrNull(x)`
//! * `toStringOrNull(x)`
//!
//! Semantics mirror Neo4j:
//!
//! * A `NULL` argument yields `NULL` (NULL-propagating on the single argument).
//! * Any value that cannot be converted — an unparseable string, an
//!   out-of-shape type (a List, a Map), a node — yields `NULL` rather than an
//!   error. `toStringOrNull` is the one with behaviour distinct from its strict
//!   sibling: scalar `toString` *errors* on a non-stringifiable value, whereas
//!   `toStringOrNull` yields `NULL`.
//! * Wrong arity is still a recoverable `ExecError::InvalidFunctionCall`.
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

// ===== toIntegerOrNull ======================================================

#[test]
fn to_integer_or_null_happy_path() {
    let db = db();
    assert_eq!(
        one("RETURN toIntegerOrNull('42') AS v", &db),
        Value::Integer(42)
    );
    // A float truncates toward zero.
    assert_eq!(
        one("RETURN toIntegerOrNull(3.9) AS v", &db),
        Value::Integer(3)
    );
    assert_eq!(
        one("RETURN toIntegerOrNull(-4) AS v", &db),
        Value::Integer(-4)
    );
}

#[test]
fn to_integer_or_null_unconvertible_is_null_not_error() {
    let db = db();
    // Unparseable string, boolean, list, and map all yield NULL.
    assert_eq!(one("RETURN toIntegerOrNull('abc') AS v", &db), Value::Null);
    assert_eq!(one("RETURN toIntegerOrNull(true) AS v", &db), Value::Null);
    assert_eq!(one("RETURN toIntegerOrNull([1, 2]) AS v", &db), Value::Null);
    assert_eq!(one("RETURN toIntegerOrNull({a: 1}) AS v", &db), Value::Null);
}

// ===== toFloatOrNull ========================================================

#[test]
fn to_float_or_null_happy_path() {
    let db = db();
    assert_eq!(
        one("RETURN toFloatOrNull('1.5') AS v", &db),
        Value::Float(1.5)
    );
    assert_eq!(one("RETURN toFloatOrNull(2) AS v", &db), Value::Float(2.0));
}

#[test]
fn to_float_or_null_unconvertible_is_null() {
    let db = db();
    assert_eq!(one("RETURN toFloatOrNull('x') AS v", &db), Value::Null);
    assert_eq!(one("RETURN toFloatOrNull(true) AS v", &db), Value::Null);
    assert_eq!(one("RETURN toFloatOrNull([3.0]) AS v", &db), Value::Null);
}

// ===== toBooleanOrNull ======================================================

#[test]
fn to_boolean_or_null_happy_path() {
    let db = db();
    assert_eq!(
        one("RETURN toBooleanOrNull('TRUE') AS v", &db),
        Value::Bool(true)
    );
    assert_eq!(
        one("RETURN toBooleanOrNull('false') AS v", &db),
        Value::Bool(false)
    );
    assert_eq!(
        one("RETURN toBooleanOrNull(1) AS v", &db),
        Value::Bool(true)
    );
    assert_eq!(
        one("RETURN toBooleanOrNull(0) AS v", &db),
        Value::Bool(false)
    );
}

#[test]
fn to_boolean_or_null_unconvertible_is_null() {
    let db = db();
    assert_eq!(
        one("RETURN toBooleanOrNull('maybe') AS v", &db),
        Value::Null
    );
    // An integer other than 0/1 is not a boolean.
    assert_eq!(one("RETURN toBooleanOrNull(7) AS v", &db), Value::Null);
    assert_eq!(one("RETURN toBooleanOrNull([true]) AS v", &db), Value::Null);
}

// ===== toStringOrNull (the genuinely distinct one) ==========================

#[test]
fn to_string_or_null_stringifies_scalars() {
    let db = db();
    assert_eq!(
        one("RETURN toStringOrNull(42) AS v", &db),
        Value::String("42".into())
    );
    assert_eq!(
        one("RETURN toStringOrNull(2.5) AS v", &db),
        Value::String("2.5".into())
    );
    assert_eq!(
        one("RETURN toStringOrNull(true) AS v", &db),
        Value::String("true".into())
    );
    assert_eq!(
        one("RETURN toStringOrNull('hi') AS v", &db),
        Value::String("hi".into())
    );
}

#[test]
fn to_string_or_null_is_lenient_where_strict_to_string_errors() {
    let db = db();
    // The strict scalar `toString` raises InvalidFunctionCall on a List/Map;
    // `toStringOrNull` yields NULL instead — the key behavioural difference.
    assert_eq!(one("RETURN toStringOrNull([1, 2]) AS v", &db), Value::Null);
    assert_eq!(one("RETURN toStringOrNull({a: 1}) AS v", &db), Value::Null);
    // Contrast: strict toString errors on the same input.
    match run_err("RETURN toString([1, 2]) AS v", &db) {
        ExecError::InvalidFunctionCall { .. } => {}
        other => panic!("expected strict toString to error, got {other:?}"),
    }
}

// ===== NULL propagation & arity =============================================

#[test]
fn null_argument_propagates_to_null() {
    let db = db();
    for f in [
        "toIntegerOrNull",
        "toFloatOrNull",
        "toBooleanOrNull",
        "toStringOrNull",
    ] {
        let src = format!("RETURN {f}(null) AS v");
        assert_eq!(one(&src, &db), Value::Null, "{src}");
    }
}

#[test]
fn wrong_arity_is_recoverable_error() {
    let db = db();
    for f in [
        "toIntegerOrNull",
        "toFloatOrNull",
        "toBooleanOrNull",
        "toStringOrNull",
    ] {
        let src = format!("RETURN {f}(1, 2) AS v");
        match run_err(&src, &db) {
            ExecError::InvalidFunctionCall { .. } => {}
            other => panic!("expected InvalidFunctionCall from {src:?}, got {other:?}"),
        }
    }
}

// ===== Composition ==========================================================

#[test]
fn composes_in_where_filtering_out_unconvertible_rows() {
    let db = db();
    // Stringified scores; the unparseable "n/a" becomes NULL and is dropped by
    // the `> 5` comparison (three-valued logic), without aborting the scan.
    let rows = run(
        r#"UNWIND ['8', 'n/a', '3', '9'] AS raw
           WITH toIntegerOrNull(raw) AS n
           WHERE n > 5
           RETURN n ORDER BY n"#,
        &db,
    );
    assert_eq!(rows, vec![vec![Value::Integer(8)], vec![Value::Integer(9)]]);
}

#[test]
fn feeds_coalesce_for_a_safe_default() {
    let db = db();
    // The canonical `*OrNull` idiom: convert leniently, then supply a default.
    assert_eq!(
        one("RETURN coalesce(toIntegerOrNull('oops'), -1) AS v", &db),
        Value::Integer(-1)
    );
    assert_eq!(
        one("RETURN coalesce(toIntegerOrNull('7'), -1) AS v", &db),
        Value::Integer(7)
    );
}

// ===== Scenario-domain workflows ===========================================

#[test]
fn cbt_journal_free_text_mood_to_integer() {
    let db = db();
    // A mood rating typed as free text is normalized; a non-numeric entry
    // safely defaults rather than failing the whole journal query.
    let rows = run(
        r#"UNWIND [{day: 'mon', mood: '6'}, {day: 'tue', mood: 'meh'}] AS e
           RETURN e.day AS day, coalesce(toIntegerOrNull(e.mood), 0) AS mood
           ORDER BY day"#,
        &db,
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::String("mon".into()), Value::Integer(6)],
            vec![Value::String("tue".into()), Value::Integer(0)],
        ]
    );
}

#[test]
fn erp_price_string_to_float() {
    let db = db();
    db_exec(
        &db,
        "CREATE (:Product {sku: 'A1', price_text: '19.99'}), (:Product {sku: 'B2', price_text: 'TBD'})",
    );
    let rows = run(
        r#"MATCH (p:Product)
           RETURN p.sku AS sku, toFloatOrNull(p.price_text) AS price
           ORDER BY sku"#,
        &db,
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::String("A1".into()), Value::Float(19.99)],
            vec![Value::String("B2".into()), Value::Null],
        ]
    );
}

#[test]
fn bug_tracker_resolved_flag_string_to_boolean() {
    let db = db();
    let rows = run(
        r#"UNWIND ['true', 'False', 'yes'] AS flag
           RETURN toBooleanOrNull(flag) AS resolved"#,
        &db,
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::Bool(true)],
            vec![Value::Bool(false)],
            vec![Value::Null], // 'yes' is not a Cypher boolean literal
        ]
    );
}

#[test]
fn task_manager_id_to_string_for_display() {
    let db = db();
    assert_eq!(
        one("RETURN toStringOrNull(4096) AS id", &db),
        Value::String("4096".into())
    );
}

#[test]
fn story_editor_chapter_numbers_round_trip_through_unwind() {
    let db = db();
    // Convert stringified chapter numbers leniently, drop the unparseable one,
    // then expand into rows — exercises composition with UNWIND (task `00135`).
    let rows = run(
        r#"UNWIND ['1', 'prologue', '2'] AS raw
           WITH toIntegerOrNull(raw) AS ch
           WHERE ch IS NOT NULL
           RETURN ch ORDER BY ch"#,
        &db,
    );
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
}

/// Run a statement for its side effects (CREATE), discarding the result.
fn db_exec(drevo: &Drevo, source: &str) {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute");
}
