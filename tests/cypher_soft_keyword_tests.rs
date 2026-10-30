//! Soft keywords as identifiers (issue #765). `STARTS`, `ENDS`, `CONTAINS`,
//! `IN`, `IS`, `ON`, `BY`, `ASC`, `DESC`, `AND`, `OR` and `XOR` only ever
//! appear between two operands or inside a clause, so a bare one where a
//! variable is expected is a variable, as in Neo4j: `collect(s) AS starts`,
//! `MATCH (ends:Stop)`, `UNWIND list AS in`. Each still works as a keyword
//! in its own position, and the written casing of the name is kept.

use std::collections::HashMap;

use drevo::cypher::ast::{Clause, ProjectionItem};
use drevo::cypher::executor::{ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

/// Every keyword that may name a variable.
const SOFT: [&str; 12] = [
    "starts", "ends", "contains", "in", "is", "on", "by", "asc", "desc", "and", "or", "xor",
];

fn run(svc: &NativeService, q: &str) -> ExecResult {
    svc.execute(
        &parse(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}")),
        HashMap::new(),
    )
    .unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

fn ints(r: &ExecResult) -> Vec<i64> {
    r.rows
        .iter()
        .map(|row| match &row[0] {
            Value::Integer(i) => *i,
            other => panic!("not an integer: {other:?}"),
        })
        .collect()
}

#[test]
fn every_soft_keyword_names_a_return_alias() {
    let svc = NativeService::in_memory();
    for word in SOFT {
        let r = run(&svc, &format!("RETURN 1 AS {word}"));
        assert_eq!(r.columns, vec![word.to_string()], "{word}");
        assert_eq!(ints(&r), vec![1], "{word}");
    }
}

#[test]
fn every_soft_keyword_names_a_with_variable() {
    let svc = NativeService::in_memory();
    for word in SOFT {
        let r = run(&svc, &format!("WITH 7 AS {word} RETURN {word} + 1 AS v"));
        assert_eq!(ints(&r), vec![8], "{word}");
    }
}

#[test]
fn every_soft_keyword_names_a_pattern_variable() {
    let svc = NativeService::in_memory();
    run(
        &svc,
        "CREATE (:Stop {name: 'Central'})-[:LINKS {m: 300}]->(:Stop {name: 'North'})",
    );
    for word in SOFT {
        let r = run(
            &svc,
            &format!("MATCH ({word}:Stop {{name: 'Central'}}) RETURN {word}.name AS n"),
        );
        assert_eq!(r.rows.len(), 1, "{word}");
        assert_eq!(r.rows[0][0], Value::String("Central".into()), "{word}");
        let r = run(
            &svc,
            &format!("MATCH (:Stop)-[{word}:LINKS]->(:Stop) RETURN {word}.m AS m"),
        );
        assert_eq!(ints(&r), vec![300], "{word}");
    }
}

#[test]
fn a_soft_keyword_names_a_path() {
    let svc = NativeService::in_memory();
    run(
        &svc,
        "CREATE (:Stop {name: 'A'})-[:LINKS]->(:Stop {name: 'B'})",
    );
    let r = run(
        &svc,
        "MATCH ends = (:Stop {name: 'A'})-->() RETURN length(ends) AS l",
    );
    assert_eq!(ints(&r), vec![1]);
}

#[test]
fn a_soft_keyword_names_an_unwind_variable_and_a_sort_key() {
    let svc = NativeService::in_memory();
    let r = run(&svc, "UNWIND [1, 3, 2] AS asc RETURN asc ORDER BY asc DESC");
    assert_eq!(r.columns, vec!["asc".to_string()]);
    assert_eq!(ints(&r), vec![3, 2, 1]);
    let r = run(
        &svc,
        "UNWIND [1, 3, 2] AS in RETURN in ORDER BY in ASC LIMIT 2",
    );
    assert_eq!(ints(&r), vec![1, 2]);
}

#[test]
fn soft_keywords_name_the_variables_of_list_expressions() {
    let svc = NativeService::in_memory();
    let r = run(
        &svc,
        "RETURN reduce(starts = 0, ends IN [1, 2, 3] | starts + ends) AS r",
    );
    assert_eq!(ints(&r), vec![6]);
    let r = run(&svc, "RETURN [in IN [1, 2, 3] WHERE in > 1 | in * 2] AS l");
    assert_eq!(
        r.rows[0][0],
        Value::List(vec![Value::Integer(4), Value::Integer(6)])
    );
    let r = run(
        &svc,
        "RETURN any(contains IN [1, 2] WHERE contains = 2) AS a",
    );
    assert_eq!(r.rows[0][0], Value::Bool(true));
}

#[test]
fn a_soft_keyword_names_a_foreach_variable() {
    let svc = NativeService::in_memory();
    run(&svc, "FOREACH (on IN [1, 2] | CREATE (:Made {k: on}))");
    let r = run(&svc, "MATCH (m:Made) RETURN m.k AS k ORDER BY k");
    assert_eq!(ints(&r), vec![1, 2]);
}

#[test]
fn a_soft_keyword_names_a_yield_alias() {
    let svc = NativeService::in_memory();
    let r = run(
        &svc,
        "CALL drevo.info() YIELD version AS starts RETURN starts",
    );
    assert_eq!(r.columns, vec!["starts".to_string()]);
    assert!(matches!(&r.rows[0][0], Value::String(_)));
}

#[test]
fn collected_starts_and_ends_as_in_the_timetable_benchmark() {
    let svc = NativeService::in_memory();
    run(
        &svc,
        "CREATE (:Stop {name: 'A'}), (:Stop {name: 'B'}), (:Stop {name: 'C'})",
    );
    let r = run(
        &svc,
        "MATCH (s:Stop) WHERE s.name <> 'C' WITH collect({node: s, cost: 0}) AS starts \
         MATCH (e:Stop {name: 'C'}) WITH starts, collect(e) AS ends \
         RETURN size(starts) AS ns, size(ends) AS ne",
    );
    assert_eq!(r.columns, vec!["ns".to_string(), "ne".to_string()]);
    assert_eq!(r.rows[0], vec![Value::Integer(2), Value::Integer(1)]);
}

#[test]
fn soft_keywords_keep_working_as_keywords() {
    let svc = NativeService::in_memory();
    let r = run(
        &svc,
        "WITH 'abc' AS starts, 'abc' AS ends, 'abc' AS contains \
         RETURN starts STARTS WITH 'a' AND ends ENDS WITH 'c' AND contains CONTAINS 'b' AS t, \
                2 IN [1, 2] AS i, null IS NULL AS n, true XOR false AS x, false OR true AS o",
    );
    assert_eq!(
        r.rows[0],
        vec![
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(true),
        ]
    );
    run(&svc, "CREATE (:Stop {name: 'A'})");
    let r = run(
        &svc,
        "MERGE (on:Stop {name: 'A'}) ON MATCH SET on.seen = 1 ON CREATE SET on.seen = 2 RETURN on.seen AS s",
    );
    assert_eq!(ints(&r), vec![1]);
}

#[test]
fn a_soft_keyword_alias_keeps_its_written_casing() {
    let q = parse("RETURN 1 AS Starts, 2 AS ENDS").expect("parse");
    let Clause::Return(ret) = &q.parts[0].query.clauses[0] else {
        panic!("not a RETURN");
    };
    let aliases: Vec<&str> = ret
        .items
        .iter()
        .map(|item| match item {
            ProjectionItem::Expression { alias, .. } => alias.as_deref().expect("alias"),
            ProjectionItem::Star => panic!("star"),
        })
        .collect();
    assert_eq!(aliases, vec!["Starts", "ENDS"]);
}

#[test]
fn map_keys_named_by_soft_keywords_still_work() {
    let svc = NativeService::in_memory();
    let r = run(
        &svc,
        "WITH {starts: 1, ends: 2} AS m RETURN m.starts + m.ends AS s",
    );
    assert_eq!(ints(&r), vec![3]);
}

#[test]
fn reserved_keywords_are_still_not_variables() {
    for word in [
        "match", "return", "null", "true", "end", "when", "exists", "not", "all",
    ] {
        let e = parse(&format!("RETURN 1 AS {word}")).expect_err(word);
        assert!(e.to_string().contains("expected identifier"), "{word}: {e}");
    }
}
