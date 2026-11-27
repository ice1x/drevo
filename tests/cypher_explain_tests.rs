//! `EXPLAIN` and `PROFILE` (#868).
//!
//! `EXPLAIN <query>` checks the query and describes how it would run without
//! running it; `PROFILE <query>` runs it and reports what each step did.

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

fn count(label: &str, db: &NativeGraph) -> Value {
    rows(&format!("MATCH (n:{label}) RETURN count(n) AS c"), db)[0][0].clone()
}

#[test]
fn explain_parses_and_runs_nothing() {
    let db = people();
    run("EXPLAIN CREATE (:Ghost {name: 'never written'})", &db);
    assert_eq!(count("Ghost", &db), i(0));
}

#[test]
fn explain_of_a_read_returns_none_of_its_rows() {
    let db = people();
    let got = run("EXPLAIN MATCH (p:Person) RETURN p.name AS n", &db);
    assert!(
        !got.rows.iter().flatten().any(|v| *v == s("Ann")),
        "the query itself did not run: {:?}",
        got.rows
    );
}

#[test]
fn explain_reports_a_semantic_error_of_the_query() {
    let db = people();
    let err =
        try_run("EXPLAIN MATCH (p:Person) RETURN q.name", &db).expect_err("`q` is not defined");
    assert!(
        err.contains("`q`") && !err.contains("EXPLAIN"),
        "the error is about the query: {err}"
    );
}

#[test]
fn explain_names_the_label_it_starts_from() {
    let db = people();
    let got = run("EXPLAIN MATCH (p:Person {name: 'Ann'}) RETURN p", &db);
    let text = format!("{:?} {:?}", got.columns, got.rows);
    assert!(
        text.contains("Person"),
        "the plan names the label scanned: {text}"
    );
}

#[test]
fn profile_runs_the_query() {
    let db = people();
    run("PROFILE CREATE (:Ghost {name: 'written'})", &db);
    assert_eq!(count("Ghost", &db), i(1));
}

#[test]
fn explain_is_case_insensitive_like_other_keywords() {
    let db = people();
    run("explain MATCH (n) RETURN n", &db);
    run("profile MATCH (n) RETURN count(n)", &db);
}

fn plan(query: &str, db: &NativeGraph) -> Vec<String> {
    run(query, db)
        .rows
        .into_iter()
        .map(|row| match (&row[1], &row[2]) {
            (Value::String(operator), Value::String(details)) if details.is_empty() => {
                operator.clone()
            }
            (Value::String(operator), Value::String(details)) => format!("{operator}: {details}"),
            other => panic!("a plan row is (step, operator, details): {other:?}"),
        })
        .collect()
}

#[test]
fn the_plan_is_a_table_of_numbered_steps() {
    let db = people();
    let got = run(
        "EXPLAIN MATCH (p:Person)-[:KNOWS]->(f) WHERE f.age > 26 \
         RETURN p.name AS name, count(f) AS friends ORDER BY friends DESC LIMIT 5",
        &db,
    );
    assert_eq!(got.columns, ["step", "operator", "details"]);
    let steps: Vec<Value> = got.rows.iter().map(|row| row[0].clone()).collect();
    assert_eq!(steps, (1..=6).map(i).collect::<Vec<_>>());
    assert_eq!(
        plan(
            "EXPLAIN MATCH (p:Person)-[:KNOWS]->(f) WHERE f.age > 26 \
             RETURN p.name AS name, count(f) AS friends ORDER BY friends DESC LIMIT 5",
            &db
        ),
        [
            "AllNodesScan: (p:Person), kept by label",
            "Expand: (p)-[:KNOWS]->(f)",
            "Filter: f.age > 26",
            "Aggregation: count(f) AS friends for each p.name AS name",
            "Sort: friends DESC",
            "Limit: 5",
        ]
    );
}

#[test]
fn every_kind_of_clause_has_its_step() {
    let db = people();
    assert_eq!(
        plan(
            "EXPLAIN MATCH (a:Person {name: 'Ann'}) \
             OPTIONAL MATCH (a)-[r:KNOWS*1..2]->(b) \
             WITH DISTINCT a, b SKIP 1 \
             UNWIND [1, 2] AS n \
             MERGE (t:Tag {n: n}) \
             CREATE (a)-[:HAS]->(t) \
             SET t.seen = true \
             REMOVE t.old \
             DETACH DELETE b",
            &db
        ),
        [
            "AllNodesScan: (a:Person {name: 'Ann'}), kept by label",
            "Argument: (a)",
            "OptionalVarLengthExpand: (a)-[r:KNOWS*1..2]->(b)",
            "Projection: a, b",
            "Distinct",
            "Skip: 1",
            "Unwind: [1, 2] AS n",
            "Merge: (t:Tag {n: n})",
            "Create: (a)-[:HAS]->(t)",
            "SetProperty: 1 item(s)",
            "Remove: 1 item(s)",
            "DetachDelete: b",
        ]
    );
    assert_eq!(
        plan(
            "EXPLAIN MATCH p = shortestPath((a:Person)-[*]-(c:Company)) WHERE id(a) = 1 \
             CALL db.labels() YIELD label RETURN length(p), label",
            &db
        ),
        [
            "NodeByIdSeek: (a:Person)",
            "ShortestPath: (a)-[*]-(c:Company)",
            "Filter: id(a) = 1",
            "ProcedureCall: db.labels YIELD label",
            "Projection: length(p), label",
        ]
    );
}

#[test]
fn a_subquery_and_a_union_are_shown_with_their_steps() {
    let db = people();
    assert_eq!(
        plan(
            "EXPLAIN MATCH (p:Person) \
             CALL (p) { MATCH (p)-[:KNOWS]->(f) RETURN count(f) AS c } \
             RETURN p.name, c \
             UNION ALL MATCH (c:Company) RETURN c.name AS `p.name`, 0 AS c",
            &db
        ),
        [
            "AllNodesScan: (p:Person), kept by label",
            "Subquery: once for each row; imports p",
            "Argument:   (p)",
            "Expand:   (p)-[:KNOWS]->(f)",
            "Aggregation:   count(f) AS c",
            "Projection: p.name, c",
            "Union: ALL",
            "AllNodesScan: (c:Company), kept by label",
            "Projection: c.name AS p.name, 0 AS c",
        ]
    );
}

#[test]
fn explain_checks_the_query_it_does_not_run() {
    let db = people();
    for (query, part) in [
        ("EXPLAIN MATCH (p:Person) RETURN nope(p)", "nope"),
        (
            "EXPLAIN MATCH (p:Person) RETURN p.name ORDER BY",
            "expected expression",
        ),
        ("EXPLAIN CALL no.such.proc()", "no.such.proc"),
        (
            "EXPLAIN RETURN 1 UNION RETURN 2 UNION ALL RETURN 3",
            "UNION",
        ),
        ("EXPLAIN", "empty"),
        ("EXPLAIN EXPLAIN RETURN 1", "EXPLAIN"),
        ("MATCH (p) EXPLAIN RETURN p", "EXPLAIN"),
    ] {
        let err = try_run(query, &db).expect_err(query);
        assert!(err.contains(part), "`{query}`: {err}");
    }
    // A variable named `explain` or `profile` is still a variable.
    assert_eq!(
        run(
            "UNWIND [1] AS explain WITH explain AS profile RETURN profile",
            &db
        )
        .rows,
        vec![vec![i(1)]]
    );
}

#[test]
fn profile_reports_the_rows_and_the_time_of_each_clause() {
    let db = people();
    let got = run(
        "PROFILE MATCH (p:Person)-[:KNOWS]->(f) WHERE f.age > 26 \
         SET p.checked = true RETURN p.name AS name",
        &db,
    );
    assert_eq!(got.columns, ["step", "operator", "details", "rows", "ms"]);
    let measured: Vec<(String, Value)> = got
        .rows
        .iter()
        .map(|row| match &row[1] {
            Value::String(operator) => (operator.clone(), row[3].clone()),
            other => panic!("operator is text: {other:?}"),
        })
        .collect();
    assert_eq!(
        measured,
        [
            ("AllNodesScan".to_string(), Value::Null),
            ("Expand".to_string(), Value::Null),
            // Only Bob -> Cid passes the filter: one row leaves the MATCH.
            ("Filter".to_string(), i(1)),
            ("SetProperty".to_string(), i(1)),
            ("Projection".to_string(), i(1)),
        ]
    );
    for row in &got.rows {
        match (&row[3], &row[4]) {
            (Value::Null, Value::Null) => {}
            (Value::Integer(_), Value::Float(ms)) => assert!(*ms >= 0.0),
            other => panic!("rows and ms come together: {other:?}"),
        }
    }
    // The query ran: its write is there and counted.
    assert_eq!(got.stats.properties_set, 1);
    assert_eq!(
        run("MATCH (p:Person) WHERE p.checked RETURN p.name", &db).rows,
        vec![vec![s("Bob")]]
    );
}

#[test]
fn profile_runs_every_arm_of_a_union_and_fails_as_the_query_would() {
    let db = people();
    let got = run(
        "PROFILE MATCH (p:Person) RETURN p.name AS n UNION ALL MATCH (c:Company) RETURN c.name AS n",
        &db,
    );
    let produced: Vec<Value> = got.rows.iter().map(|row| row[3].clone()).collect();
    assert_eq!(
        produced,
        [i(3), i(3), Value::Null, i(1), i(1)],
        "{:?}",
        got.rows
    );
    let err = try_run("PROFILE MATCH (p:Person) RETURN q.name", &db).expect_err("`q`");
    assert!(err.contains("`q`"), "{err}");
}

mod through_the_service {
    use super::*;

    use drevo::cypher::executor::{read_access_refusal, Access};
    use drevo::native_service::NativeService;

    fn seeded() -> NativeService {
        let svc = NativeService::in_memory();
        svc.execute(
            &parse("CREATE (:Person {name: 'Ann', age: 30}), (:Person {name: 'Bob', age: 25})")
                .unwrap(),
            HashMap::new(),
        )
        .expect("seed");
        svc
    }

    fn operators(svc: &NativeService, query: &str) -> Vec<Value> {
        svc.execute(&parse(query).unwrap(), HashMap::new())
            .unwrap_or_else(|e| panic!("`{query}`: {e}"))
            .rows
            .into_iter()
            .map(|mut row| Value::List(vec![row.remove(1), row.remove(1)]))
            .collect()
    }

    #[test]
    fn the_plan_names_the_index_the_store_has() {
        let svc = seeded();
        let seek = |details: &str| Value::List(vec![s("NodeIndexSeek"), s(details)]);
        assert_eq!(
            operators(&svc, "EXPLAIN MATCH (p:Person {name: 'Ann'}) RETURN p")[0],
            seek("(p:Person {name: 'Ann'}) by name")
        );
        assert_eq!(
            operators(
                &svc,
                "EXPLAIN MATCH (p:Person) WHERE p.name = 'Ann' AND p.age > 1 RETURN p"
            )[0],
            seek("(p:Person) by age, name")
        );
        assert_eq!(
            operators(&svc, "EXPLAIN MATCH (p:Person) RETURN p")[0],
            Value::List(vec![s("NodeByLabelScan"), s("(p:Person)")])
        );
        assert_eq!(
            operators(&svc, "EXPLAIN MATCH (n) RETURN n")[0],
            Value::List(vec![s("AllNodesScan"), s("(n)")])
        );
    }

    #[test]
    fn explain_of_a_write_is_a_read_and_profile_of_a_write_is_a_write() {
        let svc = seeded();
        let explain = parse("EXPLAIN MATCH (p:Person) SET p.x = 1").unwrap();
        assert!(read_access_refusal(&explain).is_none());
        svc.execute_with_access(&explain, HashMap::new(), None, Access::Read)
            .expect("EXPLAIN runs nothing");
        let profile = parse("PROFILE MATCH (p:Person) SET p.x = 1").unwrap();
        assert!(read_access_refusal(&profile).is_some());
        let err = svc
            .execute_with_access(&profile, HashMap::new(), None, Access::Read)
            .expect_err("PROFILE runs the write");
        assert!(err.to_string().contains("READ"), "{err}");
        let count = parse("MATCH (p:Person) WHERE p.x = 1 RETURN count(p) AS c").unwrap();
        assert_eq!(
            svc.execute(&count, HashMap::new()).unwrap().rows,
            vec![vec![i(0)]]
        );
        // With write access PROFILE commits what it wrote.
        svc.execute(&profile, HashMap::new())
            .expect("PROFILE writes");
        assert_eq!(
            svc.execute(&count, HashMap::new()).unwrap().rows,
            vec![vec![i(2)]]
        );
    }

    #[test]
    fn explain_never_calls_a_procedure() {
        let svc = seeded();
        // The call would ask a model; under EXPLAIN it is only described.
        let got = operators(
            &svc,
            "EXPLAIN CALL drevo.extract.entities('some text') YIELD name RETURN name",
        );
        assert_eq!(
            got[0],
            Value::List(vec![
                s("ProcedureCall"),
                s("drevo.extract.entities YIELD name")
            ])
        );
    }
}

/// Every `cypher` example of the reference runs, so `EXPLAIN` must accept
/// each of them: its check of variables may miss an unbound one, never
/// refuse a bound one.
#[test]
fn explain_refuses_no_example_of_the_reference() {
    let reference = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/cypher-reference.md"
    ))
    .expect("docs/cypher-reference.md");
    let mut checked = 0;
    for block in reference.split("```cypher\n").skip(1) {
        let Some((source, _)) = block.split_once("```") else {
            continue;
        };
        let Ok(parsed) = parse(source) else {
            continue;
        };
        if let Some((name, _)) = drevo::cypher::scope::unbound_variable(&parsed) {
            panic!("`{name}` is reported unbound in an example that runs:\n{source}");
        }
        checked += 1;
    }
    assert!(
        checked > 100,
        "the reference has many examples, saw {checked}"
    );
}

#[test]
fn the_plan_shows_a_where_inside_a_pattern_and_temporal_expressions() {
    let db = people();
    assert_eq!(
        plan(
            "EXPLAIN MATCH (a:Person WHERE a.age > 28)-[r:KNOWS WHERE r.since > 2019]->(b) \
             WHERE date('2026-11-27') > date('2026-01-01') RETURN b.name",
            &db
        ),
        [
            "AllNodesScan: (a:Person WHERE a.age > 28), kept by label",
            "Expand: (a)-[r:KNOWS WHERE r.since > 2019]->(b)",
            "Filter: date('2026-11-27') > date('2026-01-01')",
            "Projection: b.name",
        ]
    );
    let err = try_run("EXPLAIN MATCH (a:Person WHERE b.age > 28) RETURN a", &db)
        .map(|got| format!("{:?}", got.rows));
    // The check does not look inside a pattern, so this one is only found
    // by a run; EXPLAIN describes it.
    assert!(err.is_ok(), "{err:?}");
}
