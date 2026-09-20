//! Integration tests for concurrent managed Bolt transactions on the durable
//! native engine — issue #298, epic #444.
//!
//! Where `bolt_managed_tx_tests` pins the per-statement contract single-
//! threaded, these drive the **real** failure condition: many OS threads, each
//! a separate Bolt connection on one shared [`NativeService`], running
//! graphiti-style *managed* transactions (`BEGIN → several RUN → COMMIT`)
//! concurrently — exactly what a Neo4j driver's pooled `execute_write` does.
//!
//! The durable engine uses **optimistic concurrency**: a transaction commits
//! against the graph version it began on, and a concurrent commit that moved
//! the graph forward makes an in-flight commit fail with
//! `Neo.TransientError.Transaction.Outdated`. This is coarse-grained — even
//! transactions touching disjoint nodes serialize — so the contract these tests
//! pin is the driver one: **retry the managed transaction on a transient
//! failure** and every unit of work eventually commits. Each connection's
//! transaction is independent (its own `NativeTx`), so a failed or still-open
//! transaction on one connection never blocks another's commit.
//!
//! Scenarios modelled:
//!   * a bug-tracker / knowledge-graph bulk write (entity + episode node + a
//!     `MENTIONS` edge) run as one managed transaction, hammered from N threads
//!     with driver-style retry, and
//!   * the driver's retry path — a transaction that fails a statement, then
//!     fresh transactions on other connections that must still commit while the
//!     failed one is left open.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use drevo::bolt::packstream::Value;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::native_service::NativeService;

/// The transient error a managed transaction must retry, per the Neo4j driver
/// contract (`session.execute_write` retries `TransientError`).
const OUTDATED: &str = "Neo.TransientError.Transaction.Outdated";

/// Safety bound so a livelocked test fails loudly instead of spinning forever.
const MAX_RETRIES: usize = 100_000;

fn open() -> Arc<NativeService> {
    Arc::new(NativeService::in_memory())
}

fn hello(d: &Arc<NativeService>) -> Session<'static> {
    let mut s = Session::new_durable(Arc::clone(d));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    s
}

fn is_success(m: &ServerMessage) -> bool {
    matches!(m, ServerMessage::Success { .. })
}

/// The `code` of the first `FAILURE` in a reply, if any.
fn failure_code(replies: &[ServerMessage]) -> Option<String> {
    replies.iter().find_map(|m| match m {
        ServerMessage::Failure { metadata } => Some(match metadata.get("code") {
            Some(Value::String(c)) => c.clone(),
            _ => String::new(),
        }),
        _ => None,
    })
}

/// Run a mutation and drain its stream; `Err(code)` on the first failure.
fn run_step(s: &mut Session, query: &str) -> Result<(), String> {
    let r = s.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    if let Some(code) = failure_code(&r) {
        return Err(code);
    }
    let mut n = BTreeMap::new();
    n.insert("n".to_string(), Value::Integer(-1));
    s.handle(ClientMessage::Pull { extra: n });
    Ok(())
}

/// Count rows a read query returns, over a throwaway autocommit session.
fn count(d: &Arc<NativeService>, query: &str) -> usize {
    let mut s = hello(d);
    let r = s.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    assert!(
        !r.iter().any(|m| matches!(m, ServerMessage::Failure { .. })),
        "count query failed: {query}"
    );
    let mut n = BTreeMap::new();
    n.insert("n".to_string(), Value::Integer(-1));
    s.handle(ClientMessage::Pull { extra: n })
        .into_iter()
        .filter(|m| matches!(m, ServerMessage::Record { .. }))
        .count()
}

/// One graphiti-like bulk write as a single managed transaction: an entity
/// node, an episode node, and a `MENTIONS` edge between them. `Err(code)` if
/// any step failed (so the caller can decide whether to retry).
fn bulk_write_once(s: &mut Session, tag: &str) -> Result<(), String> {
    let begin = s.handle(ClientMessage::Begin {
        extra: BTreeMap::new(),
    });
    if let Some(code) = failure_code(&begin) {
        return Err(code);
    }
    run_step(s, &format!("CREATE (:Entity {{title: 'ent-{tag}'}})"))?;
    run_step(s, &format!("CREATE (:Episode {{title: 'epi-{tag}'}})"))?;
    run_step(
        s,
        &format!(
            "MATCH (e:Entity {{title: 'ent-{tag}'}}), (p:Episode {{title: 'epi-{tag}'}}) \
             CREATE (e)-[:MENTIONS]->(p)"
        ),
    )?;
    let commit = s.handle(ClientMessage::Commit);
    if let Some(code) = failure_code(&commit) {
        return Err(code);
    }
    Ok(())
}

/// Drive a managed transaction the way a Neo4j driver's `execute_write` does:
/// run it, and on a transient (`Outdated`) failure `RESET` the connection and
/// retry the whole unit until it commits. A non-transient failure is returned
/// as-is; livelock past `MAX_RETRIES` panics rather than spins forever.
fn bulk_write_tx(s: &mut Session, tag: &str) -> Result<(), String> {
    for _ in 0..MAX_RETRIES {
        match bulk_write_once(s, tag) {
            Ok(()) => return Ok(()),
            Err(code) if code == OUTDATED => {
                // Roll back the aborted attempt and clear any FAILED state.
                assert!(is_success(&s.handle(ClientMessage::Reset)[0]));
                continue;
            }
            Err(code) => return Err(code),
        }
    }
    panic!("managed transaction for `{tag}` did not converge within {MAX_RETRIES} retries");
}

#[test]
fn concurrent_managed_transactions_all_commit() {
    const THREADS: usize = 4;
    const PER_THREAD: usize = 25;

    let db = open();
    let failures = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let db = &db;
            let failures = &failures;
            scope.spawn(move || {
                // Each thread is its own pooled connection.
                let mut s = hello(db);
                for i in 0..PER_THREAD {
                    let tag = format!("{t}-{i}");
                    if let Err(e) = bulk_write_tx(&mut s, &tag) {
                        eprintln!("thread {t} iter {i}: {e}");
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });

    assert_eq!(
        failures.load(Ordering::Relaxed),
        0,
        "every managed transaction must eventually commit under retry"
    );
    let total = THREADS * PER_THREAD;
    assert_eq!(count(&db, "MATCH (n:Entity) RETURN n"), total);
    assert_eq!(count(&db, "MATCH (n:Episode) RETURN n"), total);
    assert_eq!(
        count(&db, "MATCH (:Entity)-[r:MENTIONS]->(:Episode) RETURN r"),
        total
    );
}

#[test]
fn concurrent_rollbacks_do_not_bleed_across_connections() {
    // Half the threads commit their write, half roll back. Each connection's
    // rollback must undo only its own node — never a peer's committed one.
    const THREADS: usize = 6;

    let db = open();

    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let db = &db;
            scope.spawn(move || {
                let mut s = hello(db);
                if t % 2 == 0 {
                    // Committers retry on the transient conflict until they land.
                    bulk_write_note(&mut s, t);
                } else {
                    // Rollbacks never conflict — they discard their own writes.
                    let begin = s.handle(ClientMessage::Begin {
                        extra: BTreeMap::new(),
                    });
                    assert!(is_success(&begin[0]), "thread {t} BEGIN");
                    run_step(&mut s, &format!("CREATE (:Note {{title: 'n-{t}'}})"))
                        .expect("create in tx");
                    assert!(is_success(&s.handle(ClientMessage::Rollback)[0]));
                }
            });
        }
    });

    // Only the committed (even-numbered) notes survive.
    for t in 0..THREADS {
        let want = if t % 2 == 0 { 1 } else { 0 };
        assert_eq!(
            count(&db, &format!("MATCH (n:Note {{title: 'n-{t}'}}) RETURN n")),
            want,
            "note n-{t}: committed should survive, rolled-back should not"
        );
    }
}

/// Commit a single-node managed transaction with driver-style retry.
fn bulk_write_note(s: &mut Session, t: usize) {
    for _ in 0..MAX_RETRIES {
        let begin = s.handle(ClientMessage::Begin {
            extra: BTreeMap::new(),
        });
        assert!(is_success(&begin[0]), "thread {t} BEGIN");
        if let Err(code) = run_step(s, &format!("CREATE (:Note {{title: 'n-{t}'}})")) {
            assert_eq!(code, OUTDATED, "unexpected RUN failure: {code}");
            assert!(is_success(&s.handle(ClientMessage::Reset)[0]));
            continue;
        }
        match failure_code(&s.handle(ClientMessage::Commit)) {
            None => return,
            Some(code) => {
                assert_eq!(code, OUTDATED, "unexpected COMMIT failure: {code}");
                assert!(is_success(&s.handle(ClientMessage::Reset)[0]));
            }
        }
    }
    panic!("note n-{t} did not converge within {MAX_RETRIES} retries");
}

#[test]
fn driver_retry_after_failed_statement_succeeds_concurrently() {
    // Model the driver's managed-tx retry: connection A's tx fails a statement
    // (duplicate title) and stays open; connections B, C and D keep committing
    // their own transactions — none is blocked by A's still-open failed
    // transaction (each connection has an independent `NativeTx`).
    let db = open();
    // Seed the title that A will collide on.
    {
        let mut s = hello(&db);
        run_step(&mut s, "CREATE (:Item {title: 'dup'})").unwrap();
    }

    // A: open a tx, fail a statement (duplicate title — a permanent client
    // error, not a transient conflict), leave it open (as a pool would).
    let mut a = hello(&db);
    assert!(is_success(
        &a.handle(ClientMessage::Begin {
            extra: BTreeMap::new()
        })[0]
    ));
    let bad = a.handle(ClientMessage::Run {
        query: "CREATE (:Item {title: 'dup'})".to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    assert!(matches!(bad[0], ServerMessage::Failure { .. }));

    // B, C, D each commit their own managed tx (with retry) while A's failed tx
    // is open.
    let ok = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for name in ["b", "c", "d"] {
            let db = &db;
            let ok = &ok;
            scope.spawn(move || {
                let mut s = hello(db);
                if bulk_write_tx(&mut s, name).is_ok() {
                    ok.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    assert_eq!(
        ok.load(Ordering::Relaxed),
        3,
        "peers must commit despite A's still-open failed transaction"
    );

    // A can still recover with RESET.
    assert!(is_success(&a.handle(ClientMessage::Reset)[0]));
}
