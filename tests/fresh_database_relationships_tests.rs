//! A fresh database that already holds many nodes keeps every relationship
//! its first relationship statement creates, whether the statement runs in
//! auto-commit mode or in an explicit transaction (a driver's
//! `execute_write`).
//!
//! The scenario is a road importer: 60,000 junctions in batches of 2,000,
//! then 2,000 roads between random junctions in one statement.

use std::collections::HashMap;

use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn rows(maps: Vec<HashMap<String, Value>>) -> Value {
    Value::List(
        maps.into_iter()
            .map(|m| Value::Map(m.into_iter().collect()))
            .collect(),
    )
}

fn run(svc: &NativeService, explicit: bool, q: &str, rows_param: Value) -> Vec<Vec<Value>> {
    let ast = parse(q).unwrap();
    let params = HashMap::from([("rows".to_string(), rows_param)]);
    if explicit {
        let tx = svc.begin_tx();
        let result = svc.execute_in_tx(tx, &ast, params).unwrap();
        svc.commit_tx(tx).unwrap();
        result.rows
    } else {
        svc.execute(&ast, params).unwrap().rows
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn import_roads(explicit: bool) {
    let svc = NativeService::in_memory();
    let mut ids = Vec::new();
    for start in (0..60_000).step_by(2_000) {
        let batch = (start..start + 2_000)
            .map(|i| HashMap::from([("n".to_string(), Value::Integer(i))]))
            .collect();
        let created = run(
            &svc,
            explicit,
            "UNWIND $rows AS r CREATE (j:Junction {n: r.n}) RETURN id(j) AS id",
            rows(batch),
        );
        ids.extend(created.iter().map(|row| int(&row[0])));
    }

    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut pick = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        Value::Integer(ids[(state % ids.len() as u64) as usize])
    };
    let roads = (0..2_000)
        .map(|_| HashMap::from([("src".to_string(), pick()), ("dst".to_string(), pick())]))
        .collect();
    let created = run(
        &svc,
        explicit,
        "UNWIND $rows AS r MATCH (a) WHERE id(a) = r.src WITH a, r MATCH (b) WHERE id(b) = r.dst \
         CREATE (a)-[x:ROAD {m: 5.3}]->(b) RETURN id(x) AS rid, id(a) AS a",
        rows(roads),
    );
    assert_eq!(created.len(), 2_000);

    let count = |q: &str, p: Value| int(&run(&svc, false, q, p)[0][0]);
    assert_eq!(
        count("MATCH ()-[x:ROAD]->() RETURN count(x)", Value::Null),
        2_000,
        "every created road is visible"
    );
    // Each one by id and from its start junction.
    for row in created.iter().take(200) {
        let probe = rows(vec![HashMap::from([
            ("rid".to_string(), row[0].clone()),
            ("a".to_string(), row[1].clone()),
        ])]);
        assert_eq!(
            count(
                "UNWIND $rows AS r MATCH ()-[x]->() WHERE id(x) = r.rid RETURN count(x)",
                probe.clone()
            ),
            1,
            "road {:?} by id",
            row[0]
        );
        assert_eq!(
            count(
                "UNWIND $rows AS r MATCH (a)-[x:ROAD]->() WHERE id(a) = r.a AND id(x) = r.rid \
                 RETURN count(x)",
                probe
            ),
            1,
            "road {:?} from its start",
            row[0]
        );
    }
}

#[test]
fn the_first_relationships_of_a_fresh_database_are_all_visible() {
    import_roads(false);
}

#[test]
fn the_same_in_an_explicit_transaction() {
    import_roads(true);
}
