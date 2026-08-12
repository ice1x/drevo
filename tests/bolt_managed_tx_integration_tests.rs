//! Integration tests for concurrent managed Bolt transactions — issue #298.
//!
//! Where `bolt_managed_tx_tests` pins the per-statement contract single-
//! threaded, these drive the **real** failure condition: many OS threads,
//! each a separate Bolt connection on one shared `Drevo` handle, running
//! graphiti-style *managed* transactions (`BEGIN → several RUN → COMMIT`)
//! concurrently — exactly what a Neo4j driver's pooled `execute_write` does.
//! Before per-connection transactions this deadlocked on a single global slot
//! and returned `Neo.TransientError.Transaction.Outdated: transaction already
//! active`; now every connection commits its own unit of work.
//!
//! Scenarios modelled:
//!   * a bug-tracker / knowledge-graph bulk write (entity + episode node +
//!     a `MENTIONS` edge) run as one managed transaction, hammered from N
//!     threads, and
//!   * the driver's retry path — a transaction that fails a statement, then
//!     a fresh transaction that must still succeed while the failed one is
//!     open on another connection.

#![cfg(all(not(target_arch = "wasm32"), feature = "redb-backend"))]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use drevo::bolt::packstream::Value;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::db::Drevo;

fn hello(d: &Drevo) -> Session<'_> {
    let mut s = Session::new(d);
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    s
}

fn is_success(m: &ServerMessage) -> bool {
    matches!(m, ServerMessage::Success { .. })
}

fn run_drain(s: &mut Session, query: &str) -> Result<(), String> {
    let r = s.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    if let Some(ServerMessage::Failure { metadata }) = r
        .iter()
        .find(|m| matches!(m, ServerMessage::Failure { .. }))
    {
        return Err(format!("{metadata:?}"));
    }
    let mut n = BTreeMap::new();
    n.insert("n".to_string(), Value::Integer(-1));
    s.handle(ClientMessage::Pull { extra: n });
    Ok(())
}

/// Count rows a read query returns, over a throwaway autocommit session.
fn count(d: &Drevo, query: &str) -> usize {
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
/// node, an episode node, and a `MENTIONS` edge between them. Returns an error
/// string if any step failed (so the caller can assert none did).
fn bulk_write_tx(s: &mut Session, tag: &str) -> Result<(), String> {
    let begin = s.handle(ClientMessage::Begin {
        extra: BTreeMap::new(),
    });
    if !is_success(&begin[0]) {
        return Err(format!("BEGIN failed: {:?}", begin[0]));
    }
    run_drain(s, &format!("CREATE (:Entity {{title: 'ent-{tag}'}})"))?;
    run_drain(s, &format!("CREATE (:Episode {{title: 'epi-{tag}'}})"))?;
    run_drain(
        s,
        &format!(
            "MATCH (e:Entity {{title: 'ent-{tag}'}}), (p:Episode {{title: 'epi-{tag}'}}) \
             CREATE (e)-[:MENTIONS]->(p)"
        ),
    )?;
    let commit = s.handle(ClientMessage::Commit);
    if !is_success(&commit[0]) {
        return Err(format!("COMMIT failed: {:?}", commit[0]));
    }
    Ok(())
}

#[test]
fn concurrent_managed_transactions_all_commit() {
    const THREADS: usize = 4;
    const PER_THREAD: usize = 25;

    let db = Drevo::open_in_memory().expect("open");
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
                        // The pre-fix failure surfaced here as
                        // "transaction already active".
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
        "no managed transaction may fail under concurrency"
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

    let db = Drevo::open_in_memory().expect("open");

    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let db = &db;
            scope.spawn(move || {
                let mut s = hello(db);
                let begin = s.handle(ClientMessage::Begin {
                    extra: BTreeMap::new(),
                });
                assert!(is_success(&begin[0]), "thread {t} BEGIN");
                run_drain(&mut s, &format!("CREATE (:Note {{title: 'n-{t}'}})"))
                    .expect("create in tx");
                if t % 2 == 0 {
                    assert!(is_success(&s.handle(ClientMessage::Commit)[0]));
                } else {
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

#[test]
fn driver_retry_after_failed_statement_succeeds_concurrently() {
    // Model the driver's managed-tx retry: connection A's tx fails a statement
    // (duplicate title) and stays open; connections B and C keep committing
    // their own transactions, and a fresh retry on D commits too — none is
    // blocked by A's still-open failed transaction.
    let db = Drevo::open_in_memory().expect("open");
    // Seed the title that A will collide on.
    {
        let mut s = hello(&db);
        run_drain(&mut s, "CREATE (:Item {title: 'dup'})").unwrap();
    }

    // A: open a tx, fail a statement, leave it open (as a pool would).
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

    // B, C, D each commit their own managed tx while A's failed tx is open.
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
