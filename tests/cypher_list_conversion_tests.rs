//! End-to-end Cypher list value-conversion function tests — Phase 10 follow-up
//! task `00157`.
//!
//! Task `00138` shipped the scalar conversion family (`toInteger` / `toFloat` /
//! `toBoolean` / `toString`). This task completes it with Neo4j's *list*
//! conversion functions, which apply the corresponding scalar conversion to
//! every element of a list:
//!
//! * `toIntegerList(list)`
//! * `toFloatList(list)`
//! * `toBooleanList(list)`
//! * `toStringList(list)`
//!
//! Semantics mirror Neo4j:
//!
//! * A `NULL` argument yields `NULL` (the functions are NULL-propagating on
//!   their single argument).
//! * A non-`List` argument is a recoverable `ExecError::InvalidFunctionCall`.
//! * Element conversion is *lenient*: an element that cannot be converted (an
//!   unparseable string, an out-of-shape type, or a `NULL` element) becomes
//!   `NULL` in the returned list rather than aborting the whole call. The list
//!   therefore always has the same length as its input.
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

/// Pull a `List` out of a one-row projection, failing on any other shape.
fn one_list(source: &str, drevo: &Drevo) -> Vec<Value> {
    match one(source, drevo) {
        Value::List(items) => items,
        other => panic!("expected List from {source:?}, got {other:?}"),
    }
}

// ===== toIntegerList ========================================================

#[test]
fn to_integer_list_converts_numbers_and_strings() {
    let db = db();
    let got = one_list(r#"RETURN toIntegerList([1, 2.9, "3", "-4"]) AS v"#, &db);
    assert_eq!(
        got,
        vec![
            Value::Integer(1),
            Value::Integer(2), // float truncates toward zero
            Value::Integer(3),
            Value::Integer(-4),
        ]
    );
}

#[test]
fn to_integer_list_unconvertible_elements_become_null() {
    let db = db();
    // Unparseable string, boolean, and a nested list cannot convert → NULL,
    // but the element count is preserved.
    let got = one_list(r#"RETURN toIntegerList(["x", true, [1], 7]) AS v"#, &db);
    assert_eq!(
        got,
        vec![Value::Null, Value::Null, Value::Null, Value::Integer(7)]
    );
}

#[test]
fn to_integer_list_preserves_null_elements() {
    let db = db();
    let got = one_list(r#"RETURN toIntegerList([1, null, 3]) AS v"#, &db);
    assert_eq!(got, vec![Value::Integer(1), Value::Null, Value::Integer(3)]);
}

// ===== toFloatList ==========================================================

#[test]
fn to_float_list_converts_numbers_and_strings() {
    let db = db();
    let got = one_list(r#"RETURN toFloatList([1, 2.5, "3.25"]) AS v"#, &db);
    assert_eq!(
        got,
        vec![Value::Float(1.0), Value::Float(2.5), Value::Float(3.25)]
    );
}

#[test]
fn to_float_list_unconvertible_elements_become_null() {
    let db = db();
    let got = one_list(r#"RETURN toFloatList(["nope", false]) AS v"#, &db);
    assert_eq!(got, vec![Value::Null, Value::Null]);
}

// ===== toBooleanList ========================================================

#[test]
fn to_boolean_list_converts_strings_and_integers() {
    let db = db();
    let got = one_list(
        r#"RETURN toBooleanList([true, "true", "FALSE", 1, 0]) AS v"#,
        &db,
    );
    assert_eq!(
        got,
        vec![
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(false), // case-insensitive
            Value::Bool(true),
            Value::Bool(false),
        ]
    );
}

#[test]
fn to_boolean_list_unconvertible_elements_become_null() {
    let db = db();
    let got = one_list(r#"RETURN toBooleanList(["yes", 2, 1.5]) AS v"#, &db);
    assert_eq!(got, vec![Value::Null, Value::Null, Value::Null]);
}

// ===== toStringList =========================================================

#[test]
fn to_string_list_converts_scalars() {
    let db = db();
    let got = one_list(r#"RETURN toStringList([1, 2.5, true, "x"]) AS v"#, &db);
    assert_eq!(
        got,
        vec![
            Value::String("1".into()),
            Value::String("2.5".into()),
            Value::String("true".into()),
            Value::String("x".into()),
        ]
    );
}

#[test]
fn to_string_list_unconvertible_elements_become_null() {
    let db = db();
    // A nested list / map cannot stringify → NULL (unlike scalar `toString`,
    // which errors; the list variant is lenient).
    let got = one_list(r#"RETURN toStringList([1, [2, 3]]) AS v"#, &db);
    assert_eq!(got, vec![Value::String("1".into()), Value::Null]);
}

// ===== NULL propagation & empty list =======================================

#[test]
fn null_argument_yields_null() {
    let db = db();
    assert_eq!(one("RETURN toIntegerList(null) AS v", &db), Value::Null);
    assert_eq!(one("RETURN toFloatList(null) AS v", &db), Value::Null);
    assert_eq!(one("RETURN toBooleanList(null) AS v", &db), Value::Null);
    assert_eq!(one("RETURN toStringList(null) AS v", &db), Value::Null);
}

#[test]
fn empty_list_round_trips_empty() {
    let db = db();
    for f in [
        "toIntegerList",
        "toFloatList",
        "toBooleanList",
        "toStringList",
    ] {
        let src = format!("RETURN {f}([]) AS v");
        assert_eq!(one(&src, &db), Value::List(vec![]));
    }
}

// ===== Error: non-list argument =============================================

#[test]
fn non_list_argument_is_recoverable_error() {
    let db = db();
    for f in [
        "toIntegerList",
        "toFloatList",
        "toBooleanList",
        "toStringList",
    ] {
        let src = format!("RETURN {f}(5) AS v");
        match run_err(&src, &db) {
            ExecError::InvalidFunctionCall { .. } => {}
            other => panic!("expected InvalidFunctionCall from {src:?}, got {other:?}"),
        }
    }
}

// ===== Scenario-domain workflows ===========================================

#[test]
fn cbt_journal_mood_scores_to_integers() {
    let db = db();
    // A journal entry stores mood ratings entered as free text; normalize to
    // integers for trend analysis, with a typo defaulting to NULL.
    let got = one_list(
        r#"RETURN toIntegerList(["7", "4", "huh", "9"]) AS scores"#,
        &db,
    );
    assert_eq!(
        got,
        vec![
            Value::Integer(7),
            Value::Integer(4),
            Value::Null,
            Value::Integer(9),
        ]
    );
}

#[test]
fn erp_line_item_prices_to_floats() {
    let db = db();
    let got = one_list(
        r#"RETURN toFloatList(["19.99", "5", "0.49"]) AS prices"#,
        &db,
    );
    assert_eq!(
        got,
        vec![Value::Float(19.99), Value::Float(5.0), Value::Float(0.49)]
    );
}

#[test]
fn bug_tracker_flags_to_booleans() {
    let db = db();
    let got = one_list(
        r#"RETURN toBooleanList(["true", "false", "true"]) AS resolved"#,
        &db,
    );
    assert_eq!(
        got,
        vec![Value::Bool(true), Value::Bool(false), Value::Bool(true)]
    );
}

#[test]
fn task_manager_ids_to_strings() {
    let db = db();
    let got = one_list(r#"RETURN toStringList([101, 102, 103]) AS ids"#, &db);
    assert_eq!(
        got,
        vec![
            Value::String("101".into()),
            Value::String("102".into()),
            Value::String("103".into()),
        ]
    );
}

#[test]
fn story_editor_chapter_numbers_round_trip_through_unwind() {
    let db = db();
    // Convert a list of stringified chapter numbers to integers, then expand
    // them into rows — exercises composition with UNWIND (task `00135`).
    let rows = run(
        r#"UNWIND toIntegerList(["1", "2", "3"]) AS ch RETURN ch ORDER BY ch"#,
        &db,
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ]
    );
}
