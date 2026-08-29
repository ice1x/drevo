//! Guards for the durable-native serving layer (`NativeService`, RFC #307
//! Phase 4/7): the store of record IS the WAL-backed native graph, and the
//! full index stack — label, property, value cache, full-text — serves
//! Cypher, staying correct across writes, restarts, and concurrency.

use std::collections::HashMap;
use std::sync::Arc;

use drevo::cypher::executor::{ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run(service: &NativeService, source: &str) -> ExecResult {
    let q = parse(source).expect("parse");
    service
        .execute(&q, HashMap::new())
        .unwrap_or_else(|e| panic!("`{source}`: {e}"))
}

fn int(result: &ExecResult) -> i64 {
    match result.rows[0].as_slice() {
        [Value::Integer(n)] => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn strings(result: &ExecResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected string, got {other:?}"),
        })
        .collect()
}

#[test]
fn writes_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.wal");
    {
        let service = NativeService::open(&path).expect("open");
        run(&service, "CREATE (:Person {title: 'ada', age: 36})");
        run(
            &service,
            "CREATE (:Person {title: 'bob', age: 25})-[:KNOWS]->(:City {title: 'paris'})",
        );
    }
    let service = NativeService::open(&path).expect("reopen");
    assert_eq!(int(&run(&service, "MATCH (n) RETURN count(*)")), 3);
    assert_eq!(
        strings(&run(
            &service,
            "MATCH (a)-[:KNOWS]->(b) RETURN b.title ORDER BY b.title"
        )),
        ["paris"]
    );
}

#[test]
fn reads_after_writes_see_fresh_indexes() {
    let service = NativeService::in_memory();
    run(&service, "CREATE (:Person {title: 'ada', team: 'core'})");
    // Property-indexed read right after the write (the re-sync path).
    assert_eq!(
        int(&run(&service, "MATCH (n {team: 'core'}) RETURN count(*)")),
        1
    );
    run(&service, "MATCH (n {title: 'ada'}) SET n.team = 'infra'");
    assert_eq!(
        int(&run(&service, "MATCH (n {team: 'infra'}) RETURN count(*)")),
        1
    );
    assert_eq!(
        int(&run(&service, "MATCH (n {team: 'core'}) RETURN count(*)")),
        0
    );
}

#[test]
fn mixed_write_then_read_statement_sees_its_own_writes() {
    let service = NativeService::in_memory();
    let r = run(
        &service,
        "CREATE (:W {k: 7}) WITH 1 AS one MATCH (m {k: 7}) RETURN count(*)",
    );
    assert_eq!(int(&r), 1);
}

#[test]
fn full_text_search_is_served_natively() {
    let service = NativeService::in_memory();
    run(
        &service,
        "CREATE (:Doc {title: 'rust-notes', body: 'ownership and borrowing in rust'})",
    );
    run(
        &service,
        "CREATE (:Doc {title: 'cooking', body: 'how to bake bread'})",
    );
    let hits = run(&service, "CALL fts.search('borrowing', 5)");
    assert_eq!(hits.rows.len(), 1, "one document mentions borrowing");
    let hits = run(&service, "CALL fts.search('bread', 5)");
    assert_eq!(hits.rows.len(), 1);
}

#[test]
fn count_pushdown_and_engine_reads_work_through_the_service() {
    let service = NativeService::in_memory();
    run(
        &service,
        "CREATE (:A {title: 'x'}), (:A {title: 'y'}), (:B {title: 'z'})",
    );
    assert_eq!(int(&run(&service, "MATCH (n) RETURN count(*)")), 3);
    assert_eq!(int(&run(&service, "MATCH (n:A) RETURN count(*)")), 2);
}

#[test]
fn kv_only_procedures_surface_the_capability_error() {
    let service = NativeService::in_memory();
    let q = parse("CALL drevo.semantic.status()").expect("parse");
    let err = service
        .execute(&q, HashMap::new())
        .expect_err("semantic procedures need the KV secondary store");
    assert!(
        err.to_string()
            .contains("not available on the active graph engine"),
        "unexpected error: {err}"
    );
}

#[test]
fn reopen_compacts_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.wal");
    {
        let service = NativeService::open(&path).expect("open");
        run(&service, "CREATE (:Counter {title: 'c', v: 0})");
        for i in 1..=20 {
            run(&service, &format!("MATCH (n:Counter) SET n.v = {i}"));
        }
    }
    let history_lines = std::fs::read_to_string(&path).unwrap().lines().count();
    assert!(history_lines >= 21, "history accumulates: {history_lines}");
    {
        let service = NativeService::open(&path).expect("reopen compacts");
        assert_eq!(int(&run(&service, "MATCH (n) RETURN count(*)")), 1);
    }
    let compacted_lines = std::fs::read_to_string(&path).unwrap().lines().count();
    assert_eq!(
        compacted_lines, 1,
        "the reopened log holds state, not history"
    );
    // And the compacted log still recovers correctly.
    let service = NativeService::open(&path).expect("open after compact");
    let v = run(&service, "MATCH (n:Counter) RETURN n.v");
    assert_eq!(v.rows, vec![vec![Value::Integer(20)]]);
}

#[test]
fn concurrent_readers_and_writer_stay_consistent() {
    let service = Arc::new(NativeService::in_memory());
    run(&service, "CREATE (:Seed {title: 's0', k: 0})");

    let writer = {
        let service = Arc::clone(&service);
        std::thread::spawn(move || {
            for i in 1..=50 {
                let q = parse(&format!("CREATE (:Seed {{title: 's{i}', k: {i}}})")).unwrap();
                service.execute(&q, HashMap::new()).expect("write");
            }
        })
    };
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let service = Arc::clone(&service);
            std::thread::spawn(move || {
                let q = parse("MATCH (n:Seed) RETURN count(*)").unwrap();
                for _ in 0..100 {
                    let r = service.execute(&q, HashMap::new()).expect("read");
                    let n = match r.rows[0].as_slice() {
                        [Value::Integer(n)] => *n,
                        other => panic!("unexpected {other:?}"),
                    };
                    assert!((1..=51).contains(&n), "count out of range: {n}");
                }
            })
        })
        .collect();
    writer.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }
    assert_eq!(int(&run(&service, "MATCH (n:Seed) RETURN count(*)")), 51);
}
