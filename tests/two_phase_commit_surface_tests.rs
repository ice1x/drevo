//! Two-phase commit surfaces — #556 slice 2.
//!
//! The engine half (slice 1, `drevo-core/tests/two_phase_commit.rs`) is
//! exposed through the serving layer:
//! - `NativeService::{prepare_tx, commit_prepared, rollback_prepared,
//!   list_prepared}` for embedders and drevo-py;
//! - Cypher: `CALL drevo.tx.listPrepared()`, `drevo.tx.commitPrepared($gid)`,
//!   `drevo.tx.rollbackPrepared($gid)` (any autocommit statement, Bolt or
//!   HTTP), and `drevo.tx.prepare($gid)` inside an explicit Bolt transaction;
//! - while a transaction is prepared, other writes fail with the structured,
//!   retryable `DrevoError::PreparedTransactionPending` (HTTP 503, Bolt
//!   `Neo.TransientError.Transaction.LockAcquisitionTimeout`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::error::DrevoError;
use drevo::native_service::NativeService;

fn exec(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
}

fn ok(svc: &NativeService, q: &str) -> ExecResult {
    exec(svc, q).unwrap_or_else(|e| panic!("`{q}`: {e:?}"))
}

fn count(svc: &NativeService, label: &str) -> i64 {
    match ok(svc, &format!("MATCH (n:{label}) RETURN count(n) AS c")).rows[0][0] {
        Value::Integer(n) => n,
        ref v => panic!("{v:?}"),
    }
}

/// A transaction that creates `n` nodes of `label`, then prepared as `gid`.
fn prepare_creating(svc: &NativeService, label: &str, n: usize, gid: &str) {
    let tx = svc.begin_tx();
    for _ in 0..n {
        svc.execute_in_tx(
            tx,
            &parse(&format!("CREATE (:{label})")).unwrap(),
            HashMap::new(),
        )
        .expect("write in tx");
    }
    svc.prepare_tx(tx, gid).expect("prepare");
}

fn is_pending(e: &ExecError) -> bool {
    matches!(
        e,
        ExecError::Storage(DrevoError::PreparedTransactionPending(_))
    )
}

// ── NativeService ────────────────────────────────────────────────────────

#[test]
fn service_prepare_commit_and_rollback() {
    let svc = NativeService::in_memory();
    prepare_creating(&svc, "T", 2, "g1");
    assert_eq!(count(&svc, "T"), 0, "prepared writes are invisible");
    assert_eq!(svc.list_prepared()[0].gid, "g1");
    svc.commit_prepared("g1").unwrap();
    assert_eq!(count(&svc, "T"), 2, "indexes see the committed write set");

    prepare_creating(&svc, "U", 1, "g2");
    svc.rollback_prepared("g2").unwrap();
    assert_eq!(count(&svc, "U"), 0);
    assert!(svc.list_prepared().is_empty());
}

#[test]
fn writes_while_prepared_are_a_structured_retryable_error() {
    let svc = NativeService::in_memory();
    prepare_creating(&svc, "T", 1, "g1");
    let err = exec(&svc, "CREATE (:X)").expect_err("fenced");
    assert!(is_pending(&err), "{err:?}");
    match err {
        ExecError::Storage(DrevoError::PreparedTransactionPending(gids)) => {
            assert_eq!(gids, vec!["g1"]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(count(&svc, "X"), 0, "reads still work");
    svc.commit_prepared("g1").unwrap();
    ok(&svc, "CREATE (:X)");
}

// ── Cypher procedures ───────────────────────────────────────────────────

#[test]
fn cypher_lists_and_commits_prepared_transactions() {
    let svc = NativeService::in_memory();
    prepare_creating(&svc, "T", 3, "coord-7");

    let listed = ok(
        &svc,
        "CALL drevo.tx.listPrepared() YIELD gid, preparedAt, opCount RETURN gid, opCount",
    );
    assert_eq!(
        listed.rows,
        vec![vec![Value::String("coord-7".into()), Value::Integer(3)]]
    );

    let committed = ok(
        &svc,
        "CALL drevo.tx.commitPrepared('coord-7') YIELD gid RETURN gid",
    );
    assert_eq!(committed.rows, vec![vec![Value::String("coord-7".into())]]);
    assert_eq!(count(&svc, "T"), 3);
    assert!(ok(&svc, "CALL drevo.tx.listPrepared()").rows.is_empty());
}

#[test]
fn cypher_rolls_back_with_a_parameter() {
    let svc = NativeService::in_memory();
    prepare_creating(&svc, "T", 1, "coord-8");
    let mut params = HashMap::new();
    params.insert("gid".to_string(), Value::String("coord-8".into()));
    svc.execute(
        &parse("CALL drevo.tx.rollbackPrepared($gid)").unwrap(),
        params,
    )
    .expect("rollback");
    assert_eq!(count(&svc, "T"), 0);
    ok(&svc, "CREATE (:T)");
}

#[test]
fn cypher_resolving_an_unknown_gid_fails_cleanly() {
    let svc = NativeService::in_memory();
    for q in [
        "CALL drevo.tx.commitPrepared('nope')",
        "CALL drevo.tx.rollbackPrepared('nope')",
    ] {
        let err = exec(&svc, q).expect_err("unknown gid");
        assert!(
            matches!(err, ExecError::InvalidProcedureCall { .. }),
            "{q}: {err:?}"
        );
        assert!(err.to_string().contains("nope"), "{err}");
    }
}

#[test]
fn cypher_prepare_outside_a_transaction_explains_itself() {
    let svc = NativeService::in_memory();
    let err = exec(&svc, "CALL drevo.tx.prepare('g')").expect_err("needs a tx");
    assert!(err.to_string().contains("explicit transaction"), "{err}");
}

// ── Bolt ────────────────────────────────────────────────────────────────

fn hello(svc: &Arc<NativeService>) -> Session<'static> {
    let mut s = Session::new_durable(Arc::clone(svc));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    s
}

fn run(s: &mut Session<'_>, q: &str) -> Vec<ServerMessage> {
    let mut out = s.handle(ClientMessage::Run {
        query: q.to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    if matches!(out.first(), Some(ServerMessage::Success { .. })) {
        let mut n = BTreeMap::new();
        n.insert("n".to_string(), Bolt::Integer(-1));
        out.extend(s.handle(ClientMessage::Pull { extra: n }));
    }
    out
}

fn failure_code(replies: &[ServerMessage]) -> Option<String> {
    replies.iter().find_map(|m| match m {
        ServerMessage::Failure { metadata } => match metadata.get("code") {
            Some(Bolt::String(c)) => Some(c.clone()),
            _ => Some(String::new()),
        },
        _ => None,
    })
}

fn records(replies: &[ServerMessage]) -> Vec<Vec<Bolt>> {
    replies
        .iter()
        .filter_map(|m| match m {
            ServerMessage::Record { fields } => Some(fields.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn bolt_prepare_inside_an_explicit_transaction_then_resolve() {
    let svc = Arc::new(NativeService::in_memory());
    let mut s = hello(&svc);
    assert!(matches!(
        s.handle(ClientMessage::Begin {
            extra: BTreeMap::new()
        })[0],
        ServerMessage::Success { .. }
    ));
    assert_eq!(failure_code(&run(&mut s, "CREATE (:T {v: 1})")), None);

    let prepared = run(&mut s, "CALL drevo.tx.prepare('bolt-1')");
    assert_eq!(failure_code(&prepared), None, "{prepared:?}");
    assert_eq!(
        records(&prepared),
        vec![vec![Bolt::String("bolt-1".into())]]
    );

    // The driver's automatic COMMIT on leaving the transaction is a no-op.
    assert_eq!(failure_code(&s.handle(ClientMessage::Commit)), None);
    assert_eq!(count(&svc, "T"), 0, "still only prepared");

    // Another writer on another connection is told to back off.
    let mut other = hello(&svc);
    assert_eq!(
        failure_code(&run(&mut other, "CREATE (:X)")).as_deref(),
        Some("Neo.TransientError.Transaction.LockAcquisitionTimeout")
    );

    // The coordinator resolves from any session.
    assert!(matches!(
        other.handle(ClientMessage::Reset)[0],
        ServerMessage::Success { .. }
    ));
    assert_eq!(
        failure_code(&run(&mut other, "CALL drevo.tx.commitPrepared('bolt-1')")),
        None
    );
    assert_eq!(count(&svc, "T"), 1);
    assert_eq!(failure_code(&run(&mut other, "CREATE (:X)")), None);
}

#[test]
fn bolt_prepare_failure_closes_the_transaction() {
    let svc = Arc::new(NativeService::in_memory());
    let mut s = hello(&svc);
    s.handle(ClientMessage::Begin {
        extra: BTreeMap::new(),
    });
    run(&mut s, "CREATE (:T)");
    ok(&svc, "CREATE (:Concurrent)"); // the graph moved since BEGIN
    let replies = run(&mut s, "CALL drevo.tx.prepare('late')");
    assert_eq!(
        failure_code(&replies).as_deref(),
        Some("Neo.TransientError.Transaction.Outdated")
    );
    assert!(svc.list_prepared().is_empty());
    ok(&svc, "CREATE (:Free)"); // no fence was raised
}

// ── Heuristic rollback (#556 slice 4) ───────────────────────────────────

#[test]
fn heuristic_rollback_from_cypher_is_reported_to_a_late_commit() {
    let svc = NativeService::in_memory();
    prepare_creating(&svc, "T", 1, "abandoned");
    let rolled = ok(
        &svc,
        "CALL drevo.tx.heuristicRollback('abandoned') YIELD gid RETURN gid",
    );
    assert_eq!(rolled.rows, vec![vec![Value::String("abandoned".into())]]);
    ok(&svc, "CREATE (:Free)"); // fence lifted
    let err = exec(&svc, "CALL drevo.tx.commitPrepared('abandoned')").expect_err("overridden");
    assert!(err.to_string().contains("heuristically"), "{err}");
}

#[test]
fn expired_prepared_transactions_are_rolled_back_heuristically() {
    let svc = NativeService::in_memory();
    prepare_creating(&svc, "T", 1, "old");
    let now = drevo::model::now_ms();
    // Not yet past the timeout: nothing happens.
    let none = drevo::problems::heuristic_rollback_expired(
        "drevo",
        &svc,
        now,
        std::time::Duration::from_secs(3600),
    );
    assert!(none.is_empty());
    assert_eq!(svc.list_prepared().len(), 1);
    // Past it: rolled back heuristically, and a late commit is told so.
    let rolled = drevo::problems::heuristic_rollback_expired(
        "drevo",
        &svc,
        now + 7_200_000,
        std::time::Duration::from_secs(3600),
    );
    assert_eq!(rolled, vec!["old".to_string()]);
    assert!(svc.list_prepared().is_empty());
    assert!(matches!(
        svc.commit_prepared("old"),
        Err(drevo::native::ResolveError::HeuristicRollback(_))
    ));
}

/// The fence's Bolt code must be one official drivers actually *retry*. The
/// Neo4j drivers reclassify `Neo.TransientError.Transaction.Terminated` and
/// `…LockClientStopped` as client errors (they mean "stopped by the user"),
/// so either would make a fenced write fail instead of backing off — found on
/// the live server with the Python driver (#556).
#[test]
fn the_fence_bolt_code_is_retryable_by_official_drivers() {
    let svc = Arc::new(NativeService::in_memory());
    prepare_creating(&svc, "T", 1, "g");
    let mut s = hello(&svc);
    let code = failure_code(&run(&mut s, "CREATE (:X)")).expect("fenced");
    assert!(code.starts_with("Neo.TransientError."), "{code}");
    for remapped in [
        "Neo.TransientError.Transaction.Terminated",
        "Neo.TransientError.Transaction.LockClientStopped",
    ] {
        assert_ne!(
            code, remapped,
            "drivers treat {remapped} as a non-retryable client error"
        );
    }
}
