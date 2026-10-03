//! Label predicates as boolean expressions — issue #568.
//!
//! In Neo4j `n:Label` (and `n:A:B`) is an ordinary boolean expression, so it
//! combines with `OR` / `AND` / `NOT`, can be projected in `RETURN`, and works
//! anywhere an expression does. drevo used to accept labels only inside node
//! patterns, so `WHERE n:A OR n:B` failed to parse. These tests cover the
//! expression form across the target scenario domains.

use std::collections::HashMap;

use drevo::cypher::executor::{execute_on_engine as execute, Value};
use drevo::cypher::parser::parse;
use drevo::native::NativeGraph;

fn run(source: &str, db: &NativeGraph) -> Vec<Vec<Value>> {
    let q = parse(source).unwrap_or_else(|e| panic!("parse `{source}`: {e}"));
    execute(&q, db, HashMap::new())
        .unwrap_or_else(|e| panic!("execute `{source}`: {e}"))
        .rows
}

fn strings(rows: &[Vec<Value>]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        })
        .collect()
}

/// Bug tracker: issues, bugs and a stray comment.
fn bug_tracker() -> NativeGraph {
    let db = NativeGraph::new();
    run(
        "CREATE (:Bug {title: 'crash on save'}), (:Bug {title: 'slow load'}), \
         (:Feature {title: 'dark mode'}), (:Comment {title: 'me too'})",
        &db,
    );
    db
}

#[test]
fn issue_568_label_predicates_combined_with_or_parse_and_filter() {
    let db = bug_tracker();
    let rows = run(
        "MATCH (n) WHERE n:Bug OR n:Feature RETURN n.title AS t ORDER BY t",
        &db,
    );
    assert_eq!(strings(&rows), ["crash on save", "dark mode", "slow load"]);
}

#[test]
fn issue_568_the_reported_delete_query_runs() {
    let db = NativeGraph::new();
    run(
        "CREATE (:ZzTwoPhase {title: 'a'}), (:ZzBlocked {title: 'b'}), (:Keep {title: 'c'})",
        &db,
    );
    let rows = run(
        "MATCH (n) WHERE n:ZzTwoPhase OR n:ZzBlocked DELETE n RETURN count(*) AS deleted",
        &db,
    );
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    let left = run("MATCH (n) RETURN n.title AS t", &db);
    assert_eq!(strings(&left), ["c"]);
}

#[test]
fn single_label_predicate_in_where() {
    let db = bug_tracker();
    let rows = run("MATCH (n) WHERE n:Comment RETURN n.title AS t", &db);
    assert_eq!(strings(&rows), ["me too"]);
}

#[test]
fn not_label_predicate_excludes_the_label() {
    let db = bug_tracker();
    let rows = run(
        "MATCH (n) WHERE NOT n:Bug RETURN n.title AS t ORDER BY t",
        &db,
    );
    assert_eq!(strings(&rows), ["dark mode", "me too"]);
}

#[test]
fn label_predicate_binds_tighter_than_and() {
    let db = bug_tracker();
    let rows = run(
        "MATCH (n) WHERE n:Bug AND n.title STARTS WITH 'slow' RETURN n.title AS t",
        &db,
    );
    assert_eq!(strings(&rows), ["slow load"]);
}

#[test]
fn chained_labels_require_all_of_them() {
    // ERP: a supplier that is also a customer.
    let db = NativeGraph::new();
    run(
        "CREATE (:Partner:Supplier {title: 'Acme'}), (:Partner {title: 'Globex'})",
        &db,
    );
    let rows = run(
        "MATCH (n) WHERE n:Partner:Supplier RETURN n.title AS t",
        &db,
    );
    assert_eq!(strings(&rows), ["Acme"]);
}

#[test]
fn label_predicate_is_projectable() {
    // Task manager: flag which items are blocked.
    let db = NativeGraph::new();
    run(
        "CREATE (:Task:Blocked {title: 'deploy'}), (:Task {title: 'review'})",
        &db,
    );
    let rows = run(
        "MATCH (t:Task) RETURN t.title AS t, t:Blocked AS blocked ORDER BY t",
        &db,
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::String("deploy".into()), Value::Bool(true)],
            vec![Value::String("review".into()), Value::Bool(false)],
        ]
    );
}

#[test]
fn label_predicate_on_null_is_null() {
    // CBT journal: a thought with no linked distortion.
    let db = NativeGraph::new();
    run("CREATE (:Thought {title: 'lonely'})", &db);
    let rows = run(
        "MATCH (t:Thought) OPTIONAL MATCH (t)-[:HAS]->(d) RETURN d:Distortion AS x",
        &db,
    );
    assert_eq!(rows, vec![vec![Value::Null]]);
}

#[test]
fn relationship_type_predicate() {
    // Story editor: chapters linked by FOLLOWS or REFERENCES.
    let db = NativeGraph::new();
    run(
        "CREATE (a:Chapter {title: 'one'})-[:FOLLOWS]->(b:Chapter {title: 'two'}), \
         (b)-[:REFERENCES]->(a)",
        &db,
    );
    let rows = run(
        "MATCH (x)-[r]->(y) WHERE r:FOLLOWS RETURN x.title AS t",
        &db,
    );
    assert_eq!(strings(&rows), ["one"]);
}

#[test]
fn label_predicate_in_with_where_and_case() {
    let db = bug_tracker();
    let rows = run(
        "MATCH (n) WITH n WHERE n:Bug OR n:Comment \
         RETURN CASE WHEN n:Bug THEN 'bug' ELSE 'other' END AS k, n.title AS t ORDER BY t",
        &db,
    );
    assert_eq!(strings(&rows), ["bug", "other", "bug"]);
}

#[test]
fn set_and_remove_label_syntax_still_works() {
    let db = NativeGraph::new();
    run("CREATE (:Task {title: 'x'})", &db);
    run("MATCH (n:Task) SET n:Done", &db);
    assert_eq!(
        run("MATCH (n) WHERE n:Done RETURN count(n) AS c", &db),
        vec![vec![Value::Integer(1)]]
    );
    run("MATCH (n:Task) REMOVE n:Done", &db);
    assert_eq!(
        run("MATCH (n) WHERE n:Done RETURN count(n) AS c", &db),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn map_literals_and_projections_keep_their_colons() {
    let db = bug_tracker();
    let rows = run(
        "MATCH (n:Feature) RETURN {k: n.title} AS m, n {name: n.title} AS p",
        &db,
    );
    assert_eq!(rows.len(), 1);
}

#[test]
fn label_predicate_on_a_scalar_is_a_type_error() {
    let db = NativeGraph::new();
    let q = parse("RETURN 'x':Foo AS b").expect("parse");
    let e = execute(&q, &db, HashMap::new()).expect_err("type error");
    assert!(e.to_string().contains("Node or Relationship"), "{e}");
}

#[test]
fn parenthesised_label_predicates_are_not_mistaken_for_patterns() {
    let db = bug_tracker();
    run("CREATE (:Bug:Archived {title: 'old crash'})", &db);
    let rows = run(
        "MATCH (n) WHERE (n:Bug OR n:Feature) AND NOT n:Archived \
         RETURN n.title AS t ORDER BY t",
        &db,
    );
    assert_eq!(strings(&rows), ["crash on save", "dark mode", "slow load"]);
}
