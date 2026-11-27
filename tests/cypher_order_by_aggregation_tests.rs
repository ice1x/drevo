//! ORDER BY after an aggregating RETURN / WITH (#863).
//!
//! `RETURN p.name, count(*) ORDER BY p.name` sorts by the grouping
//! expression, and `ORDER BY count(*)` by the aggregate, whether or not the
//! item has an alias. A variable that is neither stays an error.
//!
//! The scenario is a bug tracker's "how many per kind" report.

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
fn sorts_by_a_grouping_property_without_an_alias() {
    let db = people();
    let got = rows(
        "MATCH (p:Person) RETURN p.name, count(*) ORDER BY p.name DESC",
        &db,
    );
    assert_eq!(
        got,
        vec![
            vec![s("Cid"), i(1)],
            vec![s("Bob"), i(1)],
            vec![s("Ann"), i(1)]
        ]
    );
}

#[test]
fn sorts_by_a_grouping_function_call_without_an_alias() {
    let db = people();
    let got = rows(
        "MATCH ()-[r]->() RETURN type(r), count(*) ORDER BY type(r) DESC",
        &db,
    );
    assert_eq!(got, vec![vec![s("WORKS_AT"), i(1)], vec![s("KNOWS"), i(2)]]);
}

#[test]
fn sorts_by_the_grouping_expression_when_it_has_an_alias() {
    let db = people();
    let got = rows(
        "MATCH ()-[r]->() RETURN type(r) AS t, count(*) AS c ORDER BY type(r) DESC",
        &db,
    );
    assert_eq!(got, vec![vec![s("WORKS_AT"), i(1)], vec![s("KNOWS"), i(2)]]);
    let got = rows(
        "MATCH (p:Person) RETURN toLower(p.name) AS v, count(*) AS c \
         ORDER BY toLower(p.name) DESC",
        &db,
    );
    assert_eq!(
        got,
        vec![
            vec![s("cid"), i(1)],
            vec![s("bob"), i(1)],
            vec![s("ann"), i(1)]
        ]
    );
}

#[test]
fn sorts_by_the_aggregate_itself() {
    let db = people();
    let got = rows(
        "MATCH ()-[r]->() RETURN type(r) AS t, count(*) AS c ORDER BY count(*) DESC",
        &db,
    );
    assert_eq!(got, vec![vec![s("KNOWS"), i(2)], vec![s("WORKS_AT"), i(1)]]);
    let got = rows(
        "MATCH (p:Person) RETURN p.name AS v, sum(p.age) AS total ORDER BY sum(p.age) DESC",
        &db,
    );
    assert_eq!(
        got,
        vec![
            vec![s("Cid"), i(41)],
            vec![s("Ann"), i(30)],
            vec![s("Bob"), i(25)]
        ]
    );
}

#[test]
fn with_sorts_by_its_grouping_expression() {
    let db = people();
    let got = rows(
        "MATCH (p:Person) WITH p.name AS n, count(*) AS c ORDER BY p.name DESC RETURN n",
        &db,
    );
    assert_eq!(got, vec![vec![s("Cid")], vec![s("Bob")], vec![s("Ann")]]);
}

#[test]
fn a_variable_that_is_not_a_grouping_key_is_refused_by_name_of_the_rule() {
    let db = people();
    let err = try_run(
        "MATCH (p:Person) RETURN p.name AS v, count(*) AS c ORDER BY p.age",
        &db,
    )
    .expect_err("p.age is neither a grouping key nor an aggregate");
    assert!(
        err.contains("aggregation") && !err.contains("unbound variable"),
        "the error names the rule: {err}"
    );
}

#[test]
fn sorts_by_a_property_of_a_grouping_node_returned_under_an_alias() {
    let db = people();
    let got = rows(
        "MATCH (p:Person) RETURN p AS person, count(*) AS c ORDER BY p.name DESC",
        &db,
    );
    let names: Vec<Value> = rows(
        "MATCH (p:Person) RETURN p AS person, count(*) AS c ORDER BY person.name DESC",
        &db,
    )
    .into_iter()
    .map(|row| row[0].clone())
    .collect();
    assert_eq!(got.len(), 3);
    assert_eq!(
        got.into_iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>(),
        names
    );
}

#[test]
fn sorts_by_the_aggregate_first_and_the_group_second() {
    let db = people();
    run(
        "MATCH (b:Person {name: 'Bob'}) CREATE (b)-[:WORKS_AT]->(:Company {name: 'Zed'})",
        &db,
    );
    // KNOWS 2, WORKS_AT 2: the tie is broken by the type, descending.
    let got = rows(
        "MATCH ()-[r]->() RETURN type(r), count(*) ORDER BY count(*) DESC, type(r) DESC",
        &db,
    );
    assert_eq!(got, vec![vec![s("WORKS_AT"), i(2)], vec![s("KNOWS"), i(2)]]);
}

#[test]
fn sorts_by_an_expression_around_a_returned_aggregate() {
    let db = people();
    let got = rows(
        "MATCH (a:Person)-[:KNOWS]-(b) \
         RETURN a.name AS n, collect(b.name) AS friends \
         ORDER BY size(collect(b.name)) DESC, a.name",
        &db,
    );
    assert_eq!(
        got.iter().map(|row| row[0].clone()).collect::<Vec<_>>(),
        vec![s("Bob"), s("Ann"), s("Cid")]
    );
    let got = rows(
        "MATCH (p:Person) RETURN p.name AS n, sum(p.age) AS total ORDER BY 0 - sum(p.age)",
        &db,
    );
    assert_eq!(
        got.iter().map(|row| row[0].clone()).collect::<Vec<_>>(),
        vec![s("Cid"), s("Ann"), s("Bob")]
    );
}

#[test]
fn an_aggregate_that_is_not_returned_is_refused() {
    let db = people();
    for query in [
        "MATCH (p:Person) RETURN p.name AS n, count(*) AS c ORDER BY max(p.age)",
        // `count(DISTINCT x)` is not `count(x)`.
        "MATCH (p:Person) RETURN p.name AS n, count(p.age) AS c ORDER BY count(DISTINCT p.age)",
    ] {
        let err = try_run(query, &db).expect_err("the aggregate is not an item of the RETURN");
        assert!(
            err.contains("must also be an item of the RETURN"),
            "`{query}`: {err}"
        );
    }
}

#[test]
fn an_aggregate_in_order_by_without_an_aggregating_return_is_still_refused() {
    let db = people();
    assert!(try_run("MATCH (p:Person) RETURN p.name ORDER BY count(*)", &db).is_err());
}

#[test]
fn a_literal_that_only_looks_the_same_is_a_different_expression() {
    let db = people();
    // `p.name + 'a'` is returned; `p.name + 'b'` is not, and `p` is gone.
    let err = try_run(
        "MATCH (p:Person) RETURN p.name + 'a' AS v, count(*) AS c ORDER BY p.name + 'b'",
        &db,
    )
    .expect_err("a different expression");
    assert!(err.contains("aggregation"), "{err}");
}

#[test]
fn sorting_no_groups_returns_no_rows() {
    let db = people();
    let got = rows(
        "MATCH (p:Ghost) RETURN p.name, count(*) ORDER BY p.name, count(*) DESC",
        &db,
    );
    assert!(got.is_empty());
}

#[test]
fn skip_and_limit_cut_after_the_sort() {
    let db = people();
    let got = rows(
        "MATCH (p:Person) RETURN p.name, count(*) ORDER BY p.name DESC SKIP 1 LIMIT 1",
        &db,
    );
    assert_eq!(got, vec![vec![s("Bob"), i(1)]]);
}
