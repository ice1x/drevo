//! End-to-end Cypher `EXISTS { ... }` existential-subquery tests — Phase 10
//! follow-up task `00152`.
//!
//! An existential subquery `EXISTS { [MATCH] pattern [WHERE predicate] }` used in
//! a boolean position (a `WHERE` filter, or anywhere an expression is expected)
//! tests whether **at least one** match of the enclosed pattern exists relative
//! to the current row — anchored on any variables the surrounding query has
//! already bound. It is the brace-delimited, richer sibling of the bare pattern
//! predicate (`00151`): where a pattern predicate is a single path in expression
//! position, `EXISTS { ... }` wraps the pattern in braces (so a bare node is
//! unambiguous), accepts an optional leading `MATCH` keyword, and accepts an
//! optional inner `WHERE` filter evaluated in each match's binding scope.
//!
//! Semantics exercised here mirror Neo4j:
//!
//! * `EXISTS { (a)-[:R]->(b) }` is an existence test, evaluating to a boolean,
//! * a leading `MATCH` keyword is accepted and equivalent,
//! * a bare node `EXISTS { (n) }` is legal (the braces disambiguate from a
//!   grouped expression) — it is `true` when the anchored node matches,
//! * an inner `WHERE` filters the matches before the existence test,
//! * the pattern is anchored on already-bound variables (each row only sees its
//!   own matches); new variables in the subquery are subquery-scoped,
//! * `NOT EXISTS { ... }` negates the test (no match → `true`),
//! * it composes with `AND` / `OR` and other predicates,
//! * a head variable already bound to `null` (an unmatched `OPTIONAL MATCH`
//!   node) makes the subquery `null` (three-valued logic) rather than a type
//!   error,
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
    let rows = run(
        "MATCH (p:Person) WHERE EXISTS { (p)-[:KNOWS]->() } RETURN p.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Ann"]);
}

#[test]
fn match_keyword_inside_braces_is_accepted_and_equivalent() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    let rows = run(
        "MATCH (p:Person) WHERE EXISTS { MATCH (p)-[:KNOWS]->() } RETURN p.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Ann"]);
}

#[test]
fn negated_exists_keeps_rows_without_a_match() {
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
        "MATCH (p:Person) WHERE NOT EXISTS { (p)-[:KNOWS]->() } RETURN p.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Bob", "Loner"]);
}

#[test]
fn inner_where_filters_the_matches() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob', age: 30})
         CREATE (c:Person {name: 'Cara', age: 17})
         CREATE (d:Person {name: 'Dan'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (d)-[:KNOWS]->(c)",
        &d,
    );
    // Only Ann knows someone aged >= 18 (Bob); Dan only knows Cara (17).
    let rows = run(
        "MATCH (p:Person)
         WHERE EXISTS { (p)-[:KNOWS]->(f) WHERE f.age >= 18 }
         RETURN p.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Ann"]);
}

#[test]
fn bare_node_subquery_tests_anchored_node_match() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann', active: true})
         CREATE (b:Person {name: 'Bob', active: false})",
        &d,
    );
    // `EXISTS { (p {active: true}) }` re-tests the anchored node against the
    // inline property filter — true only for Ann.
    let rows = run(
        "MATCH (p:Person) WHERE EXISTS { (p {active: true}) } RETURN p.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Ann"]);
}

#[test]
fn unbound_pattern_is_a_global_existence_scan() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    // The subquery introduces only fresh variables, so every Person row sees the
    // same global answer: does *any* KNOWS edge exist? It does → all rows kept.
    let rows = run(
        "MATCH (p:Person) WHERE EXISTS { ()-[:KNOWS]->() } RETURN p.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Ann", "Bob"]);
}

#[test]
fn exists_in_return_position_yields_a_boolean_column() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    let rows = run(
        "MATCH (p:Person) RETURN p.name AS name, EXISTS { (p)-[:KNOWS]->() } AS knows
         ORDER BY name",
        &d,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![s("Ann"), Value::Bool(true)]);
    assert_eq!(rows[1], vec![s("Bob"), Value::Bool(false)]);
}

#[test]
fn composes_with_and_or() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann', vip: true})
         CREATE (b:Person {name: 'Bob', vip: false})
         CREATE (c:Person {name: 'Cara', vip: true})
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    // VIPs who also know someone: only Ann (Cara is a VIP but isolated).
    let rows = run(
        "MATCH (p:Person)
         WHERE p.vip = true AND EXISTS { (p)-[:KNOWS]->() }
         RETURN p.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Ann"]);
}

#[test]
fn null_anchor_makes_subquery_null_under_optional_match() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    // `q` is null for the unmatched OPTIONAL MATCH. The `WITH` re-projects the
    // row so its `WHERE` is a standalone post-filter (a `WHERE` written directly
    // after `OPTIONAL MATCH` would instead bind to the optional match itself).
    // `EXISTS { (q)... }` over a null anchor is null, so the filter drops the
    // row (null is not true).
    let rows = run(
        "MATCH (p:Person)
         OPTIONAL MATCH (p)-[:NONEXISTENT]->(q)
         WITH p, q
         WHERE EXISTS { (q)-[:KNOWS]->() }
         RETURN p.name",
        &d,
    );
    assert!(
        rows.is_empty(),
        "null-anchored EXISTS must not keep any row"
    );
}

#[test]
fn multi_hop_pattern_inside_subquery() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (c:Person {name: 'Cara'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (b)-[:KNOWS]->(c)",
        &d,
    );
    // Only Ann has a friend-of-a-friend (Ann -> Bob -> Cara).
    let rows = run(
        "MATCH (p:Person)
         WHERE EXISTS { (p)-[:KNOWS]->()-[:KNOWS]->() }
         RETURN p.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Ann"]);
}

// ===== Scenario coverage ====================================================

#[test]
fn cbt_journal_thoughts_with_a_linked_distortion() {
    let d = db();
    run(
        "CREATE (t1:Thought {text: 'I always fail'})
         CREATE (t2:Thought {text: 'That was hard'})
         CREATE (dist:Distortion {name: 'Overgeneralization'})
         CREATE (t1)-[:EXHIBITS]->(dist)",
        &d,
    );
    let rows = run(
        "MATCH (t:Thought) WHERE EXISTS { (t)-[:EXHIBITS]->(:Distortion) }
         RETURN t.text",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["I always fail"]);
}

#[test]
fn story_editor_chapters_that_introduce_a_character() {
    let d = db();
    run(
        "CREATE (c1:Chapter {title: 'Arrival'})
         CREATE (c2:Chapter {title: 'Silence'})
         CREATE (hero:Character {name: 'Mara'})
         CREATE (c1)-[:INTRODUCES]->(hero)",
        &d,
    );
    let rows = run(
        "MATCH (c:Chapter) WHERE EXISTS { (c)-[:INTRODUCES]->(:Character) }
         RETURN c.title",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Arrival"]);
}

#[test]
fn task_manager_tasks_blocked_by_an_open_dependency() {
    let d = db();
    run(
        "CREATE (a:Task {title: 'Deploy', status: 'todo'})
         CREATE (b:Task {title: 'Write tests', status: 'open'})
         CREATE (c:Task {title: 'Lint', status: 'done'})
         CREATE (d:Task {title: 'Ship', status: 'todo'})
         CREATE (a)-[:DEPENDS_ON]->(b)
         CREATE (d)-[:DEPENDS_ON]->(c)",
        &d,
    );
    // Deploy depends on an *open* task; Ship's dependency is already done.
    let rows = run(
        "MATCH (t:Task)
         WHERE EXISTS { (t)-[:DEPENDS_ON]->(dep) WHERE dep.status = 'open' }
         RETURN t.title",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Deploy"]);
}

#[test]
fn erp_customers_without_any_order() {
    let d = db();
    run(
        "CREATE (a:Customer {name: 'Acme'})
         CREATE (b:Customer {name: 'Globex'})
         CREATE (o:Order {id: 'O-1'})
         CREATE (a)-[:PLACED]->(o)",
        &d,
    );
    let rows = run(
        "MATCH (c:Customer) WHERE NOT EXISTS { (c)-[:PLACED]->(:Order) }
         RETURN c.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Globex"]);
}

#[test]
fn bug_tracker_issues_assigned_to_an_active_engineer() {
    let d = db();
    run(
        "CREATE (i1:Issue {title: 'Crash on save'})
         CREATE (i2:Issue {title: 'Typo in label'})
         CREATE (e1:Engineer {name: 'Rae', active: true})
         CREATE (e2:Engineer {name: 'Sam', active: false})
         CREATE (i1)-[:ASSIGNED_TO]->(e1)
         CREATE (i2)-[:ASSIGNED_TO]->(e2)",
        &d,
    );
    let rows = run(
        "MATCH (i:Issue)
         WHERE EXISTS { (i)-[:ASSIGNED_TO]->(e) WHERE e.active = true }
         RETURN i.title",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Crash on save"]);
}

// ===== Parser-error surface =================================================

#[test]
fn exists_without_brace_is_a_parse_error() {
    // `EXISTS` is only a block subquery in this implementation; the deprecated
    // `exists(n.prop)` function form is not supported (use `IS NOT NULL`).
    assert!(parse("MATCH (n) WHERE EXISTS (n)-[:R]->() RETURN n").is_err());
}

#[test]
fn exists_with_unclosed_brace_is_a_parse_error() {
    assert!(parse("MATCH (n) WHERE EXISTS { (n)-[:R]->() RETURN n").is_err());
}
