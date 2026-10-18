//! Sequences (`drevo.sequence.*`) in a bug tracker: human-readable numbers
//! such as `BUG-1042`, unique and increasing, handed out outside
//! transactions so concurrent writers never conflict over them.

use std::collections::HashMap;
use std::sync::Arc;

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

fn scalar(svc: &NativeService, q: &str) -> Value {
    ok(svc, q).rows.remove(0).remove(0)
}

fn int(svc: &NativeService, q: &str) -> i64 {
    match scalar(svc, q) {
        Value::Integer(n) => n,
        other => panic!("`{q}` gave {other:?}"),
    }
}

#[test]
fn bugs_get_consecutive_numbers() {
    let svc = NativeService::in_memory();
    let created = ok(
        &svc,
        "CALL drevo.sequence.create('bug', {start: 1000, step: 1}) \
         YIELD name, start, step, current RETURN name, start, step, current",
    );
    assert_eq!(
        created.rows,
        vec![vec![
            Value::String("bug".into()),
            Value::Integer(1000),
            Value::Integer(1),
            Value::Null,
        ]]
    );
    for title in ["crash on save", "slow search", "typo"] {
        ok(
            &svc,
            &format!(
                "CREATE (:Bug {{title: '{title}', key: 'BUG-' + toString(drevo.sequence.next('bug'))}})"
            ),
        );
    }
    let keys = ok(&svc, "MATCH (b:Bug) RETURN b.key ORDER BY b.key");
    assert_eq!(
        keys.rows,
        vec![
            vec![Value::String("BUG-1000".into())],
            vec![Value::String("BUG-1001".into())],
            vec![Value::String("BUG-1002".into())],
        ]
    );
    assert_eq!(int(&svc, "RETURN drevo.sequence.current('bug')"), 1002);
}

#[test]
fn defaults_count_from_one_and_a_step_can_be_negative() {
    let svc = NativeService::in_memory();
    ok(&svc, "CALL drevo.sequence.create('plain')");
    assert_eq!(
        scalar(&svc, "RETURN drevo.sequence.current('plain')"),
        Value::Null
    );
    assert_eq!(int(&svc, "RETURN drevo.sequence.next('plain')"), 1);
    ok(
        &svc,
        "CALL drevo.sequence.create('countdown', {start: 10, step: -2})",
    );
    let values = ok(
        &svc,
        "UNWIND range(1, 3) AS i RETURN drevo.sequence.next('countdown') AS v",
    );
    assert_eq!(
        values.rows,
        vec![
            vec![Value::Integer(10)],
            vec![Value::Integer(8)],
            vec![Value::Integer(6)],
        ]
    );
}

#[test]
fn a_rolled_back_transaction_leaves_a_gap() {
    let svc = NativeService::in_memory();
    ok(&svc, "CALL drevo.sequence.create('bug', {start: 1})");
    let tx = svc.begin_tx();
    svc.execute_in_tx(
        tx,
        &parse("CREATE (:Bug {n: drevo.sequence.next('bug')})").unwrap(),
        HashMap::new(),
    )
    .unwrap();
    svc.rollback_tx(tx);
    assert_eq!(int(&svc, "MATCH (b:Bug) RETURN count(b)"), 0);
    assert_eq!(int(&svc, "RETURN drevo.sequence.next('bug')"), 2);
}

#[test]
fn a_failed_statement_leaves_a_gap_too() {
    let svc = NativeService::in_memory();
    ok(&svc, "CALL drevo.sequence.create('bug')");
    assert!(run(&svc, "WITH drevo.sequence.next('bug') AS n RETURN n / 0").is_err());
    assert_eq!(int(&svc, "RETURN drevo.sequence.next('bug')"), 2);
}

#[test]
fn concurrent_writers_get_distinct_numbers_without_conflicts() {
    let svc = Arc::new(NativeService::in_memory());
    ok(&svc, "CALL drevo.sequence.create('bug', {start: 1})");
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let svc = Arc::clone(&svc);
            std::thread::spawn(move || {
                (0..250)
                    .map(|_| {
                        let r = svc
                            .execute(
                                &parse("CREATE (b:Bug {n: drevo.sequence.next('bug')}) RETURN b.n")
                                    .unwrap(),
                                HashMap::new(),
                            )
                            .expect("no commit conflicts");
                        match &r.rows[0][0] {
                            Value::Integer(n) => *n,
                            other => panic!("{other:?}"),
                        }
                    })
                    .collect::<Vec<i64>>()
            })
        })
        .collect();
    let mut all: Vec<i64> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all, (1..=2000).collect::<Vec<i64>>());
    assert_eq!(int(&svc, "MATCH (b:Bug) RETURN count(DISTINCT b.n)"), 2000);
}

#[test]
fn a_restart_continues_past_every_issued_number() {
    let dir = tempfile::tempdir().unwrap();
    let wal = dir.path().join("native.wal");
    let last = {
        let svc = NativeService::open(&wal).unwrap();
        ok(&svc, "CALL drevo.sequence.create('invoice', {start: 1})");
        ok(
            &svc,
            "UNWIND range(1, 5) AS i RETURN drevo.sequence.next('invoice')",
        );
        int(&svc, "RETURN drevo.sequence.current('invoice')")
    };
    let svc = NativeService::open(&wal).unwrap();
    let listed = ok(
        &svc,
        "CALL drevo.sequence.list() YIELD name, start, step RETURN name, start, step",
    );
    assert_eq!(
        listed.rows,
        vec![vec![
            Value::String("invoice".into()),
            Value::Integer(1),
            Value::Integer(1),
        ]]
    );
    assert!(int(&svc, "RETURN drevo.sequence.next('invoice')") > last);
}

#[test]
fn list_and_drop() {
    let svc = NativeService::in_memory();
    ok(&svc, "CALL drevo.sequence.create('b')");
    ok(&svc, "CALL drevo.sequence.create('a', {start: 5})");
    ok(&svc, "RETURN drevo.sequence.next('a')");
    let listed = ok(
        &svc,
        "CALL drevo.sequence.list() YIELD name, current RETURN name, current",
    );
    assert_eq!(
        listed.rows,
        vec![
            vec![Value::String("a".into()), Value::Integer(5)],
            vec![Value::String("b".into()), Value::Null],
        ]
    );
    let dropped = ok(&svc, "CALL drevo.sequence.drop('a') YIELD name RETURN name");
    assert_eq!(dropped.rows, vec![vec![Value::String("a".into())]]);
    assert!(ok(&svc, "CALL drevo.sequence.drop('a')").rows.is_empty());
    assert!(run(&svc, "RETURN drevo.sequence.next('a')").is_err());
}

#[test]
fn bad_calls_are_refused() {
    let svc = NativeService::in_memory();
    ok(&svc, "CALL drevo.sequence.create('bug')");
    for q in [
        "CALL drevo.sequence.create('bug')",
        "CALL drevo.sequence.create('zero', {step: 0})",
        "CALL drevo.sequence.create('', {})",
        "CALL drevo.sequence.create('x', {start: 'one'})",
        "CALL drevo.sequence.create('x', {cache: 10})",
        "CALL drevo.sequence.create(42)",
        "RETURN drevo.sequence.next('missing')",
        "RETURN drevo.sequence.current('missing')",
        "RETURN drevo.sequence.next(1)",
        "RETURN drevo.sequence.next()",
    ] {
        let err = run(&svc, q).expect_err(q);
        assert!(
            matches!(
                err,
                ExecError::InvalidProcedureCall { .. } | ExecError::InvalidFunctionCall { .. }
            ),
            "{q}: {err:?}"
        );
    }
    assert_eq!(
        scalar(&svc, "RETURN drevo.sequence.next(null)"),
        Value::Null
    );
}

#[test]
fn an_exhausted_sequence_says_so() {
    let svc = NativeService::in_memory();
    ok(
        &svc,
        "CALL drevo.sequence.create('top', {start: 9223372036854775807})",
    );
    ok(&svc, "RETURN drevo.sequence.next('top')");
    let err = run(&svc, "RETURN drevo.sequence.next('top')").unwrap_err();
    assert!(err.to_string().contains("exhausted"), "{err}");
}

#[test]
fn creating_and_dropping_are_refused_in_an_explicit_transaction() {
    let svc = NativeService::in_memory();
    ok(&svc, "CALL drevo.sequence.create('bug')");
    let tx = svc.begin_tx();
    let in_tx = |q: &str| svc.execute_in_tx(tx, &parse(q).unwrap(), HashMap::new());
    for q in [
        "CALL drevo.sequence.create('other')",
        "CALL drevo.sequence.drop('bug')",
    ] {
        assert!(
            matches!(in_tx(q), Err(ExecError::ForbiddenInTransaction { .. })),
            "{q}"
        );
    }
    // Drawing values and listing work in a transaction.
    assert!(in_tx("RETURN drevo.sequence.next('bug')").is_ok());
    assert!(in_tx("CALL drevo.sequence.list()").is_ok());
    svc.rollback_tx(tx);
}

#[test]
fn a_read_only_database_hands_out_nothing() {
    let svc = NativeService::in_memory();
    ok(&svc, "CALL drevo.sequence.create('bug')");
    svc.set_read_only(true).unwrap();
    for q in [
        "RETURN drevo.sequence.next('bug')",
        "CALL drevo.sequence.create('other')",
        "CALL drevo.sequence.drop('bug')",
    ] {
        let err = run(&svc, q).expect_err(q);
        assert!(
            matches!(err, ExecError::Storage(DrevoError::ReadOnly)),
            "{q}: {err:?}"
        );
    }
    assert_eq!(
        scalar(&svc, "RETURN drevo.sequence.current('bug')"),
        Value::Null
    );
    assert_eq!(ok(&svc, "CALL drevo.sequence.list()").rows.len(), 1);
}
