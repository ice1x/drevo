//! End-to-end Cypher list / map indexing & slicing tests — Phase 10
//! follow-up task `00139`.
//!
//! The executor evaluates the two element-access expression forms the parser
//! already produces (`00062`):
//!
//! * `expr[index]` — a single list element (`xs[0]`, `xs[-1]`) or a
//!   map / node / relationship field (`m['key']`, equivalent to property
//!   access). A list index is zero-based, may be negative (counting from the
//!   end), and yields `NULL` when out of range. A map key must be a string;
//!   an absent key yields `NULL`.
//! * `expr[from..to]` — a `from`-inclusive / `to`-exclusive list slice with
//!   negative bounds counting from the end, every bound clamped into range,
//!   and either bound optional (`xs[..n]` / `xs[n..]` / `xs[..]`).
//!
//! `NULL` propagates through a `NULL` base, a `NULL` index, or a `NULL` slice
//! bound. Genuine misuse — a non-integer list index, a non-string map key, or
//! indexing / slicing a scalar — is a recoverable `ExecError::TypeMismatch`.
//!
//! These cases drive the real parser → executor pipeline across the five
//! drevo target scenario domains (CBT journal, story / book editor, IT task
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

fn run_with(source: &str, drevo: &Drevo, params: HashMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, params).expect("execute").rows
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

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn ints(values: &[i64]) -> Value {
    Value::List(values.iter().map(|i| Value::Integer(*i)).collect())
}

// ===== List indexing — core semantics =======================================

#[test]
fn list_literal_index_zero_based() {
    let db = db();
    assert_eq!(one("RETURN [10, 20, 30][0] AS v", &db), Value::Integer(10));
    assert_eq!(one("RETURN [10, 20, 30][2] AS v", &db), Value::Integer(30));
}

#[test]
fn list_literal_index_negative_counts_from_end() {
    let db = db();
    assert_eq!(one("RETURN [10, 20, 30][-1] AS v", &db), Value::Integer(30));
    assert_eq!(one("RETURN [10, 20, 30][-3] AS v", &db), Value::Integer(10));
}

#[test]
fn list_index_out_of_range_is_null() {
    let db = db();
    assert_eq!(one("RETURN [1, 2, 3][3] AS v", &db), Value::Null);
    assert_eq!(one("RETURN [1, 2, 3][-4] AS v", &db), Value::Null);
    assert_eq!(one("RETURN [1, 2, 3][99] AS v", &db), Value::Null);
}

#[test]
fn list_index_with_parameter() {
    let db = db();
    let mut params = HashMap::new();
    params.insert("i".to_string(), Value::Integer(1));
    let rows = run_with("RETURN ['a', 'b', 'c'][$i] AS v", &db, params);
    assert_eq!(rows[0][0], s("b"));
}

#[test]
fn list_index_non_integer_is_type_error() {
    let db = db();
    let e = run_err("RETURN [1, 2, 3]['x'] AS v", &db);
    assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
}

#[test]
fn indexing_a_scalar_is_type_error() {
    let db = db();
    let e = run_err("RETURN 42[0] AS v", &db);
    assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
}

#[test]
fn null_base_index_propagates_null() {
    let db = db();
    assert_eq!(one("RETURN null[0] AS v", &db), Value::Null);
}

// ===== Map indexing =========================================================

#[test]
fn map_index_by_string_key() {
    let db = db();
    assert_eq!(one("RETURN {a: 1, b: 2}['b'] AS v", &db), Value::Integer(2));
    // Absent key -> NULL, never an error.
    assert_eq!(one("RETURN {a: 1}['missing'] AS v", &db), Value::Null);
}

#[test]
fn map_index_non_string_key_is_type_error() {
    let db = db();
    let e = run_err("RETURN {a: 1}[0] AS v", &db);
    assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
}

// ===== List slicing — core semantics ========================================

#[test]
fn slice_inclusive_from_exclusive_to() {
    let db = db();
    assert_eq!(one("RETURN [1, 2, 3, 4, 5][1..3] AS v", &db), ints(&[2, 3]));
}

#[test]
fn slice_open_bounds() {
    let db = db();
    assert_eq!(one("RETURN [1, 2, 3, 4][..2] AS v", &db), ints(&[1, 2]));
    assert_eq!(one("RETURN [1, 2, 3, 4][2..] AS v", &db), ints(&[3, 4]));
    assert_eq!(
        one("RETURN [1, 2, 3, 4][..] AS v", &db),
        ints(&[1, 2, 3, 4])
    );
}

#[test]
fn slice_negative_bounds() {
    let db = db();
    assert_eq!(
        one("RETURN [1, 2, 3, 4, 5][-3..-1] AS v", &db),
        ints(&[3, 4])
    );
}

#[test]
fn slice_out_of_range_bounds_clamp() {
    let db = db();
    assert_eq!(
        one("RETURN [1, 2, 3][-100..100] AS v", &db),
        ints(&[1, 2, 3])
    );
}

#[test]
fn slice_from_ge_to_is_empty() {
    let db = db();
    assert_eq!(
        one("RETURN [1, 2, 3][2..2] AS v", &db),
        Value::List(Vec::new())
    );
    assert_eq!(
        one("RETURN [1, 2, 3][3..1] AS v", &db),
        Value::List(Vec::new())
    );
}

#[test]
fn slice_null_base_propagates_null() {
    let db = db();
    assert_eq!(one("RETURN null[1..3] AS v", &db), Value::Null);
}

#[test]
fn slice_non_list_base_is_type_error() {
    let db = db();
    let e = run_err("RETURN 'abc'[0..1] AS v", &db);
    assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
}

// ===== Composition with other expression machinery ==========================

#[test]
fn index_chained_after_function_call() {
    let db = db();
    // range(1, 5) -> [1,2,3,4,5]; element 0 is 1.
    assert_eq!(one("RETURN range(1, 5)[0] AS v", &db), Value::Integer(1));
    // tail() then index.
    assert_eq!(one("RETURN range(1, 5)[-1] AS v", &db), Value::Integer(5));
}

#[test]
fn nested_index_into_list_of_lists() {
    let db = db();
    assert_eq!(
        one("RETURN [[1, 2], [3, 4]][1][0] AS v", &db),
        Value::Integer(3)
    );
}

#[test]
fn index_inside_arithmetic() {
    let db = db();
    assert_eq!(
        one("RETURN [10, 20, 30][1] + 5 AS v", &db),
        Value::Integer(25)
    );
}

// ===== CBT journal: a mood entry's tag list =================================

#[test]
fn cbt_journal_first_distortion_tag() {
    let db = db();
    exec(
        "CREATE (:Thought {title: 'spiralling', distortions: ['catastrophising', 'mind-reading']})",
        &db,
    );
    let rows = run("MATCH (t:Thought) RETURN t.distortions[0] AS first", &db);
    assert_eq!(rows, vec![vec![s("catastrophising")]]);
}

#[test]
fn cbt_journal_filter_by_indexed_tag() {
    let db = db();
    exec(
        "CREATE (:Thought {title: 'keep', distortions: ['all-or-nothing', 'x']})",
        &db,
    );
    exec(
        "CREATE (:Thought {title: 'drop', distortions: ['labelling', 'y']})",
        &db,
    );
    let rows = run(
        "MATCH (t:Thought) WHERE t.distortions[0] = 'all-or-nothing' RETURN t.title AS title",
        &db,
    );
    assert_eq!(rows, vec![vec![s("keep")]]);
}

// ===== Story / book editor: an ordered scene list ===========================

#[test]
fn story_editor_slice_opening_scenes() {
    let db = db();
    exec(
        "CREATE (:Chapter {title: 'one', scenes: ['arrival', 'conflict', 'twist', 'resolution']})",
        &db,
    );
    let rows = run("MATCH (c:Chapter) RETURN c.scenes[0..2] AS opening", &db);
    assert_eq!(
        rows,
        vec![vec![Value::List(vec![s("arrival"), s("conflict")])]]
    );
}

#[test]
fn story_editor_last_scene_via_negative_index() {
    let db = db();
    exec(
        "CREATE (:Chapter {title: 'one', scenes: ['arrival', 'conflict', 'resolution']})",
        &db,
    );
    assert_eq!(
        one("MATCH (c:Chapter) RETURN c.scenes[-1] AS finale", &db),
        s("resolution")
    );
}

// ===== IT task manager: a checklist =========================================

#[test]
fn task_manager_remaining_checklist_after_first() {
    let db = db();
    exec(
        "CREATE (:Task {title: 'deploy', steps: ['build', 'test', 'ship']})",
        &db,
    );
    let rows = run("MATCH (t:Task) RETURN t.steps[1..] AS remaining", &db);
    assert_eq!(rows, vec![vec![Value::List(vec![s("test"), s("ship")])]]);
}

// ===== ERP: line items on an order ==========================================

#[test]
fn erp_order_indexed_line_quantity() {
    let db = db();
    exec(
        "CREATE (:Order {title: 'PO-1', quantities: [5, 12, 3]})",
        &db,
    );
    assert_eq!(
        one("MATCH (o:Order) RETURN o.quantities[1] AS q", &db),
        Value::Integer(12)
    );
}

#[test]
fn erp_order_index_beyond_line_items_is_null() {
    let db = db();
    exec("CREATE (:Order {title: 'PO-2', quantities: [5]})", &db);
    assert_eq!(
        one("MATCH (o:Order) RETURN o.quantities[3] AS q", &db),
        Value::Null
    );
}

// ===== Bug tracker: a triage history ========================================

#[test]
fn bug_tracker_latest_status_via_negative_index() {
    let db = db();
    exec(
        "CREATE (:Bug {title: 'crash', history: ['open', 'triaged', 'in-progress', 'closed']})",
        &db,
    );
    assert_eq!(
        one("MATCH (b:Bug) RETURN b.history[-1] AS latest", &db),
        s("closed")
    );
}

#[test]
fn bug_tracker_group_by_first_status() {
    let db = db();
    exec(
        "CREATE (:Bug {title: 'a', history: ['open', 'closed']})",
        &db,
    );
    exec(
        "CREATE (:Bug {title: 'b', history: ['open', 'triaged']})",
        &db,
    );
    exec(
        "CREATE (:Bug {title: 'c', history: ['reopened', 'closed']})",
        &db,
    );
    // The indexed element becomes a grouping key alongside an aggregation.
    let rows = run(
        "MATCH (b:Bug) RETURN b.history[0] AS first, count(*) AS c ORDER BY first",
        &db,
    );
    assert_eq!(
        rows,
        vec![
            vec![s("open"), Value::Integer(2)],
            vec![s("reopened"), Value::Integer(1)],
        ]
    );
}

// ===== Unicode robustness ===================================================

#[test]
fn index_preserves_unicode_elements() {
    let db = db();
    exec(
        "CREATE (:Doc {title: 'i18n', tags: ['café', '日本語', '🚀']})",
        &db,
    );
    assert_eq!(one("MATCH (d:Doc) RETURN d.tags[1] AS t", &db), s("日本語"));
    assert_eq!(one("MATCH (d:Doc) RETURN d.tags[-1] AS t", &db), s("🚀"));
}
