//! End-to-end Cypher list-comprehension tests — Phase 10 follow-up task `00146`.
//!
//! A list comprehension `[var IN list WHERE predicate | projection]` evaluates
//! `list`, binds each element to `var` in a child scope, keeps the elements for
//! which `predicate` holds, and collects `projection` of each survivor (or the
//! element itself when there is no `| projection`). It is the in-expression
//! counterpart to `UNWIND` + `collect`: where `UNWIND` flattens a list into
//! rows and `collect` folds rows back, a comprehension transforms a list into a
//! list *without leaving the row*.
//!
//! These cases drive the real parser → executor pipeline across the cross-cutting
//! semantics (filter-only / projection-only / both, null propagation, type
//! errors, order preservation, variable scoping, nesting, composition with
//! aggregation and graph reads) plus the five drevo target scenario domains
//! (CBT journal, story/book editor, IT task manager, ERP, bug tracker).

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

fn ints(v: &[Value]) -> Vec<i64> {
    v.iter()
        .map(|x| match x {
            Value::Integer(i) => *i,
            other => panic!("expected integer element, got {other:?}"),
        })
        .collect()
}

fn strings(v: &[Value]) -> Vec<String> {
    v.iter()
        .map(|x| match x {
            Value::String(s) => s.clone(),
            other => panic!("expected string element, got {other:?}"),
        })
        .collect()
}

/// The single list value returned by a one-row, one-column query.
fn list_cell(rows: &[Vec<Value>]) -> Vec<Value> {
    assert_eq!(rows.len(), 1, "expected exactly one row");
    match &rows[0][0] {
        Value::List(items) => items.clone(),
        other => panic!("expected list cell, got {other:?}"),
    }
}

// ===== Core semantics =======================================================

#[test]
fn comprehension_filter_and_projection() {
    let db = db();
    let rows = run(
        "RETURN [x IN [1, 2, 3, 4, 5] WHERE x % 2 = 0 | x * 10] AS r",
        &db,
    );
    assert_eq!(ints(&list_cell(&rows)), vec![20, 40]);
}

#[test]
fn comprehension_filter_only_keeps_elements_unchanged() {
    let db = db();
    let rows = run("RETURN [x IN [1, 2, 3, 4] WHERE x > 2] AS r", &db);
    assert_eq!(ints(&list_cell(&rows)), vec![3, 4]);
}

#[test]
fn comprehension_projection_only_maps_every_element() {
    let db = db();
    let rows = run("RETURN [x IN [1, 2, 3] | x + 100] AS r", &db);
    assert_eq!(ints(&list_cell(&rows)), vec![101, 102, 103]);
}

#[test]
fn comprehension_preserves_source_order() {
    let db = db();
    let rows = run("RETURN [x IN [5, 3, 9, 1] | x] AS r", &db);
    assert_eq!(ints(&list_cell(&rows)), vec![5, 3, 9, 1]);
}

#[test]
fn comprehension_over_empty_list_is_empty() {
    let db = db();
    let rows = run("RETURN [x IN [] WHERE x > 0 | x] AS r", &db);
    assert!(list_cell(&rows).is_empty());
}

#[test]
fn comprehension_where_filters_all_out_yields_empty_list() {
    let db = db();
    let rows = run("RETURN [x IN [1, 2, 3] WHERE x > 100 | x] AS r", &db);
    assert!(list_cell(&rows).is_empty());
}

#[test]
fn comprehension_null_list_propagates_to_null() {
    let db = db();
    // A missing property reaches the evaluator as null; the whole
    // comprehension is null rather than erroring (mirrors UNWIND / IN).
    let rows = run("RETURN [x IN null WHERE x > 0 | x] AS r", &db);
    assert_eq!(rows[0][0], Value::Null);
}

#[test]
fn comprehension_over_string_elements() {
    let db = db();
    let rows = run(
        "RETURN [w IN ['apple', 'kiwi', 'banana'] WHERE size(w) > 4 | toUpper(w)] AS r",
        &db,
    );
    assert_eq!(strings(&list_cell(&rows)), vec!["APPLE", "BANANA"]);
}

#[test]
fn comprehension_non_list_source_is_type_error() {
    let db = db();
    let e = run_err("RETURN [x IN 7 WHERE x > 0 | x] AS r", &db);
    assert!(
        matches!(e, ExecError::TypeMismatch { ref expected, .. } if expected == "List"),
        "got {e:?}"
    );
}

#[test]
fn comprehension_non_boolean_predicate_is_type_error() {
    let db = db();
    let e = run_err("RETURN [x IN [1, 2, 3] WHERE x + 1 | x] AS r", &db);
    assert!(
        matches!(e, ExecError::TypeMismatch { ref expected, .. } if expected == "Bool"),
        "got {e:?}"
    );
}

#[test]
fn comprehension_predicate_null_drops_element() {
    let db = db();
    // `null > 0` is null under three-valued logic → the element is dropped,
    // exactly like a `false` predicate (no error).
    let rows = run("RETURN [x IN [1, null, 3] WHERE x > 0 | x] AS r", &db);
    assert_eq!(ints(&list_cell(&rows)), vec![1, 3]);
}

// ===== Variable scoping =====================================================

#[test]
fn comprehension_variable_is_scoped_to_the_comprehension() {
    let db = db();
    // `x` bound by the comprehension does not leak: the outer RETURN sees the
    // row's `x`, not the loop variable.
    let rows = run(
        "WITH 99 AS x RETURN [x IN [1, 2] | x] AS inner, x AS outer",
        &db,
    );
    assert_eq!(ints(&list_cell(&[vec![rows[0][0].clone()]])), vec![1, 2]);
    assert_eq!(rows[0][1], Value::Integer(99));
}

#[test]
fn comprehension_projection_can_reference_outer_binding() {
    let db = db();
    // The child scope is a superset of the row, so the projection can mix the
    // loop variable with an outer binding.
    let rows = run(
        "WITH 10 AS base RETURN [x IN [1, 2, 3] | x + base] AS r",
        &db,
    );
    assert_eq!(ints(&list_cell(&rows)), vec![11, 12, 13]);
}

#[test]
fn nested_comprehension_inner_shadows_outer_variable() {
    let db = db();
    let rows = run("RETURN [x IN [1, 2] | [x IN [10, 20] | x]] AS r", &db);
    let outer = list_cell(&rows);
    assert_eq!(outer.len(), 2);
    for inner in &outer {
        match inner {
            Value::List(items) => assert_eq!(ints(items), vec![10, 20]),
            other => panic!("expected nested list, got {other:?}"),
        }
    }
}

// ===== Composition with graph reads + aggregation ===========================

#[test]
fn comprehension_over_a_node_list_property() {
    let db = db();
    run("CREATE (:Note {title: 'n1', scores: [3, 8, 1, 9, 5]})", &db);
    let rows = run(
        "MATCH (n:Note {title: 'n1'}) RETURN [s IN n.scores WHERE s >= 5 | s] AS high",
        &db,
    );
    assert_eq!(ints(&list_cell(&rows)), vec![8, 9, 5]);
}

#[test]
fn comprehension_result_feeds_size_function() {
    let db = db();
    let rows = run(
        "RETURN size([x IN [1, 2, 3, 4, 5, 6] WHERE x % 2 = 0]) AS evens",
        &db,
    );
    assert_eq!(rows[0][0], Value::Integer(3));
}

#[test]
fn comprehension_alongside_aggregation_in_projection() {
    let db = db();
    run("CREATE (:Bag {title: 'b', items: [1, 2, 3, 4]})", &db);
    // The comprehension is a group key; count(*) aggregates. Both coexist.
    let rows = run(
        "MATCH (b:Bag) RETURN [x IN b.items WHERE x > 2 | x] AS big, count(*) AS n",
        &db,
    );
    assert_eq!(ints(&list_cell(&[vec![rows[0][0].clone()]])), vec![3, 4]);
    assert_eq!(rows[0][1], Value::Integer(1));
}

#[test]
fn comprehension_in_where_via_membership() {
    let db = db();
    run("CREATE (:Item {title: 'a', n: 4})", &db);
    run("CREATE (:Item {title: 'b', n: 7})", &db);
    // Keep nodes whose n is one of the doubled small numbers [2,4,6].
    let rows = run(
        "MATCH (i:Item) WHERE i.n IN [x IN [1, 2, 3] | x * 2] RETURN i.title AS t",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("a".to_string()));
}

#[test]
fn comprehension_with_parameter_list() {
    let db = db();
    let mut params = HashMap::new();
    params.insert(
        "vals".to_string(),
        Value::List(vec![
            Value::Integer(2),
            Value::Integer(5),
            Value::Integer(8),
        ]),
    );
    let rows = run_with_params("RETURN [x IN $vals WHERE x > 3 | x] AS r", &db, params);
    assert_eq!(ints(&list_cell(&rows)), vec![5, 8]);
}

// ===== Scenario 1 — CBT journal =============================================

#[test]
fn cbt_filter_distressing_tags() {
    let db = db();
    run(
        "CREATE (:JournalEntry {title: 'monday', tags: ['anxious', 'calm', 'panic', 'hopeful']})",
        &db,
    );
    // Surface only the distressing tags for a coping-skills prompt.
    let rows = run(
        "MATCH (e:JournalEntry {title: 'monday'}) \
         RETURN [t IN e.tags WHERE t = 'anxious' OR t = 'panic'] AS distressing",
        &db,
    );
    assert_eq!(
        strings(&list_cell(&rows)),
        vec!["anxious".to_string(), "panic".to_string()]
    );
}

// ===== Scenario 2 — story / book editor =====================================

#[test]
fn story_uppercase_chapter_titles() {
    let db = db();
    run(
        "CREATE (:Book {title: 'Saga', chapters: ['dawn', 'noon', 'dusk']})",
        &db,
    );
    let rows = run(
        "MATCH (b:Book {title: 'Saga'}) RETURN [c IN b.chapters | toUpper(c)] AS headings",
        &db,
    );
    assert_eq!(
        strings(&list_cell(&rows)),
        vec!["DAWN".to_string(), "NOON".to_string(), "DUSK".to_string()]
    );
}

// ===== Scenario 3 — IT task manager =========================================

#[test]
fn tasks_select_overdue_estimates() {
    let db = db();
    run(
        "CREATE (:Sprint {title: 's1', estimates: [2, 8, 13, 1, 21]})",
        &db,
    );
    // Tasks estimated above the 8-point threshold need to be split.
    let rows = run(
        "MATCH (s:Sprint {title: 's1'}) RETURN [e IN s.estimates WHERE e > 8 | e] AS too_big",
        &db,
    );
    assert_eq!(ints(&list_cell(&rows)), vec![13, 21]);
}

// ===== Scenario 4 — ERP =====================================================

#[test]
fn erp_apply_discount_to_line_items() {
    let db = db();
    run(
        "CREATE (:Order {title: 'o1', amounts: [100, 250, 40]})",
        &db,
    );
    // 10% discount on every line, keeping only the lines worth charging.
    let rows = run(
        "MATCH (o:Order {title: 'o1'}) RETURN [a IN o.amounts WHERE a >= 50 | a - a / 10] AS net",
        &db,
    );
    assert_eq!(ints(&list_cell(&rows)), vec![90, 225]);
}

// ===== Scenario 5 — bug tracker =============================================

#[test]
fn bugs_keep_high_severity_ids() {
    let db = db();
    run(
        "CREATE (:Report {title: 'r1', severities: [1, 5, 3, 5, 2]})",
        &db,
    );
    // Count how many critical (severity 5) findings the report carries.
    let rows = run(
        "MATCH (r:Report {title: 'r1'}) \
         RETURN size([s IN r.severities WHERE s = 5]) AS critical",
        &db,
    );
    assert_eq!(rows[0][0], Value::Integer(2));
}
