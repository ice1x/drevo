//! Integration tests for MVCC configurable isolation levels (Phase 13 task
//! `00084`).
//!
//! These drive the [`Transaction`] handle and the [`run_transaction`] retry
//! loop the way the higher graph layers will, anchoring each isolation level's
//! guarantee in a realistic workflow from the target domains (bug tracker, IT
//! task manager, ERP, CBT journal):
//!
//! * **Read Committed** — a dashboard re-reads and sees committed updates
//!   between statements, but never a dirty (uncommitted) write.
//! * **Snapshot Isolation** — a long-running report reads a stable view even
//!   as writers move on (repeatable reads), but the write-skew anomaly is
//!   permitted.
//! * **Serializable** — the same write-skew schedule is refused, keeping a
//!   cross-row invariant intact; disjoint transactions still commit; and a
//!   contended workload driven through [`run_transaction`] converges.
//!
//! A `proptest` pins the core safety invariant: serializable read-modify-write
//! increments through [`run_transaction`] always converge to the exact sum, no
//! matter how the attempts interleave.
//!
//! [`Transaction`]: drevo::mvcc::Transaction
//! [`run_transaction`]: drevo::mvcc::run_transaction

use std::sync::{Arc, Barrier};
use std::thread;

use drevo::mvcc::{run_transaction, IsolationLevel, MvccError, TransactionManager, VersionedStore};
use proptest::prelude::*;

/// A fresh manager + string-keyed store sharing it.
fn fresh<V: Clone>() -> Arc<VersionedStore<String, V>> {
    let mgr = Arc::new(TransactionManager::new());
    Arc::new(VersionedStore::new(mgr))
}

/// Commit an initial value so a test starts from known committed state.
fn seed<V: Clone>(store: &VersionedStore<String, V>, key: &str, value: V) {
    let mut tx = store
        .begin_transaction(IsolationLevel::SnapshotIsolation)
        .unwrap();
    tx.put(key.into(), value).unwrap();
    tx.commit().unwrap();
}

// --- Read Committed ----------------------------------------------------------

/// IT task-manager dashboard: a board polls a ticket's status repeatedly. Under
/// Read Committed each poll re-snapshots, so once an agent commits a status
/// change the dashboard sees it — but it never observes an in-flight edit.
#[test]
fn read_committed_dashboard_sees_committed_not_dirty() {
    let store = fresh::<String>();
    seed(&store, "TICKET-7", "open".to_string());

    let mut dashboard = store
        .begin_transaction(IsolationLevel::ReadCommitted)
        .unwrap();
    assert_eq!(
        dashboard.get(&"TICKET-7".into()).unwrap(),
        Some("open".to_string())
    );

    // An agent starts editing but has NOT committed: no dirty read.
    let mut agent = store
        .begin_transaction(IsolationLevel::ReadCommitted)
        .unwrap();
    agent
        .put("TICKET-7".into(), "in_progress".to_string())
        .unwrap();
    assert_eq!(
        dashboard.get(&"TICKET-7".into()).unwrap(),
        Some("open".to_string())
    );

    // Once the agent commits, the dashboard's next poll reflects it.
    agent.commit().unwrap();
    assert_eq!(
        dashboard.get(&"TICKET-7".into()).unwrap(),
        Some("in_progress".to_string())
    );
}

// --- Snapshot Isolation ------------------------------------------------------

/// ERP month-end report: a long-running reader tallies inventory while sales
/// terminals keep selling. Under Snapshot Isolation the report sees one
/// consistent point-in-time view for its whole life, even as committed writes
/// pile up — exactly the repeatable-read guarantee a report needs.
#[test]
fn snapshot_isolation_report_sees_stable_view() {
    let store = fresh::<i64>();
    seed(&store, "sku_widgets", 100);
    seed(&store, "sku_gadgets", 50);

    let mut report = store
        .begin_transaction(IsolationLevel::SnapshotIsolation)
        .unwrap();
    let widgets_at_start = report.get(&"sku_widgets".into()).unwrap().unwrap();

    // Sales terminals commit several decrements while the report runs.
    for _ in 0..10 {
        run_transaction(&store, IsolationLevel::SnapshotIsolation, 5, |tx| {
            let cur = tx.get(&"sku_widgets".into())?.unwrap();
            tx.put("sku_widgets".into(), cur - 1)
        })
        .unwrap();
    }

    // The report's repeated reads are unchanged: stable snapshot.
    assert_eq!(
        report.get(&"sku_widgets".into()).unwrap(),
        Some(widgets_at_start)
    );
    assert_eq!(report.get(&"sku_gadgets".into()).unwrap(), Some(50));

    // A fresh reader, however, sees the committed decrements.
    let mut fresh_reader = store
        .begin_transaction(IsolationLevel::SnapshotIsolation)
        .unwrap();
    assert_eq!(fresh_reader.get(&"sku_widgets".into()).unwrap(), Some(90));
}

/// The anomaly Snapshot Isolation famously *permits*: write-skew. Two
/// transactions read an overlapping set and write disjoint keys, so there is no
/// write-write conflict — both commit, even though their combined effect was
/// never possible under any serial order. This documents the gap that
/// Serializable closes.
#[test]
fn snapshot_isolation_permits_write_skew() {
    let store = fresh::<i64>();
    seed(&store, "alice_on_call", 1);
    seed(&store, "bob_on_call", 1);

    let mut t1 = store
        .begin_transaction(IsolationLevel::SnapshotIsolation)
        .unwrap();
    let mut t2 = store
        .begin_transaction(IsolationLevel::SnapshotIsolation)
        .unwrap();
    t1.get(&"alice_on_call".into()).unwrap();
    t1.get(&"bob_on_call".into()).unwrap();
    t2.get(&"alice_on_call".into()).unwrap();
    t2.get(&"bob_on_call".into()).unwrap();
    t1.put("alice_on_call".into(), 0).unwrap();
    t2.put("bob_on_call".into(), 0).unwrap();
    t1.commit().unwrap();
    t2.commit().unwrap(); // SI lets both through

    // The "at least one on call" invariant is now violated: 0 + 0.
    let mut check = store
        .begin_transaction(IsolationLevel::SnapshotIsolation)
        .unwrap();
    let total = check.get(&"alice_on_call".into()).unwrap().unwrap()
        + check.get(&"bob_on_call".into()).unwrap().unwrap();
    assert_eq!(total, 0);
}

// --- Serializable ------------------------------------------------------------

/// The same on-call write-skew schedule under Serializable: the second
/// committer read a row the first one modified, so it is refused with a
/// serialization failure and the invariant is preserved.
#[test]
fn serializable_refuses_write_skew_and_keeps_invariant() {
    let store = fresh::<i64>();
    seed(&store, "alice_on_call", 1);
    seed(&store, "bob_on_call", 1);

    let mut t1 = store
        .begin_transaction(IsolationLevel::Serializable)
        .unwrap();
    let mut t2 = store
        .begin_transaction(IsolationLevel::Serializable)
        .unwrap();
    t1.get(&"alice_on_call".into()).unwrap();
    t1.get(&"bob_on_call".into()).unwrap();
    t2.get(&"alice_on_call".into()).unwrap();
    t2.get(&"bob_on_call".into()).unwrap();
    t1.put("alice_on_call".into(), 0).unwrap();
    t2.put("bob_on_call".into(), 0).unwrap();

    t1.commit().unwrap();
    let err = t2.commit().unwrap_err();
    assert!(matches!(err, MvccError::SerializationFailure { .. }));

    let mut check = store
        .begin_transaction(IsolationLevel::SnapshotIsolation)
        .unwrap();
    let total = check.get(&"alice_on_call".into()).unwrap().unwrap()
        + check.get(&"bob_on_call".into()).unwrap().unwrap();
    assert_eq!(total, 1); // invariant preserved
}

/// Serializable must not raise *false* serialization failures: two transactions
/// reading and writing completely disjoint CBT-journal entries both commit.
#[test]
fn serializable_disjoint_journal_entries_both_commit() {
    let store = fresh::<String>();
    seed(&store, "entry:mon", "anxious".to_string());
    seed(&store, "entry:tue", "calm".to_string());

    let mut t1 = store
        .begin_transaction(IsolationLevel::Serializable)
        .unwrap();
    let mut t2 = store
        .begin_transaction(IsolationLevel::Serializable)
        .unwrap();
    t1.get(&"entry:mon".into()).unwrap();
    t2.get(&"entry:tue".into()).unwrap();
    t1.put("entry:mon".into(), "reframed".to_string()).unwrap();
    t2.put("entry:tue".into(), "grateful".to_string()).unwrap();
    t1.commit().unwrap();
    t2.commit().unwrap();

    let mut check = store
        .begin_transaction(IsolationLevel::SnapshotIsolation)
        .unwrap();
    assert_eq!(
        check.get(&"entry:mon".into()).unwrap(),
        Some("reframed".to_string())
    );
    assert_eq!(
        check.get(&"entry:tue".into()).unwrap(),
        Some("grateful".to_string())
    );
}

/// Serializable read-modify-write under contention, via the retry loop: a
/// bug-tracker "open bug count" maintained by many concurrent triagers. Every
/// increment must land — retries re-derive each change against the winner.
#[test]
fn serializable_contended_counter_converges_via_retry() {
    let store = fresh::<i64>();
    seed(&store, "open_bug_count", 0);

    const THREADS: usize = 8;
    const PER_THREAD: usize = 25;
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..PER_THREAD {
                run_transaction(&store, IsolationLevel::Serializable, 10_000, |tx| {
                    let cur = tx.get(&"open_bug_count".into())?.unwrap();
                    tx.put("open_bug_count".into(), cur + 1)
                })
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let mut check = store
        .begin_transaction(IsolationLevel::SnapshotIsolation)
        .unwrap();
    assert_eq!(
        check.get(&"open_bug_count".into()).unwrap(),
        Some((THREADS * PER_THREAD) as i64)
    );
}

/// A many-threaded read storm against a snapshot-isolated store must never tear
/// or deadlock while writers commit concurrently — the mixed-level workload the
/// phase targets.
#[test]
fn mixed_level_read_storm_never_tears() {
    let store = fresh::<i64>();
    seed(&store, "k", 1);

    const READERS: usize = 16;
    let barrier = Arc::new(Barrier::new(READERS + 1));
    let mut handles = Vec::new();
    for _ in 0..READERS {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            // A snapshot-isolated reader sees one stable value across reads.
            let mut tx = store
                .begin_transaction(IsolationLevel::SnapshotIsolation)
                .unwrap();
            let first = tx.get(&"k".into()).unwrap();
            for _ in 0..50 {
                assert_eq!(tx.get(&"k".into()).unwrap(), first); // never tears
            }
        }));
    }
    // A writer races, committing updates.
    let writer = {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            for v in 2..=20 {
                run_transaction(&store, IsolationLevel::ReadCommitted, 1000, |tx| {
                    tx.put("k".into(), v)
                })
                .unwrap();
            }
        })
    };
    for h in handles {
        h.join().unwrap();
    }
    writer.join().unwrap();
}

// --- property: serializable increments always converge -----------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// N serializable read-modify-write increments through `run_transaction`,
    /// spread across a variable number of threads, always converge to exactly
    /// N — no lost updates, no spurious aborts that drop work.
    #[test]
    fn serializable_increments_converge_to_exact_sum(
        threads in 1usize..=6,
        per_thread in 1usize..=20,
    ) {
        let store = fresh::<i64>();
        seed(&store, "n", 0);

        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::new();
        for _ in 0..threads {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..per_thread {
                    run_transaction(&store, IsolationLevel::Serializable, 100_000, |tx| {
                        let cur = tx.get(&"n".into())?.unwrap();
                        tx.put("n".into(), cur + 1)
                    })
                    .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let mut check = store
            .begin_transaction(IsolationLevel::SnapshotIsolation)
            .unwrap();
        prop_assert_eq!(check.get(&"n".into()).unwrap(), Some((threads * per_thread) as i64));
    }
}
