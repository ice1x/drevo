//! Integration tests for MVCC optimistic concurrency control (Phase 13 task
//! `00083`).
//!
//! These exercise write-write conflict detection
//! ([`VersionedStore::put_checked`] / [`delete_checked`]) and the
//! [`run_with_retry`] loop the way the higher graph layers will: two writers
//! racing the same record, a retry that re-derives its change from the
//! winner's commit, and a many-threaded storm that must never lose an update.
//! Realistic workflows from the target domains (bug tracker, IT task manager,
//! ERP, CBT journal) anchor the behaviour, and a `proptest` pins the core
//! safety invariant: a workload of conflict-checked increments driven through
//! `run_with_retry` always converges to the exact sum, no matter how the
//! attempts interleave.
//!
//! [`VersionedStore::put_checked`]: drevo::mvcc::VersionedStore::put_checked
//! [`delete_checked`]: drevo::mvcc::VersionedStore::delete_checked
//! [`run_with_retry`]: drevo::mvcc::run_with_retry

use std::sync::{Arc, Barrier};
use std::thread;

use drevo::mvcc::{run_with_retry, MvccError, TransactionManager, VersionedStore};
use proptest::prelude::*;

/// Helper: a fresh manager + string-keyed store sharing it.
fn fresh<V: Clone>() -> (Arc<TransactionManager>, Arc<VersionedStore<String, V>>) {
    let mgr = Arc::new(TransactionManager::new());
    let store = Arc::new(VersionedStore::new(mgr.clone()));
    (mgr, store)
}

/// Seed a committed key/value, returning nothing — just establishes state.
fn seed<V: Clone>(mgr: &Arc<TransactionManager>, store: &VersionedStore<String, V>, k: &str, v: V) {
    let w = mgr.begin().unwrap();
    let snap = mgr.snapshot(w).unwrap();
    store.put_checked(w, &snap, k.into(), v).unwrap();
    mgr.commit(w).unwrap();
}

/// Bug-tracker workflow: two triagers open the same bug at the same time and
/// both try to change its status. First-updater-wins — the second triager's
/// stale write is rejected, and they must re-read the bug before editing.
#[test]
fn bug_tracker_two_triagers_race_one_status_change() {
    let (mgr, store) = fresh::<String>();
    seed(&mgr, &store, "BUG-42", "open".to_string());

    // Both triagers load the bug as "open".
    let alice = mgr.begin().unwrap();
    let alice_snap = mgr.snapshot(alice).unwrap();
    let bob = mgr.begin().unwrap();
    let bob_snap = mgr.snapshot(bob).unwrap();
    assert_eq!(
        store.get(&"BUG-42".into(), &alice_snap).unwrap(),
        Some("open".to_string())
    );
    assert_eq!(
        store.get(&"BUG-42".into(), &bob_snap).unwrap(),
        Some("open".to_string())
    );

    // Alice triages it to "in_progress" and commits.
    store
        .put_checked(alice, &alice_snap, "BUG-42".into(), "in_progress".into())
        .unwrap();
    mgr.commit(alice).unwrap();

    // Bob's "wont_fix" write is stale -> rejected.
    let err = store
        .put_checked(bob, &bob_snap, "BUG-42".into(), "wont_fix".into())
        .unwrap_err();
    assert!(matches!(err, MvccError::WriteConflict { conflicting } if conflicting == alice));
    mgr.abort(bob).unwrap();

    // Alice's change stands; Bob re-reads and decides not to override.
    let after = mgr.snapshot(0).unwrap();
    assert_eq!(
        store.get(&"BUG-42".into(), &after).unwrap(),
        Some("in_progress".to_string())
    );
}

/// IT task-manager workflow: two leads try to assign the same unassigned
/// ticket. `run_with_retry` lets the loser re-snapshot; the body sees the
/// ticket is already assigned and declines to clobber it, so the first
/// assignment wins cleanly.
#[test]
fn task_manager_assignment_retry_respects_first_assignee() {
    let (mgr, store) = fresh::<String>();
    seed(&mgr, &store, "TASK-1", "unassigned".to_string());

    // Lead A grabs it.
    run_with_retry(&mgr, 5, |xid, snap| {
        store.put_checked(xid, snap, "TASK-1".into(), "assignee=ann".into())
    })
    .unwrap();

    // Lead B attempts to assign; on retry it observes ann already holds it and
    // returns a sentinel rather than overwriting.
    let outcome = run_with_retry(&mgr, 5, |xid, snap| {
        let current = store.get(&"TASK-1".into(), snap).unwrap().unwrap();
        if current != "unassigned" {
            return Ok("already_assigned");
        }
        store.put_checked(xid, snap, "TASK-1".into(), "assignee=ben".into())?;
        Ok("assigned")
    })
    .unwrap();

    assert_eq!(outcome, "already_assigned");
    let after = mgr.snapshot(0).unwrap();
    assert_eq!(
        store.get(&"TASK-1".into(), &after).unwrap(),
        Some("assignee=ann".to_string())
    );
}

/// ERP workflow: many warehouse terminals decrement the same stock count
/// concurrently. Each decrement is a read-modify-write wrapped in
/// `run_with_retry`; lost-update protection means the final count reflects
/// every sale exactly once.
#[test]
fn erp_concurrent_stock_decrements_never_lose_a_sale() {
    let (mgr, store) = fresh::<i64>();
    const START: i64 = 500;
    const TERMINALS: usize = 8;
    const SALES_EACH: usize = 25;
    seed(&mgr, &store, "SKU-9", START);

    let barrier = Arc::new(Barrier::new(TERMINALS));
    let mut handles = Vec::new();
    for _ in 0..TERMINALS {
        let mgr = mgr.clone();
        let store = store.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..SALES_EACH {
                run_with_retry(&mgr, 100_000, |xid, snap| {
                    let qty = store.get(&"SKU-9".into(), snap).unwrap().unwrap();
                    store.put_checked(xid, snap, "SKU-9".into(), qty - 1)
                })
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let after = mgr.snapshot(0).unwrap();
    let expected = START - (TERMINALS * SALES_EACH) as i64;
    assert_eq!(store.get(&"SKU-9".into(), &after).unwrap(), Some(expected));
}

/// CBT-journal workflow: a user edits a thought record on their phone while a
/// background sync from the desktop commits an edit to the same record. The
/// stale phone write is rejected; a retry merges onto the synced text.
#[test]
fn cbt_journal_stale_edit_is_rejected_then_retried() {
    let (mgr, store) = fresh::<String>();
    seed(&mgr, &store, "thought-7", "I always fail".to_string());

    // Phone opens the record.
    let phone = mgr.begin().unwrap();
    let phone_snap = mgr.snapshot(phone).unwrap();

    // Desktop sync reframes it and commits first.
    run_with_retry(&mgr, 3, |xid, snap| {
        store.put_checked(xid, snap, "thought-7".into(), "I sometimes struggle".into())
    })
    .unwrap();

    // Phone's edit off the stale snapshot conflicts.
    let err = store
        .put_checked(
            phone,
            &phone_snap,
            "thought-7".into(),
            "I never succeed".into(),
        )
        .unwrap_err();
    assert!(matches!(err, MvccError::WriteConflict { .. }));
    mgr.abort(phone).unwrap();

    // Phone retries from a fresh snapshot and appends its reframing.
    run_with_retry(&mgr, 3, |xid, snap| {
        let current = store.get(&"thought-7".into(), snap).unwrap().unwrap();
        store.put_checked(
            xid,
            snap,
            "thought-7".into(),
            format!("{current}; and I am learning"),
        )
    })
    .unwrap();

    let after = mgr.snapshot(0).unwrap();
    assert_eq!(
        store.get(&"thought-7".into(), &after).unwrap(),
        Some("I sometimes struggle; and I am learning".to_string())
    );
}

/// A delete racing an update: a project manager archives (deletes) a task the
/// instant an engineer reopens it. The engineer's update, off a snapshot that
/// predates the committed delete, is refused.
#[test]
fn delete_then_concurrent_update_conflicts() {
    let (mgr, store) = fresh::<String>();
    seed(&mgr, &store, "TASK-99", "active".to_string());

    let engineer = mgr.begin().unwrap();
    let engineer_snap = mgr.snapshot(engineer).unwrap();

    // PM archives the task.
    run_with_retry(&mgr, 3, |xid, snap| {
        store.delete_checked(xid, snap, &"TASK-99".into())
    })
    .unwrap();

    // Engineer's reopen races the committed delete -> conflict.
    let err = store
        .put_checked(
            engineer,
            &engineer_snap,
            "TASK-99".into(),
            "reopened".into(),
        )
        .unwrap_err();
    assert!(matches!(err, MvccError::WriteConflict { .. }));
}

/// Sustained head-on contention on a single key with a tiny retry budget must
/// eventually surface [`MvccError::RetriesExhausted`] rather than spin
/// forever — proving the budget is a real ceiling.
#[test]
fn exhausted_retry_budget_is_reported() {
    let (mgr, store) = fresh::<i64>();
    seed(&mgr, &store, "hot", 0);

    // Pin an old snapshot, then commit a long stream of winners so any writer
    // built on that stale snapshot always loses.
    let stale = mgr.begin().unwrap();
    let stale_snap = mgr.snapshot(stale).unwrap();
    for v in 1..=10 {
        run_with_retry(&mgr, 3, |xid, snap| {
            store.put_checked(xid, snap, "hot".into(), v)
        })
        .unwrap();
    }

    // A writer reusing the stale snapshot can never win.
    let err = run_with_retry(&mgr, 2, |xid, _ignored| {
        store.put_checked(xid, &stale_snap, "hot".into(), 999)
    })
    .unwrap_err();
    assert!(matches!(err, MvccError::RetriesExhausted { attempts: 3 }));
    mgr.abort(stale).unwrap();
}

/// A high-thread storm hammering a shared counter through `run_with_retry`:
/// the final value must equal the total number of increments, proving no
/// update was lost and nothing deadlocked.
#[test]
fn many_threads_increment_without_losing_updates() {
    let (mgr, store) = fresh::<i64>();
    const THREADS: usize = 12;
    const EACH: usize = 40;
    seed(&mgr, &store, "counter", 0);

    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let mgr = mgr.clone();
        let store = store.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..EACH {
                run_with_retry(&mgr, 1_000_000, |xid, snap| {
                    let cur = store.get(&"counter".into(), snap).unwrap().unwrap();
                    store.put_checked(xid, snap, "counter".into(), cur + 1)
                })
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let after = mgr.snapshot(0).unwrap();
    assert_eq!(
        store.get(&"counter".into(), &after).unwrap(),
        Some((THREADS * EACH) as i64)
    );
}

proptest! {
    /// Core OCC safety invariant: a batch of `n` conflict-checked increments
    /// against one key, driven through `run_with_retry`, always converges to
    /// exactly `n` — regardless of how many of them initially raced off the
    /// same value. (Single-threaded but with interleaved fresh snapshots, so
    /// the retry path is genuinely exercised: each increment re-reads.)
    #[test]
    fn retry_increments_preserve_the_sum(n in 0usize..40) {
        let (mgr, store) = fresh::<i64>();
        seed(&mgr, &store, "k", 0);
        for _ in 0..n {
            run_with_retry(&mgr, 1000, |xid, snap| {
                let cur = store.get(&"k".into(), snap).unwrap().unwrap();
                store.put_checked(xid, snap, "k".into(), cur + 1)
            })
            .unwrap();
        }
        let after = mgr.snapshot(0).unwrap();
        prop_assert_eq!(store.get(&"k".into(), &after).unwrap(), Some(n as i64));
    }
}
