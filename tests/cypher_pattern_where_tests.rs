//! `WHERE` inside a node or relationship pattern (#867).
//!
//! `(p:Person WHERE p.age > 28)` and `-[r:KNOWS WHERE r.since > 2020]->`
//! mean the same as the predicate in the clause's `WHERE`.

use std::collections::HashMap;

use drevo::cypher::executor::{execute_on_engine as execute, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native::NativeGraph;

/// Ann (30) knows Bob (25) since 2020, Bob knows Cid (41) since 2021, Cid
/// works at Acme.
fn people() -> NativeGraph {
    let db = NativeGraph::new();
    run(
        "CREATE (a:Person {name: 'Ann', age: 30})-[:KNOWS {since: 2020}]->\
         (b:Person {name: 'Bob', age: 25}), \
         (b)-[:KNOWS {since: 2021}]->(c:Person {name: 'Cid', age: 41}), \
         (c)-[:WORKS_AT]->(:Company {name: 'Acme'})",
        &db,
    );
    db
}

fn try_run(source: &str, db: &NativeGraph) -> Result<ExecResult, String> {
    let query = parse(source).map_err(|e| e.to_string())?;
    execute(&query, db, HashMap::new()).map_err(|e| e.to_string())
}

fn run(source: &str, db: &NativeGraph) -> ExecResult {
    try_run(source, db).unwrap_or_else(|e| panic!("`{source}` failed: {e}"))
}

fn rows(source: &str, db: &NativeGraph) -> Vec<Vec<Value>> {
    run(source, db).rows
}

fn s(text: &str) -> Value {
    Value::String(text.into())
}

fn i(n: i64) -> Value {
    Value::Integer(n)
}

#[test]
fn a_node_pattern_filters_with_where() {
    let db = people();
    let got = rows(
        "MATCH (p:Person WHERE p.age > 28) RETURN p.name AS n ORDER BY n",
        &db,
    );
    assert_eq!(got, vec![vec![s("Ann")], vec![s("Cid")]]);
}

#[test]
fn a_relationship_pattern_filters_with_where() {
    let db = people();
    let got = rows(
        "MATCH (:Person)-[r:KNOWS WHERE r.since > 2020]->(b) RETURN b.name AS n, r.since AS y",
        &db,
    );
    assert_eq!(got, vec![vec![s("Cid"), i(2021)]]);
}

#[test]
fn the_predicate_sees_variables_bound_earlier_in_the_pattern() {
    let db = people();
    let got = rows(
        "MATCH (a:Person)-[:KNOWS]->(b WHERE b.age > a.age) RETURN a.name AS a, b.name AS b",
        &db,
    );
    assert_eq!(got, vec![vec![s("Bob"), s("Cid")]]);
}

#[test]
fn it_combines_with_a_property_map_and_the_clause_where() {
    let db = people();
    let got = rows(
        "MATCH (p:Person {name: 'Ann'} WHERE p.age > 28) WHERE p.age < 40 RETURN p.name AS n",
        &db,
    );
    assert_eq!(got, vec![vec![s("Ann")]]);
}

#[test]
fn optional_match_keeps_the_row_when_the_inline_where_rejects() {
    let db = people();
    let got = rows(
        "MATCH (p:Person {name: 'Ann'}) \
         OPTIONAL MATCH (p)-[:KNOWS]->(f WHERE f.age > 100) \
         RETURN p.name AS n, f",
        &db,
    );
    assert_eq!(got, vec![vec![s("Ann"), Value::Null]]);
}

#[test]
fn it_works_inside_an_exists_subquery_and_a_pattern_comprehension() {
    let db = people();
    let got = rows(
        "MATCH (p:Person) \
         WHERE EXISTS { (p)-[:KNOWS]->(f WHERE f.age > 40) } \
         RETURN p.name AS n",
        &db,
    );
    assert_eq!(got, vec![vec![s("Bob")]]);
    let got = rows(
        "MATCH (p:Person {name: 'Ann'}) \
         RETURN [(p)-[:KNOWS]->(f WHERE f.age < 40) | f.name] AS friends",
        &db,
    );
    assert_eq!(got, vec![vec![Value::List(vec![s("Bob")])]]);
}

#[test]
fn create_refuses_an_inline_where() {
    let db = people();
    let err = try_run("CREATE (p:Person WHERE p.age > 1)", &db)
        .expect_err("a predicate has no meaning in CREATE");
    assert!(err.contains("CREATE"), "the error names the clause: {err}");
}

#[test]
fn a_count_of_a_filtered_node_counts_only_what_passes() {
    let db = people();
    // A bare count of a label is answered from a counter; a WHERE inside
    // the node must not be.
    assert_eq!(
        rows(
            "MATCH (p:Person WHERE p.age > 28) RETURN count(p) AS c",
            &db
        ),
        vec![vec![i(2)]]
    );
    assert_eq!(
        rows("MATCH (n WHERE n.age > 28) RETURN count(*) AS c", &db),
        vec![vec![i(2)]]
    );
    assert_eq!(
        rows("MATCH (p:Person) RETURN count(p) AS c", &db),
        vec![vec![i(3)]]
    );
}

#[test]
fn a_node_bound_earlier_is_checked_again() {
    let db = people();
    let got = rows(
        "MATCH (p:Person) MATCH (p WHERE p.age < 30)-[:KNOWS]->(f) RETURN p.name AS n, f.name AS f",
        &db,
    );
    assert_eq!(got, vec![vec![s("Bob"), s("Cid")]]);
}

#[test]
fn a_predicate_that_is_null_or_not_a_boolean_does_not_match() {
    let db = people();
    assert_eq!(
        rows(
            "MATCH (n WHERE n.age > 28) RETURN n.name AS n ORDER BY n",
            &db
        ),
        vec![vec![s("Ann")], vec![s("Cid")]],
        "Acme has no age: null is not true"
    );
    assert!(rows("MATCH (p:Person WHERE p.name) RETURN p", &db).is_empty());
}

#[test]
fn an_anonymous_node_or_relationship_filters_by_what_is_bound_before() {
    let db = people();
    let got = rows(
        "MATCH (a:Person)-[:KNOWS WHERE a.age > 28]->(b) RETURN a.name AS a, b.name AS b",
        &db,
    );
    assert_eq!(got, vec![vec![s("Ann"), s("Bob")]]);
    let got = rows(
        "MATCH (a:Person)-[:KNOWS]->(WHERE a.age < 28) RETURN a.name AS a",
        &db,
    );
    assert_eq!(got, vec![vec![s("Bob")]]);
}

#[test]
fn it_filters_the_ends_of_a_variable_length_and_a_shortest_path() {
    let db = people();
    let got = rows(
        "MATCH (a:Person {name: 'Ann'})-[:KNOWS*1..3]->(b WHERE b.age > 40) RETURN b.name AS b",
        &db,
    );
    assert_eq!(got, vec![vec![s("Cid")]]);
    let got = rows(
        "MATCH p = shortestPath((a:Person WHERE a.name = 'Ann')-[*]-(c:Company WHERE c.name = 'Acme')) \
         RETURN length(p) AS hops",
        &db,
    );
    assert_eq!(got, vec![vec![i(3)]]);
}

#[test]
fn a_variable_length_relationship_refuses_an_inline_where() {
    let db = people();
    let err = try_run(
        "MATCH (a:Person)-[r:KNOWS*1..2 WHERE r.since > 2020]->(b) RETURN b",
        &db,
    )
    .expect_err("a predicate per hop is not supported");
    assert!(err.contains("variable-length"), "{err}");
}

#[test]
fn merge_refuses_an_inline_where_and_writes_nothing() {
    let db = people();
    let err = try_run("MERGE (p:Person {name: 'Dee'} WHERE p.age > 1)", &db)
        .expect_err("a predicate has no meaning in MERGE");
    assert!(err.contains("MERGE"), "{err}");
    assert_eq!(
        rows("MATCH (p:Person) RETURN count(p) AS c", &db),
        vec![vec![i(3)]]
    );
}

#[test]
fn where_may_use_a_parameter_a_function_and_a_count_subquery() {
    let db = people();
    let query = parse(
        "MATCH (p:Person WHERE p.age >= $min AND toLower(p.name) STARTS WITH 'c') RETURN p.name AS n",
    )
    .expect("parse");
    let got = execute(&query, &db, HashMap::from([("min".to_string(), i(18))])).expect("execute");
    assert_eq!(got.rows, vec![vec![s("Cid")]]);
    let got = rows(
        "MATCH (p:Person WHERE COUNT { (p)-[:KNOWS]->() } = 0) RETURN p.name AS n",
        &db,
    );
    assert_eq!(got, vec![vec![s("Cid")]]);
}

#[test]
fn a_pattern_without_where_and_a_malformed_one_are_as_before() {
    let db = people();
    assert_eq!(
        rows("MATCH (p:Person {name: 'Ann'}) RETURN p.age AS a", &db),
        vec![vec![i(30)]]
    );
    for query in [
        "MATCH (p:Person WHERE) RETURN p",
        "MATCH (p:Person WHERE p.age > 1 {name: 'Ann'}) RETURN p",
        "MATCH (p)-[r WHERE]->(q) RETURN p",
    ] {
        assert!(try_run(query, &db).is_err(), "`{query}` does not parse");
    }
}

mod with_the_property_index {
    use super::*;

    use drevo::native_service::NativeService;

    #[test]
    fn an_indexed_property_and_an_inline_where_give_the_same_rows() {
        let svc = NativeService::in_memory();
        let exec = |query: &str| {
            svc.execute(&parse(query).expect("parse"), HashMap::new())
                .unwrap_or_else(|e| panic!("`{query}`: {e}"))
                .rows
        };
        exec(
            "CREATE (:Task {status: 'open', points: 5, title: 'a'}), \
                    (:Task {status: 'open', points: 1, title: 'b'}), \
                    (:Task {status: 'done', points: 8, title: 'c'})",
        );
        assert_eq!(
            exec("MATCH (t:Task {status: 'open'} WHERE t.points > 2) RETURN t.title"),
            vec![vec![s("a")]]
        );
        assert_eq!(
            exec("MATCH (t:Task WHERE t.status = 'open' AND t.points > 2) RETURN count(t)"),
            vec![vec![i(1)]]
        );
        assert_eq!(
            exec("MATCH (t:Task WHERE t.points > 2) RETURN count(*)"),
            vec![vec![i(2)]]
        );
    }
}
