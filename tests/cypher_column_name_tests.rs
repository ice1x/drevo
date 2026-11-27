//! Names of RETURN columns without `AS` (#864).
//!
//! A column without an alias is named by the text of its expression, as in
//! Neo4j (`count(*)`, `max(p.age)`), so two of them never share a name and a
//! client that reads a row by column name loses nothing.

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

fn columns(source: &str, db: &NativeGraph) -> Vec<String> {
    run(source, db).columns
}

#[test]
fn two_aggregates_have_their_own_names() {
    let db = people();
    assert_eq!(
        columns("MATCH (p:Person) RETURN count(*), max(p.age)", &db),
        ["count(*)", "max(p.age)"]
    );
}

#[test]
fn function_calls_are_named_by_their_text() {
    let db = people();
    assert_eq!(
        columns(
            "MATCH (p:Person) RETURN p.name, toUpper(p.name), size(p.name) LIMIT 1",
            &db
        ),
        ["p.name", "toUpper(p.name)", "size(p.name)"]
    );
    assert_eq!(
        columns("MATCH (p:Person) RETURN labels(p), id(p) LIMIT 1", &db),
        ["labels(p)", "id(p)"]
    );
}

#[test]
fn an_operator_expression_is_named_by_its_text() {
    let db = people();
    assert_eq!(columns("RETURN 1 + 1, 2 * 3", &db), ["1 + 1", "2 * 3"]);
}

#[test]
fn no_column_is_called_expr() {
    let db = people();
    for query in [
        "MATCH (p:Person) RETURN count(*)",
        "MATCH (p:Person) RETURN collect(p.name)[0]",
        "RETURN [x IN [1, 2] | x * 2]",
        "RETURN CASE WHEN true THEN 1 ELSE 2 END",
        "MATCH (p:Person) RETURN p {.name} LIMIT 1",
    ] {
        let names = columns(query, &db);
        assert!(
            !names.contains(&"expr".to_string()),
            "`{query}` names its column {names:?}"
        );
    }
}

#[test]
fn an_alias_and_a_plain_property_keep_their_names() {
    let db = people();
    assert_eq!(
        columns(
            "MATCH (p:Person) RETURN p, p.name, count(*) AS c LIMIT 1",
            &db
        ),
        ["p", "p.name", "c"]
    );
}

#[test]
fn a_row_read_by_column_name_keeps_every_value() {
    let db = people();
    let got = run(
        "MATCH (p:Person) RETURN count(*), max(p.age), min(p.name)",
        &db,
    );
    let row: HashMap<String, Value> = got
        .columns
        .iter()
        .cloned()
        .zip(
            rows(
                "MATCH (p:Person) RETURN count(*), max(p.age), min(p.name)",
                &db,
            )
            .remove(0),
        )
        .collect();
    assert_eq!(row.len(), 3, "three columns, three names: {row:?}");
    assert_eq!(row.get("count(*)"), Some(&i(3)));
    assert_eq!(row.get("max(p.age)"), Some(&i(41)));
    assert_eq!(row.get("min(p.name)"), Some(&s("Ann")));
}

#[test]
fn the_name_is_the_text_as_written() {
    let db = people();
    assert_eq!(
        columns("MATCH (p:Person) RETURN  count( * ) ,max(p.age)", &db),
        ["count( * )", "max(p.age)"]
    );
    assert_eq!(
        columns("RETURN 'a', \"b\", null, [1, 2], {k: 1}", &db),
        ["'a'", "\"b\"", "null", "[1, 2]", "{k: 1}"]
    );
}

#[test]
fn a_parameter_and_a_property_keep_their_plain_names() {
    let db = people();
    let query = parse("MATCH (p:Person) RETURN p . name, $limit LIMIT 1").expect("parse");
    let got = execute(&query, &db, HashMap::from([("limit".to_string(), i(1))])).expect("execute");
    assert_eq!(got.columns, ["p.name", "$limit"]);
}

#[test]
fn a_counted_label_is_named_like_any_other_aggregate() {
    let db = people();
    // A bare `count` of one label is answered from the label's counter.
    assert_eq!(
        columns("MATCH (p:Person) RETURN count(p)", &db),
        ["count(p)"]
    );
    assert_eq!(
        columns("MATCH (p:Person) RETURN count(*)", &db),
        ["count(*)"]
    );
    assert_eq!(
        rows("MATCH (p:Person) RETURN count(*)", &db),
        vec![vec![i(3)]]
    );
}

#[test]
fn distinct_and_a_later_clause_do_not_change_the_name() {
    let db = people();
    assert_eq!(
        columns(
            "MATCH (p:Person) RETURN DISTINCT toUpper(p.name) ORDER BY toUpper(p.name) LIMIT 2",
            &db
        ),
        ["toUpper(p.name)"]
    );
}

#[test]
fn union_arms_agree_when_written_the_same() {
    let db = people();
    let got = run(
        "MATCH (p:Person) RETURN count(*) UNION ALL MATCH (c:Company) RETURN count(*)",
        &db,
    );
    assert_eq!(got.columns, ["count(*)"]);
    assert_eq!(got.rows, vec![vec![i(3)], vec![i(1)]]);
    let err =
        try_run("RETURN 1 + 1 UNION RETURN 2 + 2", &db).expect_err("two different column names");
    assert!(err.contains("UNION"), "{err}");
}

#[test]
fn the_parser_keeps_the_text_of_each_item() {
    use drevo::cypher::ast::{Clause, ProjectionItem};
    let query =
        parse("MATCH (p) RETURN p.age + 1 AS next, size(collect(p.name)), *").expect("parse");
    let Some(Clause::Return(ret)) = query.parts[0].query.clauses.last() else {
        panic!("the query ends with RETURN");
    };
    let texts: Vec<&str> = ret
        .items
        .iter()
        .map(|item| match item {
            ProjectionItem::Expression { text, .. } => text.as_str(),
            ProjectionItem::Star => "*",
        })
        .collect();
    assert_eq!(texts, ["p.age + 1", "size(collect(p.name))", "*"]);
}
