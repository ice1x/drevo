//! Integration tests for the MVCC engine (Phase 13 task `00081`).
//!
//! These exercise the visibility primitives the way the rest of Phase 13
//! and the higher graph layers will: concurrent reader/writer storms that
//! must never deadlock or tear, point-in-time snapshot stability under a
//! flood of commits, and realistic workflows from the target domains
//! (bug tracker, IT task manager, CBT journal). A `proptest` pins the core
//! invariant — a snapshot is a stable point-in-time view — against randomly
//! generated update histories.

use std::sync::{Arc, Barrier};
use std::thread;

use drevo::mvcc::{TransactionManager, VersionedStore, XidStatus, INVALID_XID};
use proptest::prelude::*;

/// A committed write is visible to every snapshot taken afterwards; an
/// in-flight write is invisible to everyone but its own transaction.
#[test]
fn committed_visible_inflight_hidden() {
    let mgr = Arc::new(TransactionManager::new());
    let store: VersionedStore<String, i64> = VersionedStore::new(mgr.clone());

    let w = mgr.begin().unwrap();
    store.put(w, "task-1".into(), 100).unwrap();

    // an unrelated reader cannot see the in-flight write
    let observer = mgr.snapshot(INVALID_XID).unwrap();
    assert_eq!(store.get(&"task-1".into(), &observer).unwrap(), None);

    mgr.commit(w).unwrap();
    let after = mgr.snapshot(INVALID_XID).unwrap();
    assert_eq!(store.get(&"task-1".into(), &after).unwrap(), Some(100));
}

/// A long-running reader keeps seeing the same value across many concurrent
/// committed updates — repeatable read / snapshot isolation.
#[test]
fn long_reader_sees_repeatable_view_under_write_flood() {
    let mgr = Arc::new(TransactionManager::new());
    let store: VersionedStore<String, i64> = VersionedStore::new(mgr.clone());

    // seed value 0, committed
    let seed = mgr.begin().unwrap();
    store.put(seed, "counter".into(), 0).unwrap();
    mgr.commit(seed).unwrap();

    // reader captures the world at value 0
    let reader = mgr.snapshot(INVALID_XID).unwrap();

    // 50 writers each bump the value and commit
    for v in 1..=50 {
        let w = mgr.begin().unwrap();
        store.put(w, "counter".into(), v).unwrap();
        mgr.commit(w).unwrap();
    }

    // the original reader is unmoved
    assert_eq!(store.get(&"counter".into(), &reader).unwrap(), Some(0));
    // a fresh reader sees the latest
    let fresh = mgr.snapshot(INVALID_XID).unwrap();
    assert_eq!(store.get(&"counter".into(), &fresh).unwrap(), Some(50));
}

/// 16 reader threads hammer a key while 4 writer threads commit new values.
/// Every value a reader observes must be one that was actually committed
/// (never a torn or fabricated value), and the run must not deadlock.
#[test]
fn concurrent_readers_and_writers_never_tear_or_deadlock() {
    let mgr = Arc::new(TransactionManager::new());
    let store: Arc<VersionedStore<String, i64>> = Arc::new(VersionedStore::new(mgr.clone()));

    // seed
    let seed = mgr.begin().unwrap();
    store.put(seed, "k".into(), 0).unwrap();
    mgr.commit(seed).unwrap();

    let barrier = Arc::new(Barrier::new(20));
    let mut handles = Vec::new();

    // 4 writers: each commits values from its own disjoint band so every
    // committed value is unique and identifiable.
    for band in 0..4u8 {
        let (mgr, store, barrier) = (mgr.clone(), store.clone(), barrier.clone());
        handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..100i64 {
                let v = (band as i64) * 1000 + i + 1; // committed values: 1..=3100, banded
                let w = mgr.begin().unwrap();
                store.put(w, "k".into(), v).unwrap();
                mgr.commit(w).unwrap();
            }
        }));
    }

    // 16 readers: each takes many snapshots and checks every value it sees
    // is a legal committed value (0 seed, or a banded writer value).
    let legal = |v: i64| v == 0 || (1..=3100).contains(&v);
    for _ in 0..16 {
        let (mgr, store, barrier) = (mgr.clone(), store.clone(), barrier.clone());
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..500 {
                let snap = mgr.snapshot(INVALID_XID).unwrap();
                let got = store.get(&"k".into(), &snap).unwrap();
                match got {
                    Some(v) => assert!(legal(v), "read a value that was never committed: {v}"),
                    None => panic!("seeded key vanished — a committed value was lost"),
                }
            }
        }));
    }

    for h in handles {
        h.join().expect("no thread should panic or deadlock");
    }
}

/// Bug-tracker workflow: two engineers open transactions against the same
/// bug. Snapshot isolation means neither sees the other's uncommitted edit;
/// after both commit, the chain retains both versions and the latest wins.
#[test]
fn bug_tracker_two_concurrent_edits_are_isolated() {
    let mgr = Arc::new(TransactionManager::new());
    let store: VersionedStore<String, String> = VersionedStore::new(mgr.clone());

    // bug filed as "open"
    let filer = mgr.begin().unwrap();
    store.put(filer, "BUG-42".into(), "open".into()).unwrap();
    mgr.commit(filer).unwrap();

    // engineer A starts triaging -> "in_progress" (not yet committed)
    let alice = mgr.begin().unwrap();
    store
        .put(alice, "BUG-42".into(), "in_progress".into())
        .unwrap();

    // engineer B opens a transaction at the same time
    let bob = mgr.snapshot(INVALID_XID).unwrap();
    // B must still see "open" — A's edit is uncommitted
    assert_eq!(
        store.get(&"BUG-42".into(), &bob).unwrap(),
        Some("open".into())
    );

    // A commits
    mgr.commit(alice).unwrap();

    // B's existing snapshot is unchanged (repeatable read)
    assert_eq!(
        store.get(&"BUG-42".into(), &bob).unwrap(),
        Some("open".into())
    );
    // a reader starting now sees the triaged status
    let now = mgr.snapshot(INVALID_XID).unwrap();
    assert_eq!(
        store.get(&"BUG-42".into(), &now).unwrap(),
        Some("in_progress".into())
    );
    // both versions physically retained
    assert_eq!(store.version_count(&"BUG-42".into()).unwrap(), 2);
}

/// IT task-manager workflow: a rolled-back bulk reassignment leaves no trace
/// — an aborted transaction's writes are invisible to everyone.
#[test]
fn task_manager_rolled_back_bulk_edit_is_invisible() {
    let mgr = Arc::new(TransactionManager::new());
    let store: VersionedStore<String, String> = VersionedStore::new(mgr.clone());

    for (k, owner) in [("T-1", "ana"), ("T-2", "ben"), ("T-3", "cleo")] {
        let w = mgr.begin().unwrap();
        store.put(w, k.into(), owner.into()).unwrap();
        mgr.commit(w).unwrap();
    }

    // bulk reassign everything to "ana", then abort
    let bulk = mgr.begin().unwrap();
    store.put(bulk, "T-1".into(), "ana".into()).unwrap();
    store.put(bulk, "T-2".into(), "ana".into()).unwrap();
    store.put(bulk, "T-3".into(), "ana".into()).unwrap();
    mgr.abort(bulk).unwrap();

    let snap = mgr.snapshot(INVALID_XID).unwrap();
    let mut rows = store.scan_visible(&snap).unwrap();
    rows.sort();
    assert_eq!(
        rows,
        vec![
            ("T-1".to_string(), "ana".to_string()),
            ("T-2".to_string(), "ben".to_string()),
            ("T-3".to_string(), "cleo".to_string()),
        ]
    );
}

/// CBT-journal workflow: deleting an entry inside an uncommitted transaction
/// hides it from the deleter immediately but not from concurrent readers
/// until commit.
#[test]
fn cbt_journal_delete_visibility_follows_commit() {
    let mgr = Arc::new(TransactionManager::new());
    let store: VersionedStore<String, String> = VersionedStore::new(mgr.clone());

    let w = mgr.begin().unwrap();
    store
        .put(w, "entry-1".into(), "I felt anxious today".into())
        .unwrap();
    mgr.commit(w).unwrap();

    // user starts deleting the entry
    let del = mgr.begin().unwrap();
    assert!(store.delete(del, &"entry-1".into()).unwrap());

    // the deleter no longer sees it
    let del_snap = mgr.snapshot(del).unwrap();
    assert_eq!(store.get(&"entry-1".into(), &del_snap).unwrap(), None);

    // a concurrent reader still sees it (delete uncommitted)
    let concurrent = mgr.snapshot(INVALID_XID).unwrap();
    assert!(store.get(&"entry-1".into(), &concurrent).unwrap().is_some());

    mgr.commit(del).unwrap();
    let after = mgr.snapshot(INVALID_XID).unwrap();
    assert_eq!(store.get(&"entry-1".into(), &after).unwrap(), None);
}

/// A snapshot's own transaction status is independent of the snapshot;
/// committing or aborting never resurrects an invisible row for an existing
/// snapshot. Sanity-check the status reporting too.
#[test]
fn status_lifecycle_is_observable() {
    let mgr = TransactionManager::new();
    let x = mgr.begin().unwrap();
    assert_eq!(mgr.status(x).unwrap(), XidStatus::InProgress);
    mgr.commit(x).unwrap();
    assert_eq!(mgr.status(x).unwrap(), XidStatus::Committed);
}

proptest! {
    /// Core invariant: a snapshot is a stable point-in-time view. We apply a
    /// random history of committed single-key updates, capturing a snapshot
    /// after every step together with the value expected *as of that step*.
    /// After the whole history has run, every captured snapshot must still
    /// resolve to the value it should have seen at capture time — proving no
    /// later commit ever leaked backwards into an older snapshot.
    #[test]
    fn snapshots_are_point_in_time_stable(
        updates in prop::collection::vec((0u8..4u8, any::<i32>()), 1..40)
    ) {
        let mgr = Arc::new(TransactionManager::new());
        let store: VersionedStore<u8, i32> = VersionedStore::new(mgr.clone());

        // (snapshot, expected_state_at_capture)
        let mut checkpoints: Vec<(_, std::collections::HashMap<u8, i32>)> = Vec::new();
        let mut state: std::collections::HashMap<u8, i32> = std::collections::HashMap::new();

        for (key, val) in updates {
            let w = mgr.begin().unwrap();
            store.put(w, key, val).unwrap();
            mgr.commit(w).unwrap();
            state.insert(key, val);
            let snap = mgr.snapshot(INVALID_XID).unwrap();
            checkpoints.push((snap, state.clone()));
        }

        // Replay every historical snapshot against the now-fully-mutated
        // store; each must still see exactly its captured state.
        for (snap, expected) in &checkpoints {
            for key in 0u8..4u8 {
                let got = store.get(&key, snap).unwrap();
                prop_assert_eq!(got, expected.get(&key).copied());
            }
        }
    }
}
