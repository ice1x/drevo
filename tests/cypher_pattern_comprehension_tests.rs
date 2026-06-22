//! End-to-end Cypher pattern-comprehension tests — Phase 10 follow-up task `00150`.
//!
//! A pattern comprehension `[ pattern WHERE pred | projection ]` builds a list
//! by matching `pattern` (a path with at least one relationship) relative to
//! the current row — anchored on any variables the surrounding query has
//! already bound — keeping the matches for which `pred` holds (when present),
//! and collecting `projection` over each survivor. It is the shaping idiom for
//! gathering a per-row list off the graph without a second `MATCH`:
//! `RETURN p, [(p)-[:KNOWS]->(f) | f.name] AS friends`.
//!
//! Semantics exercised here mirror Neo4j:
//!
//! * the pattern is anchored on the already-bound head variable, so each row
//!   only sees its own matches,
//! * the `| projection` is mandatory and runs in each match's binding scope,
//! * an optional `WHERE` filters matches under three-valued logic,
//! * no match yields an empty list (never `null`),
//! * a head variable already bound to `null` (an unmatched `OPTIONAL MATCH`
//!   node) yields an empty list rather than a type error,
//! * a non-boolean `WHERE` is a recoverable `TypeMismatch`,
//! * a pattern comprehension is a group key alongside an aggregation,
//!
//! plus the five scenario domains (CBT journal, story editor, task manager,
//! ERP, bug tracker) the drevo Cypher suite standardises on.

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

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

/// The single `List` value of a one-row, one-column result.
fn one_list(rows: &[Vec<Value>]) -> Vec<Value> {
    assert_eq!(rows.len(), 1, "expected exactly one row");
    assert_eq!(rows[0].len(), 1, "expected exactly one column");
    match &rows[0][0] {
        Value::List(items) => items.clone(),
        other => panic!("expected a List value, got {other:?}"),
    }
}

/// The string elements of a list, lexicographically sorted — for assertions
/// that do not care about match order.
fn sorted_strings(items: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = items
        .iter()
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => panic!("expected String element, got {other:?}"),
        })
        .collect();
    out.sort();
    out
}

// ===== Core semantics =======================================================

#[test]
fn collects_projected_property_off_each_match() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (c:Person {name: 'Cal'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (a)-[:KNOWS]->(c)",
        &d,
    );
    let list = one_list(&run(
        "MATCH (a:Person {name: 'Ann'})
         RETURN [(a)-[:KNOWS]->(f) | f.name] AS friends",
        &d,
    ));
    assert_eq!(sorted_strings(&list), vec!["Bob", "Cal"]);
}

#[test]
fn where_filters_matches_before_projection() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob', age: 40})
         CREATE (c:Person {name: 'Cal', age: 20})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (a)-[:KNOWS]->(c)",
        &d,
    );
    let list = one_list(&run(
        "MATCH (a:Person {name: 'Ann'})
         RETURN [(a)-[:KNOWS]->(f) WHERE f.age > 30 | f.name] AS friends",
        &d,
    ));
    assert_eq!(list, vec![s("Bob")]);
}

#[test]
fn no_match_yields_empty_list_not_null() {
    let d = db();
    run("CREATE (a:Person {name: 'Loner'})", &d);
    let list = one_list(&run(
        "MATCH (a:Person) RETURN [(a)-[:KNOWS]->(f) | f.name] AS friends",
        &d,
    ));
    assert!(
        list.is_empty(),
        "no relationship → empty list, got {list:?}"
    );
}

#[test]
fn anchors_on_the_bound_head_so_each_row_sees_only_its_own_matches() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (x:Person {name: 'Xan'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (x)-[:KNOWS]->(a)",
        &d,
    );
    let rows = run(
        "MATCH (p:Person)
         RETURN p.name AS who, [(p)-[:KNOWS]->(f) | f.name] AS friends
         ORDER BY who",
        &d,
    );
    // Ann → [Bob]; Bob → []; Xan → [Ann].
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], s("Ann"));
    assert_eq!(rows[0][1], Value::List(vec![s("Bob")]));
    assert_eq!(rows[1][0], s("Bob"));
    assert_eq!(rows[1][1], Value::List(vec![]));
    assert_eq!(rows[2][0], s("Xan"));
    assert_eq!(rows[2][1], Value::List(vec![s("Ann")]));
}

#[test]
fn multi_hop_pattern_reaches_friends_of_friends() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (c:Person {name: 'Cal'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (b)-[:KNOWS]->(c)",
        &d,
    );
    let list = one_list(&run(
        "MATCH (a:Person {name: 'Ann'})
         RETURN [(a)-[:KNOWS]->()-[:KNOWS]->(ff) | ff.name] AS fof",
        &d,
    ));
    assert_eq!(list, vec![s("Cal")]);
}

#[test]
fn projection_can_be_a_map_projection() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob', age: 40})
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    let list = one_list(&run(
        "MATCH (a:Person {name: 'Ann'})
         RETURN [(a)-[:KNOWS]->(f) | f {.name, .age}] AS cards",
        &d,
    ));
    assert_eq!(list.len(), 1);
    match &list[0] {
        Value::Map(m) => {
            assert_eq!(m.get("name"), Some(&s("Bob")));
            assert_eq!(m.get("age"), Some(&Value::Integer(40)));
        }
        other => panic!("expected a Map element, got {other:?}"),
    }
}

#[test]
fn relationship_type_restricts_matches() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (c:Person {name: 'Cal'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (a)-[:BLOCKS]->(c)",
        &d,
    );
    let list = one_list(&run(
        "MATCH (a:Person {name: 'Ann'})
         RETURN [(a)-[:KNOWS]->(f) | f.name] AS friends",
        &d,
    ));
    assert_eq!(list, vec![s("Bob")], "BLOCKS edge is excluded");
}

#[test]
fn incoming_direction_is_respected() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (b)-[:KNOWS]->(a)",
        &d,
    );
    let outgoing = one_list(&run(
        "MATCH (a:Person {name: 'Ann'})
         RETURN [(a)-[:KNOWS]->(f) | f.name] AS friends",
        &d,
    ));
    assert!(outgoing.is_empty(), "Ann has no outgoing KNOWS");
    let incoming = one_list(&run(
        "MATCH (a:Person {name: 'Ann'})
         RETURN [(a)<-[:KNOWS]-(f) | f.name] AS admirers",
        &d,
    ));
    assert_eq!(incoming, vec![s("Bob")]);
}

#[test]
fn relationship_variable_is_bound_in_the_projection() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (a)-[:KNOWS {since: 2019}]->(b)",
        &d,
    );
    let list = one_list(&run(
        "MATCH (a:Person {name: 'Ann'})
         RETURN [(a)-[r:KNOWS]->(f) | r.since] AS years",
        &d,
    ));
    assert_eq!(list, vec![Value::Integer(2019)]);
}

#[test]
fn preserves_order_and_duplicates_of_matches() {
    let d = db();
    // Two parallel KNOWS edges to the same person → two list elements.
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    let list = one_list(&run(
        "MATCH (a:Person {name: 'Ann'})
         RETURN [(a)-[:KNOWS]->(f) | f.name] AS friends",
        &d,
    ));
    assert_eq!(list, vec![s("Bob"), s("Bob")], "duplicate edge → duplicate");
}

#[test]
fn parameter_in_predicate_filters_matches() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob', age: 40})
         CREATE (c:Person {name: 'Cal', age: 25})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (a)-[:KNOWS]->(c)",
        &d,
    );
    let mut params = HashMap::new();
    params.insert("min".to_string(), Value::Integer(30));
    let list = one_list(&run_with_params(
        "MATCH (a:Person {name: 'Ann'})
         RETURN [(a)-[:KNOWS]->(f) WHERE f.age >= $min | f.name] AS friends",
        &d,
        params,
    ));
    assert_eq!(list, vec![s("Bob")]);
}

#[test]
fn usable_in_with_and_carried_forward() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (c:Person {name: 'Cal'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (a)-[:KNOWS]->(c)",
        &d,
    );
    let rows = run(
        "MATCH (a:Person {name: 'Ann'})
         WITH [(a)-[:KNOWS]->(f) | f.name] AS friends
         RETURN size(friends) AS n",
        &d,
    );
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn is_a_group_key_alongside_an_aggregation() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    // The comprehension column is the group key; count(*) aggregates each group.
    let rows = run(
        "MATCH (a:Person)
         RETURN [(a)-[:KNOWS]->(f) | f.name] AS friends, count(*) AS c
         ORDER BY c",
        &d,
    );
    assert_eq!(rows.len(), 2, "Ann's group and Bob's empty-list group");
    // Each distinct list value forms its own one-row group here.
    for row in &rows {
        assert_eq!(row[1], Value::Integer(1));
    }
}

// ===== Null / error paths ===================================================

#[test]
fn null_anchor_from_optional_match_yields_empty_list() {
    let d = db();
    run("CREATE (a:Person {name: 'Ann'})", &d);
    // No Manager exists, so `m` is null after OPTIONAL MATCH; the comprehension
    // anchored on the null head yields [] rather than a TypeMismatch.
    let rows = run(
        "MATCH (a:Person)
         OPTIONAL MATCH (a)-[:REPORTS_TO]->(m:Manager)
         RETURN [(m)-[:KNOWS]->(f) | f.name] AS peers",
        &d,
    );
    assert_eq!(rows, vec![vec![Value::List(vec![])]]);
}

#[test]
fn non_boolean_predicate_is_a_type_mismatch() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob', age: 40})
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    let err = run_err(
        "MATCH (a:Person {name: 'Ann'})
         RETURN [(a)-[:KNOWS]->(f) WHERE f.age | f.name] AS friends",
        &d,
    );
    assert!(
        matches!(err, ExecError::TypeMismatch { .. }),
        "non-boolean WHERE → TypeMismatch, got {err:?}"
    );
}

// ===== Scenario domains =====================================================

#[test]
fn cbt_collects_distortions_per_thought() {
    let d = db();
    run(
        "CREATE (t:Thought {text: 'I always fail'})
         CREATE (d1:Distortion {label: 'overgeneralization'})
         CREATE (d2:Distortion {label: 'all-or-nothing'})
         CREATE (t)-[:HAS_DISTORTION]->(d1)
         CREATE (t)-[:HAS_DISTORTION]->(d2)",
        &d,
    );
    let list = one_list(&run(
        "MATCH (t:Thought)
         RETURN [(t)-[:HAS_DISTORTION]->(d) | d.label] AS labels",
        &d,
    ));
    assert_eq!(
        sorted_strings(&list),
        vec!["all-or-nothing", "overgeneralization"]
    );
}

#[test]
fn story_collects_chapter_titles_per_book() {
    let d = db();
    run(
        "CREATE (b:Book {title: 'The Road'})
         CREATE (c1:Chapter {title: 'Dawn', ord: 1})
         CREATE (c2:Chapter {title: 'Dusk', ord: 2})
         CREATE (b)-[:HAS_CHAPTER]->(c1)
         CREATE (b)-[:HAS_CHAPTER]->(c2)",
        &d,
    );
    let list = one_list(&run(
        "MATCH (b:Book)
         RETURN [(b)-[:HAS_CHAPTER]->(c) | c.title] AS chapters",
        &d,
    ));
    assert_eq!(sorted_strings(&list), vec!["Dawn", "Dusk"]);
}

#[test]
fn task_collects_open_subtask_ids() {
    let d = db();
    run(
        "CREATE (e:Epic {key: 'PROJ-1'})
         CREATE (a:Task {key: 'PROJ-2', status: 'open'})
         CREATE (b:Task {key: 'PROJ-3', status: 'done'})
         CREATE (c:Task {key: 'PROJ-4', status: 'open'})
         CREATE (e)-[:HAS_SUBTASK]->(a)
         CREATE (e)-[:HAS_SUBTASK]->(b)
         CREATE (e)-[:HAS_SUBTASK]->(c)",
        &d,
    );
    let list = one_list(&run(
        "MATCH (e:Epic)
         RETURN [(e)-[:HAS_SUBTASK]->(t) WHERE t.status = 'open' | t.key] AS open_tasks",
        &d,
    ));
    assert_eq!(sorted_strings(&list), vec!["PROJ-2", "PROJ-4"]);
}

#[test]
fn erp_collects_high_quantity_line_items() {
    let d = db();
    run(
        "CREATE (o:Order {id: 'SO-100'})
         CREATE (a:Item {sku: 'WIDGET', qty: 5})
         CREATE (b:Item {sku: 'BOLT', qty: 1})
         CREATE (o)-[:CONTAINS]->(a)
         CREATE (o)-[:CONTAINS]->(b)",
        &d,
    );
    let list = one_list(&run(
        "MATCH (o:Order)
         RETURN [(o)-[:CONTAINS]->(i) WHERE i.qty > 1 | i.sku] AS bulk",
        &d,
    ));
    assert_eq!(list, vec![s("WIDGET")]);
}

#[test]
fn bug_collects_label_names_with_relationship_property() {
    let d = db();
    run(
        "CREATE (g:Bug {id: 'BUG-7'})
         CREATE (l1:Label {name: 'regression'})
         CREATE (l2:Label {name: 'ui'})
         CREATE (g)-[:TAGGED {at: 2024}]->(l1)
         CREATE (g)-[:TAGGED {at: 2025}]->(l2)",
        &d,
    );
    // Project a map combining the label name and the edge property.
    let list = one_list(&run(
        "MATCH (g:Bug)
         RETURN [(g)-[r:TAGGED]->(l) | l {.name, tagged_at: r.at}] AS tags",
        &d,
    ));
    assert_eq!(list.len(), 2);
    let mut names: Vec<String> = list
        .iter()
        .map(|v| match v {
            Value::Map(m) => match m.get("name") {
                Some(Value::String(s)) => s.clone(),
                other => panic!("expected name string, got {other:?}"),
            },
            other => panic!("expected Map element, got {other:?}"),
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["regression", "ui"]);
}
