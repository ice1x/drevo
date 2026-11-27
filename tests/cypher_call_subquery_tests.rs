//! `CALL { … }` subqueries (#865).
//!
//! The inner query runs once per incoming row and adds its RETURN columns to
//! the row. Variables come in through an importing `WITH` or the scope form
//! `CALL (p) { … }`.
//!
//! The scenario is an IT task manager's "newest item per owner" report.

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
fn a_standalone_subquery_returns_its_rows() {
    let db = people();
    let got = rows(
        "CALL { MATCH (p:Person) RETURN p.name AS n ORDER BY n LIMIT 1 } RETURN n",
        &db,
    );
    assert_eq!(got, vec![vec![s("Ann")]]);
}

#[test]
fn a_union_inside_the_subquery_is_processed_further() {
    let db = people();
    let got = rows(
        "CALL { RETURN 1 AS x UNION RETURN 2 AS x } RETURN sum(x) AS total",
        &db,
    );
    assert_eq!(got, vec![vec![i(3)]]);
}

#[test]
fn an_importing_with_runs_the_subquery_once_per_row() {
    let db = people();
    let got = rows(
        "MATCH (p:Person) \
         CALL { WITH p MATCH (p)-[:KNOWS]->(f) RETURN count(f) AS c } \
         RETURN p.name AS name, c ORDER BY name",
        &db,
    );
    assert_eq!(
        got,
        vec![
            vec![s("Ann"), i(1)],
            vec![s("Bob"), i(1)],
            vec![s("Cid"), i(0)]
        ]
    );
}

#[test]
fn the_scope_form_imports_the_named_variables() {
    let db = people();
    let got = rows(
        "MATCH (p:Person) \
         CALL (p) { MATCH (p)-[:KNOWS]->(f) RETURN count(f) AS c } \
         RETURN p.name AS name, c ORDER BY name",
        &db,
    );
    assert_eq!(
        got,
        vec![
            vec![s("Ann"), i(1)],
            vec![s("Bob"), i(1)],
            vec![s("Cid"), i(0)]
        ]
    );
}

#[test]
fn a_subquery_with_a_limit_gives_the_top_one_per_row() {
    let db = people();
    let got = rows(
        "UNWIND [26, 40] AS floor \
         CALL { WITH floor MATCH (p:Person) WHERE p.age > floor \
                RETURN p.name AS youngest ORDER BY p.age LIMIT 1 } \
         RETURN floor, youngest ORDER BY floor",
        &db,
    );
    assert_eq!(got, vec![vec![i(26), s("Ann")], vec![i(40), s("Cid")]]);
}

#[test]
fn a_writing_subquery_writes_once_per_row_and_keeps_the_rows() {
    let db = people();
    let got = rows(
        "MATCH (p:Person) CALL { WITH p SET p.seen = true } RETURN count(p) AS c",
        &db,
    );
    assert_eq!(got, vec![vec![i(3)]]);
    let got = rows("MATCH (p:Person) WHERE p.seen RETURN count(p) AS c", &db);
    assert_eq!(got, vec![vec![i(3)]]);
}

#[test]
fn a_variable_that_was_not_imported_is_a_new_one_inside() {
    let db = people();
    // The inner `p` is not the outer one: both KNOWS pairs match for each of
    // the three outer rows.
    let got = rows(
        "MATCH (p:Person) \
         CALL { MATCH (q)-[:KNOWS]->(f) RETURN q.name AS knower } \
         RETURN count(knower) AS c",
        &db,
    );
    assert_eq!(got, vec![vec![i(6)]]);
    assert!(
        try_run("MATCH (p:Person) CALL { RETURN p.name AS n } RETURN n", &db).is_err(),
        "`p` was not imported"
    );
}

#[test]
fn a_row_the_subquery_returns_nothing_for_is_dropped() {
    let db = people();
    let got = rows(
        "MATCH (p:Person) \
         CALL { WITH p MATCH (p)-[:WORKS_AT]->(c) RETURN c.name AS employer } \
         RETURN p.name AS name, employer",
        &db,
    );
    assert_eq!(got, vec![vec![s("Cid"), s("Acme")]]);
}

#[test]
fn union_without_all_removes_duplicates_for_each_row() {
    let db = people();
    let got = rows(
        "UNWIND [1, 2] AS n \
         CALL { WITH n RETURN n * 0 AS zero UNION WITH n RETURN 0 AS zero } \
         RETURN n, zero ORDER BY n",
        &db,
    );
    assert_eq!(got, vec![vec![i(1), i(0)], vec![i(2), i(0)]]);
    let got = rows(
        "CALL { RETURN 1 AS x UNION ALL RETURN 1 AS x } RETURN count(x) AS c",
        &db,
    );
    assert_eq!(got, vec![vec![i(2)]]);
}

#[test]
fn the_scope_form_takes_no_variables_or_all_of_them() {
    let db = people();
    let got = rows(
        "MATCH (p:Person {name: 'Ann'}) CALL () { RETURN 1 AS one } RETURN p.name AS n, one",
        &db,
    );
    assert_eq!(got, vec![vec![s("Ann"), i(1)]]);
    let got = rows(
        "MATCH (p:Person {name: 'Ann'}) WITH p, 2 AS k \
         CALL (*) { RETURN p.age * k AS doubled } RETURN doubled",
        &db,
    );
    assert_eq!(got, vec![vec![i(60)]]);
    assert!(
        try_run(
            "MATCH (p:Person) CALL () { RETURN p.name AS n } RETURN n",
            &db
        )
        .is_err(),
        "an empty scope imports nothing"
    );
    assert!(
        try_run("CALL (ghost) { RETURN 1 AS one } RETURN one", &db).is_err(),
        "the scope names a variable the outer query does not have"
    );
}

#[test]
fn subqueries_nest() {
    let db = people();
    let got = rows(
        "MATCH (p:Person {name: 'Ann'}) \
         CALL (p) { \
           MATCH (p)-[:KNOWS]->(f) \
           CALL (f) { MATCH (f)-[:KNOWS]->(g) RETURN g.name AS second } \
           RETURN f.name AS first, second \
         } \
         RETURN first, second",
        &db,
    );
    assert_eq!(got, vec![vec![s("Bob"), s("Cid")]]);
}

#[test]
fn a_procedure_can_be_called_inside() {
    let db = people();
    let got = rows(
        "CALL { CALL db.labels() YIELD label RETURN label } \
         RETURN label ORDER BY label",
        &db,
    );
    assert_eq!(got, vec![vec![s("Company")], vec![s("Person")]]);
}

#[test]
fn a_returned_column_named_like_an_outer_variable_is_refused() {
    let db = people();
    let err = try_run("MATCH (p:Person) CALL { WITH p RETURN p } RETURN p", &db)
        .expect_err("`p` is already a variable of the outer query");
    assert!(err.contains("`p`") && err.contains("another name"), "{err}");
}

#[test]
fn a_query_cannot_end_with_a_subquery_that_returns_rows() {
    let db = people();
    let err = try_run("MATCH (p:Person) CALL { WITH p RETURN p.name AS n }", &db)
        .expect_err("the rows have nowhere to go");
    assert!(err.contains("RETURN after it"), "{err}");
    // A subquery that only writes may end the query.
    run("MATCH (p:Person) CALL { WITH p SET p.checked = true }", &db);
    assert_eq!(
        rows("MATCH (p:Person) WHERE p.checked RETURN count(p) AS c", &db),
        vec![vec![i(3)]]
    );
}

#[test]
fn in_transactions_is_refused_by_name_and_writes_nothing() {
    let db = people();
    for query in [
        "MATCH (p:Person) CALL { WITH p SET p.touched = true } IN TRANSACTIONS OF 2 ROWS",
        "MATCH (p:Person) CALL { WITH p SET p.touched = true } IN TRANSACTIONS",
    ] {
        let err = try_run(query, &db).expect_err("IN TRANSACTIONS is not supported");
        assert!(err.contains("IN TRANSACTIONS"), "{err}");
    }
    assert_eq!(
        rows("MATCH (p:Person) WHERE p.touched RETURN count(p) AS c", &db),
        vec![vec![i(0)]]
    );
}

#[test]
fn a_schema_command_inside_is_refused() {
    let db = people();
    let err = try_run("CALL { DROP INDEX person_name IF EXISTS }", &db)
        .expect_err("a subquery reads and writes the graph, not the schema");
    assert!(err.contains("schema command"), "{err}");
    assert!(try_run(
        "CALL { CREATE INDEX person_name FOR (p:Person) ON (p.name) }",
        &db
    )
    .is_err());
}

#[test]
fn union_arms_must_both_return_or_neither() {
    let db = people();
    let err = try_run("CALL { RETURN 1 AS x UNION CREATE (:Tmp) } RETURN x", &db)
        .expect_err("one arm returns, the other does not");
    assert!(err.contains("UNION"), "{err}");
    assert_eq!(
        rows("MATCH (t:Tmp) RETURN count(t) AS c", &db),
        vec![vec![i(0)]]
    );
}

#[test]
fn an_unclosed_or_empty_subquery_is_a_syntax_error() {
    let db = people();
    for query in [
        "CALL { MATCH (p) RETURN p.name AS n RETURN n",
        "CALL { } RETURN 1",
        "CALL (p { RETURN 1 AS x } RETURN x",
    ] {
        assert!(try_run(query, &db).is_err(), "`{query}` does not parse");
    }
}

#[test]
fn very_deep_nesting_is_an_error_not_a_crash() {
    let db = people();
    let depth = 500;
    let query = format!(
        "{}RETURN 1 AS x{} RETURN x",
        "CALL { ".repeat(depth),
        " }".repeat(depth)
    );
    let err = try_run(&query, &db).expect_err("nested too deeply");
    assert!(err.contains("too deeply"), "{err}");
}

#[test]
fn a_procedure_call_still_parses_and_says_what_it_found_otherwise() {
    let db = people();
    assert_eq!(
        rows("CALL db.labels() YIELD label RETURN count(label) AS c", &db),
        vec![vec![i(2)]]
    );
    assert!(try_run("CALL 1", &db).is_err());
}

mod read_access {
    use super::*;

    use drevo::cypher::executor::{read_access_refusal, Access};
    use drevo::native_service::NativeService;

    #[test]
    fn a_write_inside_a_subquery_is_seen_at_any_depth() {
        for query in [
            "MATCH (p:Person) CALL { WITH p SET p.seen = true }",
            "MATCH (p:Person) CALL (p) { CALL (p) { CREATE (p)-[:SEEN]->(:Mark) } }",
            "CALL { RETURN 1 AS x UNION MATCH (n) DETACH DELETE n RETURN 1 AS x } RETURN x",
            "CALL { CALL drevo.memory.addMessage('s', 'user', 'hi') YIELD id RETURN id } RETURN id",
        ] {
            let parsed = parse(query).unwrap_or_else(|e| panic!("`{query}`: {e}"));
            assert!(
                read_access_refusal(&parsed).is_some(),
                "`{query}` is not a read"
            );
        }
        let read = parse("MATCH (p:Person) CALL { WITH p RETURN p.name AS n } RETURN n").unwrap();
        assert!(read_access_refusal(&read).is_none());
    }

    #[test]
    fn a_read_session_refuses_a_writing_subquery_and_writes_nothing() {
        let svc = NativeService::in_memory();
        svc.execute(
            &parse("CREATE (:Person {name: 'Ann'}), (:Person {name: 'Bob'})").unwrap(),
            HashMap::new(),
        )
        .expect("seed");
        let write = parse("MATCH (p:Person) CALL { WITH p SET p.seen = true }").unwrap();
        let err = svc
            .execute_with_access(&write, HashMap::new(), None, Access::Read)
            .expect_err("a write in READ access");
        assert!(err.to_string().contains("READ"), "{err}");
        let seen = parse("MATCH (p:Person) WHERE p.seen RETURN count(p) AS c").unwrap();
        let got = svc.execute(&seen, HashMap::new()).expect("count");
        assert_eq!(got.rows, vec![vec![i(0)]]);
    }

    #[test]
    fn the_service_commits_what_a_subquery_writes() {
        let svc = NativeService::in_memory();
        svc.execute(
            &parse("CREATE (:Person {name: 'Ann'}), (:Person {name: 'Bob'})").unwrap(),
            HashMap::new(),
        )
        .expect("seed");
        let write =
            parse("MATCH (p:Person) CALL { WITH p CREATE (p)-[:HAS]->(:Note {of: p.name}) }")
                .unwrap();
        let done = svc.execute(&write, HashMap::new()).expect("write");
        assert_eq!(done.stats.nodes_created, 2);
        assert_eq!(done.stats.relationships_created, 2);
        let notes =
            parse("MATCH (:Person)-[:HAS]->(n:Note) RETURN n.of AS of ORDER BY of").unwrap();
        let got = svc.execute(&notes, HashMap::new()).expect("read back");
        assert_eq!(got.rows, vec![vec![s("Ann")], vec![s("Bob")]]);
    }
}
