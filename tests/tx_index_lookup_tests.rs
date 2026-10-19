//! `MATCH` by an indexed property inside an explicit transaction (a driver's
//! `execute_write`) is served by the indexes, like in auto-commit mode, and
//! still sees exactly the transaction's own view: nodes it created, changed,
//! relabelled or deleted, before or within the statement.
//!
//! The scenario is a road importer: junctions by `title` and `osm` id, then
//! roads between them from `UNWIND` rows.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

const JUNCTIONS: i64 = 20_000;

fn junctions() -> NativeService {
    let svc = NativeService::in_memory();
    svc.execute(
        &parse(&format!(
            "UNWIND range(1, {JUNCTIONS}) AS i CREATE (:Junction {{title: 'j' + toString(i), osm: i}})"
        ))
        .unwrap(),
        HashMap::new(),
    )
    .unwrap();
    svc
}

fn rows(n: i64) -> Value {
    Value::List(
        (1..=n)
            .map(|i| {
                Value::Map(
                    [
                        ("a".to_string(), Value::String(format!("j{i}"))),
                        ("b".to_string(), Value::String(format!("j{}", i + 1000))),
                        ("oa".to_string(), Value::Integer(i)),
                        ("ob".to_string(), Value::Integer(i + 5000)),
                    ]
                    .into_iter()
                    .collect(),
                )
            })
            .collect(),
    )
}

fn in_tx(
    svc: &NativeService,
    statements: &[(&str, HashMap<String, Value>)],
) -> Vec<Vec<Vec<Value>>> {
    let tx = svc.begin_tx();
    let out = statements
        .iter()
        .map(|(q, p)| {
            svc.execute_in_tx(tx, &parse(q).unwrap(), p.clone())
                .unwrap()
                .rows
        })
        .collect();
    svc.commit_tx(tx).unwrap();
    out
}

fn one(svc: &NativeService, q: &str) -> Value {
    svc.execute(&parse(q).unwrap(), HashMap::new())
        .unwrap()
        .rows[0][0]
        .clone()
}

#[test]
fn unwind_rows_match_by_index_inside_a_transaction() {
    let svc = junctions();
    let params = HashMap::from([("rows".to_string(), rows(300))]);
    let started = Instant::now();
    let created = in_tx(
        &svc,
        &[
            (
                "UNWIND $rows AS r MATCH (a:Junction {title: r.a}), (b:Junction {title: r.b}) \
                 CREATE (a)-[:ROAD]->(b) RETURN count(*)",
                params.clone(),
            ),
            (
                "UNWIND $rows AS r MATCH (a:Junction {osm: r.oa}) WITH a, r \
                 MATCH (b:Junction {osm: r.ob}) CREATE (a)-[:ROAD]->(b) RETURN count(*)",
                params,
            ),
        ],
    );
    let took = started.elapsed();
    assert_eq!(created[0][0][0], Value::Integer(300));
    assert_eq!(created[1][0][0], Value::Integer(300));
    assert_eq!(
        one(&svc, "MATCH ()-[r:ROAD]->() RETURN count(r)"),
        Value::Integer(600)
    );
    // A scan per row would read 600 × 20,000 nodes (tens of seconds here).
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

#[test]
fn the_transactions_own_writes_are_found() {
    let svc = junctions();
    let none = HashMap::new;
    let out = in_tx(
        &svc,
        &[
            ("CREATE (:Junction {title: 'new', osm: -1})", none()),
            (
                "MATCH (j:Junction {title: 'j7'}) SET j.title = 'seven', j.osm = -7",
                none(),
            ),
            ("MATCH (j:Junction {title: 'j8'}) DETACH DELETE j", none()),
            ("MATCH (j:Junction {title: 'j9'}) SET j:Closed", none()),
            ("MATCH (j:Junction {title: 'new'}) RETURN j.osm", none()),
            ("MATCH (j:Junction {osm: -1}) RETURN j.title", none()),
            ("MATCH (j:Junction {title: 'seven'}) RETURN j.osm", none()),
            ("MATCH (j:Junction {title: 'j7'}) RETURN count(j)", none()),
            ("MATCH (j:Junction {osm: 7}) RETURN count(j)", none()),
            ("MATCH (j:Junction {title: 'j8'}) RETURN count(j)", none()),
            ("MATCH (j:Closed) RETURN j.title", none()),
            ("MATCH (j:Closed) RETURN count(j)", none()),
            ("MATCH (j:Junction) RETURN count(j)", none()),
        ],
    );
    assert_eq!(out[4], vec![vec![Value::Integer(-1)]]);
    assert_eq!(out[5], vec![vec![Value::String("new".into())]]);
    assert_eq!(out[6], vec![vec![Value::Integer(-7)]]);
    assert_eq!(
        out[7],
        vec![vec![Value::Integer(0)]],
        "the old title is gone"
    );
    assert_eq!(out[8], vec![vec![Value::Integer(0)]], "the old osm is gone");
    assert_eq!(out[9], vec![vec![Value::Integer(0)]], "deleted");
    assert_eq!(out[10], vec![vec![Value::String("j9".into())]]);
    assert_eq!(out[11], vec![vec![Value::Integer(1)]]);
    assert_eq!(out[12], vec![vec![Value::Integer(JUNCTIONS)]]);
}

#[test]
fn a_commit_by_another_writer_falls_back_to_the_snapshot() {
    let svc = junctions();
    let tx = svc.begin_tx();
    // Another writer renames j5 and creates a node after the transaction began.
    svc.execute(
        &parse("MATCH (j:Junction {title: 'j5'}) SET j.title = 'five'").unwrap(),
        HashMap::new(),
    )
    .unwrap();
    svc.execute(
        &parse("CREATE (:Junction {title: 'late'})").unwrap(),
        HashMap::new(),
    )
    .unwrap();
    let q = |q: &str| {
        svc.execute_in_tx(tx, &parse(q).unwrap(), HashMap::new())
            .unwrap()
            .rows
    };
    // The transaction still sees its snapshot.
    assert_eq!(
        q("MATCH (j:Junction {title: 'j5'}) RETURN count(j)"),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        q("MATCH (j:Junction {title: 'five'}) RETURN count(j)"),
        vec![vec![Value::Integer(0)]]
    );
    assert_eq!(
        q("MATCH (j:Junction {title: 'late'}) RETURN count(j)"),
        vec![vec![Value::Integer(0)]]
    );
    svc.rollback_tx(tx);
}
