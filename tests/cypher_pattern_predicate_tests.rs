//! End-to-end Cypher pattern-predicate tests — Phase 10 follow-up task `00151`.
//!
//! A pattern predicate `(a)-[:R]->(b)` used in a boolean position (a `WHERE`
//! filter, or anywhere an expression is expected) tests whether **at least one**
//! match of the path pattern exists relative to the current row — anchored on
//! any variables the surrounding query has already bound. It is the predicate-
//! valued sibling of the pattern comprehension (`00150`): where a pattern
//! comprehension *shapes a list* from a graph pattern, a pattern predicate
//! *tests existence* of one.
//!
//! Semantics exercised here mirror Neo4j:
//!
//! * a path with at least one relationship in expression position is an
//!   existence test, evaluating to a boolean,
//! * the pattern is anchored on the already-bound variables (each row only
//!   sees its own matches); new variables in the pattern are predicate-scoped,
//! * `NOT (a)-[:R]->(b)` negates the test (no match → `true`),
//! * it composes with `AND` / `OR` and other predicates,
//! * a head variable already bound to `null` (an unmatched `OPTIONAL MATCH`
//!   node) makes the predicate `null` (three-valued logic) rather than a type
//!   error,
//! * a bare parenthesised expression (`(a.age + 1)`, `(1 + 2) * 3`) is still
//!   ordinary grouping — only a path with `≥ 1` relationship is a predicate,
//!
//! plus the five scenario domains (CBT journal, story editor, task manager,
//! ERP, bug tracker) the drevo Cypher suite standardises on.

use std::collections::HashMap;

use drevo::cypher::executor::{execute, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

fn run(source: &str, drevo: &Drevo) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").rows
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

/// The single-column string values of a result, lexicographically sorted.
fn sorted_col(rows: &[Vec<Value>]) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected String value, got {other:?}"),
        })
        .collect();
    out.sort();
    out
}

// ===== Core semantics =======================================================

#[test]
fn keeps_only_rows_with_a_matching_relationship() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (l:Person {name: 'Loner'})
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    let rows = run("MATCH (p:Person) WHERE (p)-[:KNOWS]->() RETURN p.name", &d);
    assert_eq!(sorted_col(&rows), vec!["Ann"]);
}

#[test]
fn negated_pattern_predicate_keeps_rows_without_a_match() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (l:Person {name: 'Loner'})
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    // Bob and Loner have no *outgoing* KNOWS edge.
    let rows = run(
        "MATCH (p:Person) WHERE NOT (p)-[:KNOWS]->() RETURN p.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Bob", "Loner"]);
}

#[test]
fn target_label_constrains_the_existence_test() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (adm:Admin {name: 'Root'})
         CREATE (a)-[:KNOWS]->(adm)
         CREATE (b)-[:KNOWS]->(b)",
        &d,
    );
    let rows = run(
        "MATCH (p:Person) WHERE (p)-[:KNOWS]->(:Admin) RETURN p.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Ann"]);
}

#[test]
fn relationship_type_constrains_the_existence_test() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (a)-[:LIKES]->(b)",
        &d,
    );
    // Ann only has a LIKES edge, not KNOWS.
    let rows = run("MATCH (p:Person) WHERE (p)-[:KNOWS]->() RETURN p.name", &d);
    assert!(rows.is_empty(), "no KNOWS edge anywhere, got {rows:?}");
}

#[test]
fn anchors_on_the_bound_variable_so_each_row_sees_only_its_own_edges() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (x:Person {name: 'Xan'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (x)-[:KNOWS]->(a)",
        &d,
    );
    // Only Ann and Xan have an outgoing KNOWS; Bob does not.
    let rows = run("MATCH (p:Person) WHERE (p)-[:KNOWS]->() RETURN p.name", &d);
    assert_eq!(sorted_col(&rows), vec!["Ann", "Xan"]);
}

#[test]
fn multi_hop_pattern_predicate() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (c:Person {name: 'Cal'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (b)-[:KNOWS]->(c)",
        &d,
    );
    // Only Ann reaches a friend-of-a-friend through two KNOWS hops.
    let rows = run(
        "MATCH (p:Person) WHERE (p)-[:KNOWS]->()-[:KNOWS]->() RETURN p.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Ann"]);
}

#[test]
fn composes_with_and_or_other_predicates() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann', active: true})
         CREATE (b:Person {name: 'Bob', active: false})
         CREATE (c:Person {name: 'Cal', active: true})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (b)-[:KNOWS]->(c)",
        &d,
    );
    // active AND has an outgoing KNOWS → only Ann (Cal is active but edgeless).
    let rows = run(
        "MATCH (p:Person) WHERE p.active AND (p)-[:KNOWS]->() RETURN p.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Ann"]);
}

#[test]
fn pattern_predicate_in_return_position_yields_a_boolean() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    let rows = run(
        "MATCH (p:Person)
         RETURN p.name AS who, (p)-[:KNOWS]->() AS social
         ORDER BY who",
        &d,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], s("Ann"));
    assert_eq!(rows[0][1], Value::Bool(true));
    assert_eq!(rows[1][0], s("Bob"));
    assert_eq!(rows[1][1], Value::Bool(false));
}

#[test]
fn null_anchor_makes_the_predicate_null_not_an_error() {
    let d = db();
    run("CREATE (a:Person {name: 'Ann'})", &d);
    // No `:Ghost` node exists, so `g` is bound to null by OPTIONAL MATCH;
    // a pattern predicate anchored on it is null (three-valued), so the row
    // is dropped by WHERE rather than raising a type error.
    let rows = run(
        "MATCH (p:Person)
         OPTIONAL MATCH (g:Ghost)
         WITH p, g
         WHERE (g)-[:HAUNTS]->()
         RETURN p.name",
        &d,
    );
    assert!(
        rows.is_empty(),
        "null-anchored predicate drops the row, got {rows:?}"
    );
}

#[test]
fn null_anchor_in_return_yields_null() {
    let d = db();
    run("CREATE (a:Person {name: 'Ann'})", &d);
    let rows = run(
        "MATCH (p:Person)
         OPTIONAL MATCH (g:Ghost)
         RETURN (g)-[:HAUNTS]->() AS spooky",
        &d,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Null);
}

// ===== Grouping is unaffected (purely additive) =============================

#[test]
fn parenthesised_arithmetic_is_still_grouping() {
    let d = db();
    run("CREATE (a:N {age: 29})", &d);
    let rows = run("MATCH (a:N) WHERE (a.age + 1) > 29 RETURN a.age", &d);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(29));
}

#[test]
fn parenthesised_grouping_in_return_is_unaffected() {
    let d = db();
    run("CREATE (a:N {x: 1})", &d);
    let rows = run("MATCH (a:N) RETURN (1 + 2) * 3 AS v", &d);
    assert_eq!(rows[0][0], Value::Integer(9));
}

#[test]
fn bare_parenthesised_variable_is_grouping_not_a_predicate() {
    let d = db();
    run("CREATE (a:N {x: 7})", &d);
    // `(a).x` — `(a)` is a grouped variable, not a path pattern (no relationship).
    let rows = run("MATCH (a:N) RETURN (a).x AS v", &d);
    assert_eq!(rows[0][0], Value::Integer(7));
}

// ===== Scenario domains =====================================================

#[test]
fn task_manager_open_tasks_with_a_blocker() {
    let d = db();
    run(
        "CREATE (t1:Task {title: 'Ship release'})
         CREATE (t2:Task {title: 'Write docs'})
         CREATE (t3:Task {title: 'Fix flaky test'})
         CREATE (t1)-[:BLOCKED_BY]->(t3)",
        &d,
    );
    let rows = run(
        "MATCH (t:Task) WHERE (t)-[:BLOCKED_BY]->() RETURN t.title",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Ship release"]);
}

#[test]
fn bug_tracker_unassigned_bugs() {
    let d = db();
    run(
        "CREATE (b1:Bug {id: 'B-1'})
         CREATE (b2:Bug {id: 'B-2'})
         CREATE (dev:Dev {name: 'Mia'})
         CREATE (b1)-[:ASSIGNED_TO]->(dev)",
        &d,
    );
    let rows = run(
        "MATCH (b:Bug) WHERE NOT (b)-[:ASSIGNED_TO]->() RETURN b.id",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["B-2"]);
}

#[test]
fn erp_suppliers_who_ship_a_part() {
    let d = db();
    run(
        "CREATE (s1:Supplier {name: 'Acme'})
         CREATE (s2:Supplier {name: 'Globex'})
         CREATE (p:Part {sku: 'X-1'})
         CREATE (s1)-[:SUPPLIES]->(p)",
        &d,
    );
    let rows = run(
        "MATCH (s:Supplier) WHERE (s)-[:SUPPLIES]->(:Part) RETURN s.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Acme"]);
}

#[test]
fn story_editor_chapters_that_reference_a_character() {
    let d = db();
    run(
        "CREATE (c1:Chapter {title: 'Arrival'})
         CREATE (c2:Chapter {title: 'Epilogue'})
         CREATE (h:Character {name: 'Vera'})
         CREATE (c1)-[:MENTIONS]->(h)",
        &d,
    );
    let rows = run(
        "MATCH (c:Chapter) WHERE (c)-[:MENTIONS]->(:Character) RETURN c.title",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Arrival"]);
}

#[test]
fn cbt_journal_thoughts_linked_to_a_distortion() {
    let d = db();
    run(
        "CREATE (e:Entry {text: 'I failed the interview'})
         CREATE (e2:Entry {text: 'I went for a walk'})
         CREATE (d1:Distortion {kind: 'catastrophising'})
         CREATE (e)-[:EXHIBITS]->(d1)",
        &d,
    );
    let rows = run(
        "MATCH (e:Entry) WHERE (e)-[:EXHIBITS]->(:Distortion) RETURN e.text",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["I failed the interview"]);
}
