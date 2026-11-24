//! Two-phase commit and writers on other records (#828).
//!
//! A one-phase commit conflicts only with a commit to the same node or
//! relationship. Phase one of a two-phase commit follows the same rule: a
//! commit elsewhere in the graph does not make `drevo.tx.prepare` fail, and a
//! prepared transaction holds back only the transactions that overlap what
//! it wrote — another transaction on other records can be prepared too.
//!
//! The four cases are the table of the issue, over Bolt as a driver runs
//! them, and through `NativeService` as drevo-py does.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native::PrepareError;
use drevo::native_service::NativeService;

fn ok(svc: &NativeService, q: &str) -> ExecResult {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
        .unwrap_or_else(|e| panic!("`{q}`: {e:?}"))
}

fn count(svc: &NativeService, label: &str) -> i64 {
    match ok(svc, &format!("MATCH (n:{label}) RETURN count(n)")).rows[0][0] {
        Value::Integer(n) => n,
        ref other => panic!("expected a count, got {other:?}"),
    }
}

// ── Bolt ─────────────────────────────────────────────────────────────────

const OUTDATED: &str = "Neo.TransientError.Transaction.Outdated";
const LOCK_TIMEOUT: &str = "Neo.TransientError.Transaction.LockAcquisitionTimeout";

fn hello(svc: &Arc<NativeService>) -> Session<'static> {
    let mut s = Session::new_durable(Arc::clone(svc));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    s
}

fn begin(s: &mut Session<'_>) {
    assert!(matches!(
        s.handle(ClientMessage::Begin {
            extra: BTreeMap::new()
        })[0],
        ServerMessage::Success { .. }
    ));
}

/// Run `q` and pull its records; the failure code, if it failed.
fn run(s: &mut Session<'_>, q: &str) -> Option<String> {
    let mut out = s.handle(ClientMessage::Run {
        query: q.to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    if matches!(out.first(), Some(ServerMessage::Success { .. })) {
        let mut all = BTreeMap::new();
        all.insert("n".to_string(), Bolt::Integer(-1));
        out.extend(s.handle(ClientMessage::Pull { extra: all }));
    }
    let code = out.iter().find_map(|m| match m {
        ServerMessage::Failure { metadata } => match metadata.get("code") {
            Some(Bolt::String(code)) => Some(code.clone()),
            _ => Some(String::new()),
        },
        _ => None,
    });
    if code.is_some() {
        s.handle(ClientMessage::Reset);
    }
    code
}

/// What the driver's `tx.commit()` sends after `drevo.tx.prepare`.
fn commit(s: &mut Session<'_>) {
    s.handle(ClientMessage::Commit);
}

#[test]
fn bolt_a_commit_elsewhere_does_not_outdate_a_prepare() {
    let svc = Arc::new(NativeService::in_memory());
    let mut a = hello(&svc);
    let mut b = hello(&svc);

    // Row 1 of the table: A commits after B's autocommit — fine today.
    begin(&mut a);
    assert_eq!(run(&mut a, "CREATE (:ReproA)"), None);
    assert_eq!(run(&mut b, "CREATE (:ReproB)"), None);
    commit(&mut a);
    assert_eq!(count(&svc, "ReproA"), 1);

    // Row 2: the same, with a prepare instead of the commit.
    begin(&mut a);
    assert_eq!(run(&mut a, "CREATE (:ReproA)"), None);
    assert_eq!(run(&mut b, "CREATE (:ReproB)"), None);
    let prepared = run(&mut a, "CALL drevo.tx.prepare('row2')");
    assert_ne!(prepared.as_deref(), Some(OUTDATED), "disjoint writes");
    assert_eq!(prepared, None);
    commit(&mut a);
    assert_eq!(count(&svc, "ReproA"), 1, "prepared, not applied");
    assert_eq!(run(&mut b, "CALL drevo.tx.commitPrepared('row2')"), None);
    assert_eq!(count(&svc, "ReproA"), 2);
    assert_eq!(count(&svc, "ReproB"), 2);
}

#[test]
fn bolt_a_second_transaction_on_other_records_prepares_too() {
    let svc = Arc::new(NativeService::in_memory());
    let mut a = hello(&svc);
    let mut b = hello(&svc);

    begin(&mut a);
    assert_eq!(run(&mut a, "CREATE (:ReproA)"), None);
    assert_eq!(run(&mut a, "CALL drevo.tx.prepare('gid-a')"), None);
    commit(&mut a);

    // Row 3: an autocommit write elsewhere goes through — fine today.
    assert_eq!(run(&mut b, "CREATE (:ReproB)"), None);

    // Row 4: an explicit transaction elsewhere, prepared.
    begin(&mut b);
    assert_eq!(run(&mut b, "CREATE (:ReproB)"), None);
    let prepared = run(&mut b, "CALL drevo.tx.prepare('gid-b')");
    assert_ne!(prepared.as_deref(), Some(LOCK_TIMEOUT), "disjoint writes");
    assert_eq!(prepared, None);
    commit(&mut b);

    let listed = ok(&svc, "CALL drevo.tx.listPrepared() YIELD gid RETURN gid");
    assert_eq!(
        listed.rows,
        vec![
            vec![Value::String("gid-a".into())],
            vec![Value::String("gid-b".into())]
        ]
    );
    // Resolved in the other order.
    assert_eq!(run(&mut a, "CALL drevo.tx.commitPrepared('gid-b')"), None);
    assert_eq!(run(&mut a, "CALL drevo.tx.commitPrepared('gid-a')"), None);
    assert_eq!(count(&svc, "ReproA"), 1);
    assert_eq!(count(&svc, "ReproB"), 2);
}

#[test]
fn bolt_a_prepare_on_a_record_a_prepared_transaction_wrote_still_waits() {
    let svc = Arc::new(NativeService::in_memory());
    ok(&svc, "CREATE (:Shared {n: 0})");
    let mut a = hello(&svc);
    let mut b = hello(&svc);

    begin(&mut b);
    assert_eq!(run(&mut b, "MATCH (s:Shared) SET s.n = 2"), None);
    begin(&mut a);
    assert_eq!(run(&mut a, "MATCH (s:Shared) SET s.n = 1"), None);
    assert_eq!(run(&mut a, "CALL drevo.tx.prepare('gid-a')"), None);
    commit(&mut a);

    assert_eq!(
        run(&mut b, "CALL drevo.tx.prepare('gid-b')").as_deref(),
        Some(LOCK_TIMEOUT)
    );
    assert_eq!(run(&mut a, "CALL drevo.tx.commitPrepared('gid-a')"), None);
    let n = ok(&svc, "MATCH (s:Shared) RETURN s.n");
    assert_eq!(n.rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn bolt_a_prepare_after_a_commit_to_the_same_record_is_outdated() {
    let svc = Arc::new(NativeService::in_memory());
    ok(&svc, "CREATE (:Shared {n: 0})");
    let mut a = hello(&svc);
    let mut b = hello(&svc);

    begin(&mut a);
    assert_eq!(run(&mut a, "MATCH (s:Shared) SET s.n = 1"), None);
    assert_eq!(run(&mut b, "MATCH (s:Shared) SET s.n = 2"), None);
    assert_eq!(
        run(&mut a, "CALL drevo.tx.prepare('gid-a')").as_deref(),
        Some(OUTDATED)
    );
}

// ── NativeService (drevo-py's path) ──────────────────────────────────────

fn write_in_tx(svc: &NativeService, q: &str) -> drevo::native::NativeTxId {
    let tx = svc.begin_tx();
    svc.execute_in_tx(tx, &parse(q).expect("parse"), HashMap::new())
        .unwrap_or_else(|e| panic!("`{q}` in a transaction: {e:?}"));
    tx
}

#[test]
fn a_service_prepare_ignores_writers_on_other_records() {
    let svc = NativeService::in_memory();
    let a = write_in_tx(&svc, "CREATE (:ReproA)");
    ok(&svc, "CREATE (:ReproB)");
    svc.prepare_tx(a, "gid-a")
        .expect("a commit elsewhere since the transaction began");

    let b = write_in_tx(&svc, "CREATE (:ReproB)");
    svc.prepare_tx(b, "gid-b")
        .expect("a transaction prepared on other records");

    svc.commit_prepared("gid-b").unwrap();
    svc.commit_prepared("gid-a").unwrap();
    assert_eq!(count(&svc, "ReproA"), 1);
    assert_eq!(count(&svc, "ReproB"), 2);
}

#[test]
fn a_service_prepare_on_a_prepared_record_waits_and_names_the_gid() {
    let svc = NativeService::in_memory();
    ok(&svc, "CREATE (:Shared {n: 0})");
    let b = write_in_tx(&svc, "MATCH (s:Shared) SET s.n = 2");
    let a = write_in_tx(&svc, "MATCH (s:Shared) SET s.n = 1");
    svc.prepare_tx(a, "gid-a").unwrap();
    assert_eq!(
        svc.prepare_tx(b, "gid-b"),
        Err(PrepareError::PreparedPending(vec!["gid-a".to_string()]))
    );
    svc.rollback_prepared("gid-a").unwrap();
}
