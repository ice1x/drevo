//! Small pieces of Neo4j 5 Cypher (#870): `nullIf`, `valueType`,
//! `char_length`, `btrim`, `normalize`, the `||` operator, `exists(pattern)`
//! and the `COLLECT { }` subquery.

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

fn one(source: &str) -> Value {
    let db = people();
    let mut got = rows(source, &db);
    assert_eq!(got.len(), 1, "`{source}` returns one row");
    got.remove(0).remove(0)
}

#[test]
fn null_if_gives_null_for_equal_values() {
    assert_eq!(one("RETURN nullIf(1, 1) AS v"), Value::Null);
    assert_eq!(one("RETURN nullIf(1, 2) AS v"), i(1));
    assert_eq!(one("RETURN nullIf('', '') AS v"), Value::Null);
}

#[test]
fn value_type_names_the_type() {
    assert_eq!(one("RETURN valueType(1) AS v"), s("INTEGER NOT NULL"));
    assert_eq!(one("RETURN valueType('a') AS v"), s("STRING NOT NULL"));
    assert_eq!(one("RETURN valueType(1.5) AS v"), s("FLOAT NOT NULL"));
    assert_eq!(one("RETURN valueType(true) AS v"), s("BOOLEAN NOT NULL"));
    assert_eq!(one("RETURN valueType(null) AS v"), s("NULL"));
}

#[test]
fn char_length_counts_characters() {
    assert_eq!(one("RETURN char_length('abc') AS v"), i(3));
    assert_eq!(one("RETURN character_length('мост') AS v"), i(4));
    assert_eq!(one("RETURN char_length(null) AS v"), Value::Null);
}

#[test]
fn btrim_trims_both_ends() {
    assert_eq!(one("RETURN btrim('  a  ') AS v"), s("a"));
    assert_eq!(one("RETURN btrim('xxaxx', 'x') AS v"), s("a"));
    assert_eq!(one("RETURN btrim(null) AS v"), Value::Null);
}

#[test]
fn normalize_composes_characters() {
    // `e` followed by a combining acute accent becomes the single `é`.
    assert_eq!(one("RETURN size(normalize('e\u{301}')) AS v"), i(1));
    assert_eq!(one("RETURN normalize('a') AS v"), s("a"));
}

#[test]
fn double_bar_concatenates_strings_and_lists() {
    assert_eq!(one("RETURN 'a' || 'b' AS v"), s("ab"));
    assert_eq!(
        one("RETURN [1] || [2, 3] AS v"),
        Value::List(vec![i(1), i(2), i(3)])
    );
    assert_eq!(one("RETURN 'a' || null AS v"), Value::Null);
}

#[test]
fn exists_takes_a_pattern() {
    let db = people();
    let got = rows(
        "MATCH (p:Person) WHERE exists((p)-[:KNOWS]->()) RETURN p.name AS n ORDER BY n",
        &db,
    );
    assert_eq!(got, vec![vec![s("Ann")], vec![s("Bob")]]);
    assert_eq!(
        one("RETURN exists((:Person)-[:WORKS_AT]->(:Company)) AS v"),
        Value::Bool(true)
    );
}

#[test]
fn exists_of_a_property_still_points_to_is_not_null() {
    let db = people();
    let err = try_run("MATCH (p:Person) WHERE exists(p.name) RETURN p", &db)
        .expect_err("the property form was removed in Neo4j 5");
    assert!(
        err.contains("IS NOT NULL"),
        "the error says what to write: {err}"
    );
}

#[test]
fn collect_subquery_returns_a_list_per_row() {
    let db = people();
    let got = rows(
        "MATCH (p:Person) \
         RETURN p.name AS n, COLLECT { MATCH (p)-[:KNOWS]->(f) RETURN f.name } AS friends \
         ORDER BY n",
        &db,
    );
    assert_eq!(
        got,
        vec![
            vec![s("Ann"), Value::List(vec![s("Bob")])],
            vec![s("Bob"), Value::List(vec![s("Cid")])],
            vec![s("Cid"), Value::List(vec![])],
        ]
    );
}

#[test]
fn collect_subquery_with_two_columns_is_an_error() {
    let db = people();
    let err = try_run(
        "MATCH (p:Person) RETURN COLLECT { MATCH (p)-[:KNOWS]->(f) RETURN f.name, f.age } AS x",
        &db,
    )
    .expect_err("a COLLECT subquery returns one column");
    assert!(
        err.contains("one column"),
        "the error names the rule: {err}"
    );
}

#[test]
fn null_if_sees_a_null_on_either_side() {
    assert_eq!(one("RETURN nullIf(1, null) AS v"), i(1));
    assert_eq!(one("RETURN nullIf(null, 1) AS v"), Value::Null);
    assert_eq!(one("RETURN nullIf('a', 'A') AS v"), s("a"));
    assert_eq!(
        one("RETURN coalesce(nullIf('', ''), 'unknown') AS v"),
        s("unknown")
    );
    let db = people();
    let err = try_run("RETURN nullIf(1) AS v", &db).expect_err("two arguments");
    assert!(err.contains("expected 2 argument(s)"), "{err}");
}

#[test]
fn value_type_names_lists_graph_values_and_dates() {
    for (expr, name) in [
        ("[1, 2]", "LIST<INTEGER NOT NULL> NOT NULL"),
        ("[1, null]", "LIST<INTEGER> NOT NULL"),
        ("[1, 'a']", "LIST<ANY> NOT NULL"),
        ("[]", "LIST<NOTHING> NOT NULL"),
        ("[[1]]", "LIST<LIST<INTEGER NOT NULL> NOT NULL> NOT NULL"),
        ("{a: 1}", "MAP NOT NULL"),
        ("date('2026-11-27')", "DATE NOT NULL"),
        ("datetime()", "ZONED DATETIME NOT NULL"),
        ("duration('P1D')", "DURATION NOT NULL"),
    ] {
        assert_eq!(
            one(&format!("RETURN valueType({expr}) AS v")),
            s(name),
            "{expr}"
        );
    }
    let db = people();
    let got = rows(
        "MATCH p = (a:Person {name: 'Ann'})-[r:KNOWS]->(b) \
         RETURN valueType(a), valueType(r), valueType(p), valueType(a.missing)",
        &db,
    );
    assert_eq!(
        got,
        vec![vec![
            s("NODE NOT NULL"),
            s("RELATIONSHIP NOT NULL"),
            s("PATH NOT NULL"),
            s("NULL")
        ]]
    );
}

#[test]
fn text_functions_refuse_what_is_not_text_and_know_their_forms() {
    assert_eq!(one("RETURN btrim('--a-b--', '-') AS v"), s("a-b"));
    assert_eq!(one("RETURN btrim('xyaxy', 'yx') AS v"), s("a"));
    assert_eq!(one("RETURN btrim('a', null) AS v"), Value::Null);
    assert_eq!(one("RETURN size(normalize('\u{e9}', 'NFD')) AS v"), i(2));
    assert_eq!(one("RETURN normalize('\u{fb01}', 'nfkc') AS v"), s("fi"));
    assert_eq!(one("RETURN normalize(null) AS v"), Value::Null);
    let db = people();
    for (query, part) in [
        ("RETURN char_length(5) AS v", "String"),
        ("RETURN btrim() AS v", "1 or 2"),
        ("RETURN normalize('a', 'NFX') AS v", "NFX"),
    ] {
        let err = try_run(query, &db).expect_err(query);
        assert!(err.contains(part), "`{query}`: {err}");
    }
}

#[test]
fn double_bar_binds_like_plus_and_takes_only_strings_or_lists() {
    assert_eq!(one("RETURN 'a' || 'b' || 'c' AS v"), s("abc"));
    assert_eq!(one("RETURN 'n=' || toString(1 + 2) AS v"), s("n=3"));
    assert_eq!(one("RETURN size([1] || [2] || []) AS v"), i(2));
    assert_eq!(one("RETURN [1] || null AS v"), Value::Null);
    assert_eq!(
        one("RETURN [x IN [1] || [2] | x * 10] AS v"),
        Value::List(vec![i(10), i(20)])
    );
    let db = people();
    for query in [
        "RETURN 1 || 2 AS v",
        "RETURN 'a' || 1 AS v",
        "RETURN [1] || 'a' AS v",
    ] {
        let err = try_run(query, &db).expect_err(query);
        assert!(err.contains("||"), "`{query}`: {err}");
    }
}

#[test]
fn a_single_bar_still_separates_labels_types_and_comprehensions() {
    let db = people();
    assert_eq!(
        rows("MATCH (n:Person|Company) RETURN count(n) AS c", &db),
        vec![vec![i(4)]]
    );
    assert_eq!(
        rows("MATCH ()-[r:KNOWS|WORKS_AT]->() RETURN count(r) AS c", &db),
        vec![vec![i(3)]]
    );
    assert_eq!(
        rows(
            "MATCH (p:Person {name: 'Ann'}) RETURN [(p)-[:KNOWS]->(f) | f.name] AS v",
            &db
        ),
        vec![vec![Value::List(vec![s("Bob")])]]
    );
}

#[test]
fn exists_of_a_pattern_is_a_boolean_anywhere() {
    let db = people();
    let got = rows(
        "MATCH (p:Person) \
         RETURN p.name AS n, exists((p)-[:WORKS_AT]->()) AS employed, \
                NOT exists((p)-[:KNOWS]->()) AS knows_nobody \
         ORDER BY n",
        &db,
    );
    assert_eq!(
        got,
        vec![
            vec![s("Ann"), Value::Bool(false), Value::Bool(false)],
            vec![s("Bob"), Value::Bool(false), Value::Bool(false)],
            vec![s("Cid"), Value::Bool(true), Value::Bool(true)],
        ]
    );
    // The brace form is unchanged.
    assert_eq!(
        rows(
            "MATCH (p:Person) WHERE EXISTS { (p)-[:WORKS_AT]->() } RETURN p.name AS n",
            &db
        ),
        vec![vec![s("Cid")]]
    );
    assert!(try_run("RETURN exists(1 + 1) AS v", &db).is_err());
    assert!(try_run("RETURN exists() AS v", &db).is_err());
}

#[test]
fn collect_subquery_filters_and_leaves_the_aggregation_alone() {
    let db = people();
    let got = rows(
        "MATCH (p:Person) \
         RETURN p.name AS n, \
                COLLECT { (p)-[k:KNOWS]->(f) WHERE k.since > 2020 RETURN f.name + '!' } AS late \
         ORDER BY n",
        &db,
    );
    assert_eq!(
        got,
        vec![
            vec![s("Ann"), Value::List(vec![])],
            vec![s("Bob"), Value::List(vec![s("Cid!")])],
            vec![s("Cid"), Value::List(vec![])],
        ]
    );
    // `collect(...)` the aggregation, and a variable of that name.
    assert_eq!(
        rows("MATCH (p:Person) RETURN size(collect(p.name)) AS c", &db),
        vec![vec![i(3)]]
    );
    assert_eq!(
        rows("WITH [1] AS collect RETURN collect[0] AS v", &db),
        vec![vec![i(1)]]
    );
    for (query, part) in [
        (
            "MATCH (p:Person) RETURN COLLECT { MATCH (p)-->(f) RETURN DISTINCT f.name } AS x",
            "DISTINCT",
        ),
        (
            "MATCH (p:Person) RETURN COLLECT { MATCH (p)-->(f) RETURN f.name ORDER BY f.name } AS x",
            "ORDER BY",
        ),
        ("MATCH (p:Person) RETURN COLLECT { MATCH (p)-->(f) } AS x", "RETURN"),
    ] {
        let err = try_run(query, &db).expect_err(query);
        assert!(err.contains(part), "`{query}`: {err}");
    }
}

#[test]
fn show_functions_lists_the_new_functions() {
    let db = people();
    let got = rows(
        "SHOW FUNCTIONS YIELD name \
         WHERE name IN ['nullIf', 'valueType', 'char_length', 'character_length', 'btrim', \
                        'normalize'] \
         RETURN count(name) AS c",
        &db,
    );
    assert_eq!(got, vec![vec![i(6)]]);
}
