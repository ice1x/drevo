//! A lean database: `drevo.indexes.setSecondary(false)` drops the property
//! index, the value cache and the full-text indexes to save memory — for a
//! road graph that is only routed over — and `setSecondary(true)` brings them
//! back. Queries stay correct either way.

use std::collections::HashMap;

use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::error::DrevoError;
use drevo::native_service::NativeService;

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
}

fn ok(svc: &NativeService, q: &str) -> ExecResult {
    run(svc, q).unwrap_or_else(|e| panic!("`{q}`: {e}"))
}

fn titles(svc: &NativeService, q: &str) -> Vec<String> {
    ok(svc, q)
        .rows
        .into_iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn enabled(svc: &NativeService) -> Value {
    ok(
        svc,
        "CALL drevo.indexes.secondary() YIELD enabled RETURN enabled",
    )
    .rows
    .remove(0)
    .remove(0)
}

fn seed(svc: &NativeService) {
    ok(
        svc,
        "UNWIND range(1, 20) AS i \
         CREATE (:Junction {title: 'junction ' + toString(i), osm_id: i, lat: 42.6 + i / 1000.0})",
    );
}

#[test]
fn secondary_indexes_are_on_by_default() {
    let svc = NativeService::in_memory();
    assert!(svc.secondary_indexes());
    assert_eq!(enabled(&svc), Value::Bool(true));
}

#[test]
fn queries_stay_correct_without_them() {
    let svc = NativeService::in_memory();
    seed(&svc);
    let off = ok(
        &svc,
        "CALL drevo.indexes.setSecondary(false) YIELD enabled RETURN enabled",
    );
    assert_eq!(off.rows, vec![vec![Value::Bool(false)]]);
    assert!(!svc.secondary_indexes());

    assert_eq!(
        titles(&svc, "MATCH (j:Junction {osm_id: 7}) RETURN j.title"),
        vec!["junction 7"]
    );
    assert_eq!(
        titles(
            &svc,
            "MATCH (j:Junction) WHERE j.osm_id >= 19 RETURN j.title ORDER BY j.title"
        ),
        vec!["junction 19", "junction 20"]
    );
    // Writes while off are found by property too.
    ok(&svc, "CREATE (:Junction {title: 'late', osm_id: 99})");
    assert_eq!(
        titles(&svc, "MATCH (j {osm_id: 99}) RETURN j.title"),
        vec!["late"]
    );
    // Full-text search has no scan to fall back to.
    let err = run(
        &svc,
        "CALL fts.search('junction', 5) YIELD node RETURN node",
    )
    .unwrap_err();
    assert!(matches!(err, ExecError::EngineCapability { .. }), "{err:?}");
}

#[test]
fn turning_them_back_on_rebuilds_from_the_graph() {
    let svc = NativeService::in_memory();
    seed(&svc);
    ok(&svc, "CALL drevo.indexes.setSecondary(false)");
    ok(
        &svc,
        "CREATE (:Junction {title: 'written while off', osm_id: 500})",
    );
    ok(&svc, "CALL drevo.indexes.setSecondary(true)");
    assert_eq!(enabled(&svc), Value::Bool(true));
    let hits = titles(
        &svc,
        "CALL fts.search('written while off', 3) YIELD node RETURN node.title",
    );
    assert_eq!(hits.first().map(String::as_str), Some("written while off"));
    assert_eq!(
        titles(&svc, "MATCH (j:Junction {osm_id: 3}) RETURN j.title"),
        vec!["junction 3"]
    );
}

#[test]
fn the_choice_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let wal = dir.path().join("native.wal");
    {
        let svc = NativeService::open(&wal).unwrap();
        seed(&svc);
        ok(&svc, "CALL drevo.indexes.setSecondary(false)");
    }
    let svc = NativeService::open(&wal).unwrap();
    assert!(!svc.secondary_indexes());
    assert_eq!(
        titles(&svc, "MATCH (j:Junction {osm_id: 11}) RETURN j.title"),
        vec!["junction 11"]
    );
    ok(&svc, "CALL drevo.indexes.setSecondary(true)");
    drop(svc);
    assert!(NativeService::open(&wal).unwrap().secondary_indexes());
}

#[test]
fn changing_them_is_refused_in_a_transaction_and_on_a_read_only_database() {
    let svc = NativeService::in_memory();
    let tx = svc.begin_tx();
    let err = svc
        .execute_in_tx(
            tx,
            &parse("CALL drevo.indexes.setSecondary(false)").unwrap(),
            HashMap::new(),
        )
        .unwrap_err();
    assert!(
        matches!(err, ExecError::ForbiddenInTransaction { .. }),
        "{err:?}"
    );
    svc.rollback_tx(tx);

    svc.set_read_only(true).unwrap();
    let err = run(&svc, "CALL drevo.indexes.setSecondary(false)").unwrap_err();
    assert!(
        matches!(err, ExecError::Storage(DrevoError::ReadOnly)),
        "{err:?}"
    );
    assert_eq!(enabled(&svc), Value::Bool(true));

    let bad = run(
        &NativeService::in_memory(),
        "CALL drevo.indexes.setSecondary('no')",
    );
    assert!(matches!(bad, Err(ExecError::InvalidProcedureCall { .. })));
}
