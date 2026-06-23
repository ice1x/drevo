//! End-to-end Cypher `COUNT { ... }` counting-subquery tests — Phase 10
//! follow-up task `00153`.
//!
//! A counting subquery `COUNT { [MATCH] pattern [WHERE predicate] }` returns the
//! **number** of matches of the enclosed pattern, relative to the current row —
//! anchored on any variables the surrounding query has already bound. It is the
//! integer-valued sibling of the existential subquery (`00152`): where
//! `EXISTS { p }` tests whether at least one match exists, `COUNT { p }` counts
//! them, so `COUNT { p } > 0` is exactly `EXISTS { p }`. Like `EXISTS`, the
//! braces disambiguate the pattern from a grouped expression, so a bare node
//! `COUNT { (n) }` is legal, an optional leading `MATCH` keyword is accepted,
//! and an optional inner `WHERE` filters the matches before they are counted.
//!
//! Semantics exercised here mirror Neo4j:
//!
//! * `COUNT { (a)-[:R]->(b) }` evaluates to the integer match count,
//! * a leading `MATCH` keyword is accepted and equivalent,
//! * a bare node `COUNT { (n) }` is legal (the braces disambiguate) — it counts
//!   the anchored node when it matches the inline filter,
//! * an inner `WHERE` filters the matches before they are counted,
//! * the pattern is anchored on already-bound variables (each row only counts
//!   its own matches); new variables in the subquery are subquery-scoped,
//! * it composes in any expression slot — a `WHERE` comparison, a `RETURN`
//!   column, `ORDER BY`, arithmetic,
//! * a head variable already bound to `null` (an unmatched `OPTIONAL MATCH`
//!   node) makes the subquery `null` (three-valued logic) rather than a type
//!   error, and no match yields `0` (never `null`),
//! * the counting subquery is *not* the `count(*)` aggregation — `count(*)` and
//!   `count(x)` keep their aggregation meaning,
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

fn i(n: i64) -> Value {
    Value::Integer(n)
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
fn counts_outgoing_relationships_per_row() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (c:Person {name: 'Cara'})
         CREATE (l:Person {name: 'Loner'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (a)-[:KNOWS]->(c)
         CREATE (b)-[:KNOWS]->(c)",
        &d,
    );
    let rows = run(
        "MATCH (p:Person)
         RETURN p.name AS name, COUNT { (p)-[:KNOWS]->() } AS friends
         ORDER BY name",
        &d,
    );
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0], vec![s("Ann"), i(2)]);
    assert_eq!(rows[1], vec![s("Bob"), i(1)]);
    assert_eq!(rows[2], vec![s("Cara"), i(0)]);
    assert_eq!(rows[3], vec![s("Loner"), i(0)]);
}

#[test]
fn count_subquery_in_where_filters_by_threshold() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (c:Person {name: 'Cara'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (a)-[:KNOWS]->(c)
         CREATE (b)-[:KNOWS]->(c)",
        &d,
    );
    // Only Ann knows two or more people.
    let rows = run(
        "MATCH (p:Person) WHERE COUNT { (p)-[:KNOWS]->() } >= 2 RETURN p.name",
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
        "MATCH (p:Person)
         RETURN p.name AS name, COUNT { MATCH (p)-[:KNOWS]->() } AS n
         ORDER BY name",
        &d,
    );
    assert_eq!(rows[0], vec![s("Ann"), i(1)]);
    assert_eq!(rows[1], vec![s("Bob"), i(0)]);
}

#[test]
fn inner_where_filters_the_counted_matches() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob', age: 30})
         CREATE (c:Person {name: 'Cara', age: 17})
         CREATE (d:Person {name: 'Dan', age: 40})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (a)-[:KNOWS]->(c)
         CREATE (a)-[:KNOWS]->(d)",
        &d,
    );
    // Ann knows three people, but only two are adults (Bob, Dan).
    let rows = run(
        "MATCH (p:Person {name: 'Ann'})
         RETURN COUNT { (p)-[:KNOWS]->(f) WHERE f.age >= 18 } AS adults",
        &d,
    );
    assert_eq!(rows, vec![vec![i(2)]]);
}

#[test]
fn bare_node_subquery_counts_the_anchored_node_match() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann', active: true})
         CREATE (b:Person {name: 'Bob', active: false})",
        &d,
    );
    // `COUNT { (p {active: true}) }` re-tests the anchored node against the
    // inline property filter — 1 for Ann, 0 for Bob.
    let rows = run(
        "MATCH (p:Person)
         RETURN p.name AS name, COUNT { (p {active: true}) } AS n
         ORDER BY name",
        &d,
    );
    assert_eq!(rows[0], vec![s("Ann"), i(1)]);
    assert_eq!(rows[1], vec![s("Bob"), i(0)]);
}

#[test]
fn unbound_pattern_is_a_global_count() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (b)-[:KNOWS]->(a)",
        &d,
    );
    // The subquery introduces only fresh variables, so every Person row sees the
    // same global answer: how many KNOWS edges exist? Two.
    let rows = run(
        "MATCH (p:Person)
         RETURN p.name AS name, COUNT { ()-[:KNOWS]->() } AS total
         ORDER BY name",
        &d,
    );
    assert_eq!(rows[0], vec![s("Ann"), i(2)]);
    assert_eq!(rows[1], vec![s("Bob"), i(2)]);
}

#[test]
fn count_in_return_position_yields_an_integer_column() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    let rows = run(
        "MATCH (p:Person) RETURN p.name AS name, COUNT { (p)-[:KNOWS]->() } AS knows
         ORDER BY name",
        &d,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![s("Ann"), i(1)]);
    assert_eq!(rows[1], vec![s("Bob"), i(0)]);
}

#[test]
fn count_composes_in_arithmetic_and_order_by() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (c:Person {name: 'Cara'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (a)-[:KNOWS]->(c)",
        &d,
    );
    // Order by descending friend-count; Ann (2) first, then Bob/Cara (0).
    let rows = run(
        "MATCH (p:Person)
         RETURN p.name AS name, COUNT { (p)-[:KNOWS]->() } + 1 AS score
         ORDER BY score DESC, name",
        &d,
    );
    assert_eq!(rows[0], vec![s("Ann"), i(3)]);
    assert_eq!(rows[1], vec![s("Bob"), i(1)]);
    assert_eq!(rows[2], vec![s("Cara"), i(1)]);
}

#[test]
fn count_equals_zero_is_exists_negation() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (l:Person {name: 'Loner'})
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    // `COUNT { ... } = 0` is the counting form of `NOT EXISTS { ... }`.
    let rows = run(
        "MATCH (p:Person) WHERE COUNT { (p)-[:KNOWS]->() } = 0 RETURN p.name",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Bob", "Loner"]);
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
    // `q` is null for the unmatched OPTIONAL MATCH. `COUNT { (q)... }` over a
    // null anchor is null (not 0), so the `> 0` filter drops the row (null is
    // not true).
    let rows = run(
        "MATCH (p:Person)
         OPTIONAL MATCH (p)-[:NONEXISTENT]->(q)
         WITH p, q
         WHERE COUNT { (q)-[:KNOWS]->() } > 0
         RETURN p.name",
        &d,
    );
    assert!(rows.is_empty(), "null-anchored COUNT must not keep any row");
}

#[test]
fn null_anchor_yields_null_in_return_position() {
    let d = db();
    run("CREATE (a:Person {name: 'Ann'})", &d);
    // A null anchor produces a null count column, distinct from an honest 0.
    let rows = run(
        "MATCH (p:Person)
         OPTIONAL MATCH (p)-[:NONEXISTENT]->(q)
         WITH p, q
         RETURN COUNT { (q)-[:KNOWS]->() } AS n",
        &d,
    );
    assert_eq!(rows, vec![vec![Value::Null]]);
}

#[test]
fn multi_hop_pattern_inside_subquery() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (c:Person {name: 'Cara'})
         CREATE (e:Person {name: 'Eve'})
         CREATE (a)-[:KNOWS]->(b)
         CREATE (b)-[:KNOWS]->(c)
         CREATE (b)-[:KNOWS]->(e)",
        &d,
    );
    // Ann reaches two friends-of-a-friend (Cara, Eve) via Bob.
    let rows = run(
        "MATCH (p:Person {name: 'Ann'})
         RETURN COUNT { (p)-[:KNOWS]->()-[:KNOWS]->() } AS foaf",
        &d,
    );
    assert_eq!(rows, vec![vec![i(2)]]);
}

#[test]
fn count_subquery_is_not_the_count_aggregation() {
    let d = db();
    run(
        "CREATE (a:Person {name: 'Ann'})
         CREATE (b:Person {name: 'Bob'})
         CREATE (a)-[:KNOWS]->(b)",
        &d,
    );
    // `count(*)` aggregates over the result rows (2 people); the per-row
    // subquery counts Ann's edges (1) and Bob's (0). They coexist in one query.
    let rows = run(
        "MATCH (p:Person)
         RETURN count(*) AS people, sum(COUNT { (p)-[:KNOWS]->() }) AS edges",
        &d,
    );
    assert_eq!(rows, vec![vec![i(2), i(1)]]);
}

// ===== Scenario coverage ====================================================

#[test]
fn cbt_journal_thoughts_ranked_by_distortion_count() {
    let d = db();
    run(
        "CREATE (t1:Thought {text: 'I always fail'})
         CREATE (t2:Thought {text: 'That was hard'})
         CREATE (d1:Distortion {name: 'Overgeneralization'})
         CREATE (d2:Distortion {name: 'Catastrophizing'})
         CREATE (t1)-[:EXHIBITS]->(d1)
         CREATE (t1)-[:EXHIBITS]->(d2)
         CREATE (t2)-[:EXHIBITS]->(d1)",
        &d,
    );
    let rows = run(
        "MATCH (t:Thought)
         RETURN t.text AS text, COUNT { (t)-[:EXHIBITS]->(:Distortion) } AS distortions
         ORDER BY distortions DESC, text",
        &d,
    );
    assert_eq!(rows[0], vec![s("I always fail"), i(2)]);
    assert_eq!(rows[1], vec![s("That was hard"), i(1)]);
}

#[test]
fn story_editor_chapters_with_more_than_one_character() {
    let d = db();
    run(
        "CREATE (c1:Chapter {title: 'Arrival'})
         CREATE (c2:Chapter {title: 'Silence'})
         CREATE (h1:Character {name: 'Mara'})
         CREATE (h2:Character {name: 'Tomas'})
         CREATE (c1)-[:INTRODUCES]->(h1)
         CREATE (c1)-[:INTRODUCES]->(h2)
         CREATE (c2)-[:INTRODUCES]->(h1)",
        &d,
    );
    // Chapters that introduce more than one character: only 'Arrival'.
    let rows = run(
        "MATCH (c:Chapter)
         WHERE COUNT { (c)-[:INTRODUCES]->(:Character) } > 1
         RETURN c.title",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Arrival"]);
}

#[test]
fn task_manager_count_of_open_dependencies() {
    let d = db();
    run(
        "CREATE (a:Task {title: 'Deploy'})
         CREATE (b:Task {title: 'Write tests', status: 'open'})
         CREATE (c:Task {title: 'Lint', status: 'open'})
         CREATE (e:Task {title: 'Docs', status: 'done'})
         CREATE (a)-[:DEPENDS_ON]->(b)
         CREATE (a)-[:DEPENDS_ON]->(c)
         CREATE (a)-[:DEPENDS_ON]->(e)",
        &d,
    );
    // Deploy has three dependencies, two still open.
    let rows = run(
        "MATCH (t:Task {title: 'Deploy'})
         RETURN COUNT { (t)-[:DEPENDS_ON]->(dep) WHERE dep.status = 'open' } AS blockers",
        &d,
    );
    assert_eq!(rows, vec![vec![i(2)]]);
}

#[test]
fn erp_customers_by_order_count() {
    let d = db();
    run(
        "CREATE (a:Customer {name: 'Acme'})
         CREATE (b:Customer {name: 'Globex'})
         CREATE (o1:Order {id: 'O-1'})
         CREATE (o2:Order {id: 'O-2'})
         CREATE (a)-[:PLACED]->(o1)
         CREATE (a)-[:PLACED]->(o2)",
        &d,
    );
    let rows = run(
        "MATCH (c:Customer)
         RETURN c.name AS name, COUNT { (c)-[:PLACED]->(:Order) } AS orders
         ORDER BY name",
        &d,
    );
    assert_eq!(rows[0], vec![s("Acme"), i(2)]);
    assert_eq!(rows[1], vec![s("Globex"), i(0)]);
}

#[test]
fn bug_tracker_issues_with_multiple_active_assignees() {
    let d = db();
    run(
        "CREATE (i1:Issue {title: 'Crash on save'})
         CREATE (i2:Issue {title: 'Typo in label'})
         CREATE (e1:Engineer {name: 'Rae', active: true})
         CREATE (e2:Engineer {name: 'Sam', active: true})
         CREATE (e3:Engineer {name: 'Lee', active: false})
         CREATE (i1)-[:ASSIGNED_TO]->(e1)
         CREATE (i1)-[:ASSIGNED_TO]->(e2)
         CREATE (i2)-[:ASSIGNED_TO]->(e3)",
        &d,
    );
    // Issues with at least two *active* assignees: only 'Crash on save'.
    let rows = run(
        "MATCH (i:Issue)
         WHERE COUNT { (i)-[:ASSIGNED_TO]->(e) WHERE e.active = true } >= 2
         RETURN i.title",
        &d,
    );
    assert_eq!(sorted_col(&rows), vec!["Crash on save"]);
}

// ===== Parser-error surface =================================================

#[test]
fn count_without_brace_is_the_aggregation_not_a_subquery() {
    // `count(*)` keeps its aggregation meaning — this parses and runs as a
    // normal aggregate, not a subquery.
    let d = db();
    run("CREATE (a:Person)  CREATE (b:Person)", &d);
    let rows = run("MATCH (p:Person) RETURN count(*)", &d);
    assert_eq!(rows, vec![vec![i(2)]]);
}

#[test]
fn count_with_unclosed_brace_is_a_parse_error() {
    assert!(parse("MATCH (n) RETURN COUNT { (n)-[:R]->() ").is_err());
}
