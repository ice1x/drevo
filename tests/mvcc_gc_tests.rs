//! Integration tests for MVCC garbage collection (Phase 13 task `00082`).
//!
//! These exercise the vacuum mechanism and the background [`GcWorker`] the way
//! the higher graph layers will: a long-running reader must never lose a
//! version it can still see, dead versions must be reclaimed once no reader
//! needs them, and a concurrent reader/writer/collector storm must stay
//! correct and never deadlock. Realistic workflows from the target domains
//! (bug tracker, IT task manager, CBT journal, ERP) anchor the behaviour, and
//! a `proptest` pins the core safety invariant: vacuuming never changes what a
//! registered reader observes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use drevo::mvcc::{GcWorker, TransactionManager, VersionedStore, INVALID_XID};
use proptest::prelude::*;

/// Helper: a fresh manager + string-keyed store sharing it.
fn fresh<V: Clone>() -> (Arc<TransactionManager>, VersionedStore<String, V>) {
    let mgr = Arc::new(TransactionManager::new());
    let store = VersionedStore::new(mgr.clone());
    (mgr, store)
}

/// Repeated committed updates leave a long dead chain; one vacuum with no
/// registered reader collapses it to the single live version.
#[test]
fn vacuum_collapses_a_long_update_chain() {
    let (mgr, store) = fresh::<i64>();
    for v in 1..=100 {
        let w = mgr.begin().unwrap();
        store.put(w, "counter".into(), v).unwrap();
        mgr.commit(w).unwrap();
    }
    assert_eq!(store.version_count(&"counter".into()).unwrap(), 100);

    let report = store.vacuum_to_horizon().unwrap();
    assert_eq!(report.reclaimed_versions, 99);
    assert_eq!(store.version_count(&"counter".into()).unwrap(), 1);

    // the surviving version is the latest value
    let snap = mgr.snapshot(INVALID_XID).unwrap();
    assert_eq!(store.get(&"counter".into(), &snap).unwrap(), Some(100));
}

/// Bug-tracker workflow: an analyst opens a long-running report against a bug
/// while triagers keep editing it. GC must not pull the version the report
/// reader is pinned to, and the reader keeps seeing its snapshot value across
/// many vacuum passes.
#[test]
fn bug_tracker_report_reader_is_protected_from_gc() {
    let (mgr, store) = fresh::<String>();

    // bug filed "open"
    let filer = mgr.begin().unwrap();
    store.put(filer, "BUG-7".into(), "open".into()).unwrap();
    mgr.commit(filer).unwrap();

    // analyst opens a registered snapshot for a long report
    let report = mgr.begin_snapshot(INVALID_XID).unwrap();
    assert_eq!(
        store.get(&"BUG-7".into(), &report).unwrap(),
        Some("open".into())
    );

    // triagers churn the status
    for status in ["triaged", "in_progress", "in_review", "resolved"] {
        let w = mgr.begin().unwrap();
        store.put(w, "BUG-7".into(), status.into()).unwrap();
        mgr.commit(w).unwrap();
        // a vacuum runs between every edit, yet the report reader is pinned
        store.vacuum_to_horizon().unwrap();
        assert_eq!(
            store.get(&"BUG-7".into(), &report).unwrap(),
            Some("open".into())
        );
    }

    // the analyst still sees the original snapshot value
    assert_eq!(
        store.get(&"BUG-7".into(), &report).unwrap(),
        Some("open".into())
    );

    // after the report finishes, the intermediate versions can be reclaimed
    drop(report);
    let after = store.vacuum_to_horizon().unwrap();
    assert!(after.reclaimed_versions >= 4);
    assert_eq!(store.version_count(&"BUG-7".into()).unwrap(), 1);
    let now = mgr.snapshot(INVALID_XID).unwrap();
    assert_eq!(
        store.get(&"BUG-7".into(), &now).unwrap(),
        Some("resolved".into())
    );
}

/// CBT-journal workflow: a deleted entry's chain is fully reclaimed once the
/// delete is below the horizon, so the index does not leak tombstones.
#[test]
fn cbt_journal_deleted_entries_are_fully_reclaimed() {
    let (mgr, store) = fresh::<String>();
    for i in 0..5 {
        let w = mgr.begin().unwrap();
        store
            .put(w, format!("entry-{i}"), "a thought".into())
            .unwrap();
        mgr.commit(w).unwrap();
    }
    // user deletes three entries
    for i in 0..3 {
        let d = mgr.begin().unwrap();
        assert!(store.delete(d, &format!("entry-{i}")).unwrap());
        mgr.commit(d).unwrap();
    }

    let report = store.vacuum_to_horizon().unwrap();
    assert_eq!(report.emptied_keys, 3);
    // the deleted keys are gone from the index entirely
    for i in 0..3 {
        assert_eq!(store.version_count(&format!("entry-{i}")).unwrap(), 0);
    }
    // surviving entries remain visible
    let snap = mgr.snapshot(INVALID_XID).unwrap();
    assert_eq!(
        store.get(&"entry-3".into(), &snap).unwrap(),
        Some("a thought".into())
    );
    assert_eq!(
        store.get(&"entry-4".into(), &snap).unwrap(),
        Some("a thought".into())
    );
}

/// ERP workflow: a rolled-back bulk price update leaves dead versions behind;
/// vacuum reclaims the aborted writes and restores the originals as live.
#[test]
fn erp_aborted_bulk_update_is_reclaimed() {
    let (mgr, store) = fresh::<i64>();
    for (sku, price) in [("SKU-1", 100), ("SKU-2", 200), ("SKU-3", 300)] {
        let w = mgr.begin().unwrap();
        store.put(w, sku.into(), price).unwrap();
        mgr.commit(w).unwrap();
    }

    // bulk +10% then abort
    let bulk = mgr.begin().unwrap();
    store.put(bulk, "SKU-1".into(), 110).unwrap();
    store.put(bulk, "SKU-2".into(), 220).unwrap();
    store.put(bulk, "SKU-3".into(), 330).unwrap();
    mgr.abort(bulk).unwrap();
    // each chain now has a dead aborted version on top
    assert_eq!(store.version_count(&"SKU-1".into()).unwrap(), 2);

    let report = store.vacuum_to_horizon().unwrap();
    assert_eq!(report.reclaimed_versions, 3); // the three aborted writes

    // originals are intact and live
    let snap = mgr.snapshot(INVALID_XID).unwrap();
    assert_eq!(store.get(&"SKU-1".into(), &snap).unwrap(), Some(100));
    assert_eq!(store.get(&"SKU-2".into(), &snap).unwrap(), Some(200));
    assert_eq!(store.get(&"SKU-3".into(), &snap).unwrap(), Some(300));
    assert_eq!(store.version_count(&"SKU-1".into()).unwrap(), 1);
}

/// The background worker thread keeps a hot key's chain bounded under a steady
/// stream of writes — without it, the chain would grow without limit.
#[test]
fn background_worker_bounds_chain_growth_under_write_load() {
    let mgr = Arc::new(TransactionManager::new());
    let store: Arc<VersionedStore<String, i64>> = Arc::new(VersionedStore::new(mgr.clone()));

    let worker = GcWorker::spawn(store.clone(), Duration::from_millis(1));

    // hammer one key with 2000 committed updates
    for v in 0..2000 {
        let w = mgr.begin().unwrap();
        store.put(w, "hot".into(), v).unwrap();
        mgr.commit(w).unwrap();
    }

    // give the collector a moment to catch up to the tail of the writes
    for _ in 0..500 {
        if store.version_count(&"hot".into()).unwrap() <= 3 {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let count = store.version_count(&"hot".into()).unwrap();
    worker.stop();

    // the worker reclaimed the vast majority of the 2000 versions
    assert!(
        count <= 5,
        "chain should stay bounded, found {count} versions"
    );
    // the live value is still the latest committed one
    let snap = mgr.snapshot(INVALID_XID).unwrap();
    assert_eq!(store.get(&"hot".into(), &snap).unwrap(), Some(1999));
}

/// A concurrent storm: many writers churn keys, many readers verify they only
/// ever observe legal committed values, and a background collector vacuums the
/// whole time. The run must never tear, lose the seeded key, or deadlock.
#[test]
fn readers_writers_and_collector_never_tear_or_deadlock() {
    let mgr = Arc::new(TransactionManager::new());
    let store: Arc<VersionedStore<String, i64>> = Arc::new(VersionedStore::new(mgr.clone()));

    // seed
    let seed = mgr.begin().unwrap();
    store.put(seed, "k".into(), 0).unwrap();
    mgr.commit(seed).unwrap();

    let worker = GcWorker::spawn(store.clone(), Duration::from_millis(1));

    let barrier = Arc::new(Barrier::new(12));
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();

    // 4 writers, banded so every committed value is identifiable
    for band in 0..4u8 {
        let (mgr, store, barrier) = (mgr.clone(), store.clone(), barrier.clone());
        handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..300i64 {
                let v = (band as i64) * 10_000 + i + 1;
                let w = mgr.begin().unwrap();
                store.put(w, "k".into(), v).unwrap();
                mgr.commit(w).unwrap();
            }
        }));
    }

    // 8 readers: each registers a snapshot, reads it repeatedly (it must be
    // stable for the guard's life even as the collector runs), then releases.
    let legal = |v: i64| v == 0 || (1..=30_300).contains(&v);
    for _ in 0..8 {
        let (mgr, store, barrier, stop) =
            (mgr.clone(), store.clone(), barrier.clone(), stop.clone());
        handles.push(thread::spawn(move || {
            barrier.wait();
            while !stop.load(Ordering::Relaxed) {
                let snap = mgr.begin_snapshot(INVALID_XID).unwrap();
                let first = store.get(&"k".into(), &snap).unwrap();
                assert!(first.is_some(), "seeded key vanished under GC");
                let v = first.unwrap();
                assert!(legal(v), "read a value never committed: {v}");
                // re-read under the same registered snapshot: GC must not move it
                for _ in 0..10 {
                    assert_eq!(store.get(&"k".into(), &snap).unwrap(), Some(v));
                }
            }
        }));
    }

    // let the writers finish, then signal readers to stop
    for h in handles.drain(0..4) {
        h.join().expect("a writer panicked or deadlocked");
    }
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().expect("a reader panicked or deadlocked");
    }
    worker.stop();

    // final state: latest banded value is visible, chain has been collected
    let snap = mgr.snapshot(INVALID_XID).unwrap();
    assert!(store.get(&"k".into(), &snap).unwrap().is_some());
}

proptest! {
    /// Safety invariant: vacuuming never changes what a registered reader sees.
    /// We apply a random history of committed updates and deletes, registering
    /// a snapshot at a random point, then vacuum at the live horizon and assert
    /// the registered reader still resolves every key to exactly what it saw
    /// before the vacuum.
    #[test]
    fn vacuum_preserves_registered_reader_view(
        ops in prop::collection::vec((0u8..5u8, prop_oneof![Just(None), any::<i32>().prop_map(Some)]), 1..50),
        pin_at in 0usize..50,
    ) {
        let mgr = Arc::new(TransactionManager::new());
        let store: VersionedStore<u8, i32> = VersionedStore::new(mgr.clone());

        let mut guard = None;
        for (idx, (key, val)) in ops.iter().enumerate() {
            // register the protected reader partway through the history
            if idx == pin_at % ops.len() {
                guard = Some(mgr.begin_snapshot(INVALID_XID).unwrap());
            }
            let w = mgr.begin().unwrap();
            match val {
                Some(v) => store.put(w, *key, *v).unwrap(),
                None => { store.delete(w, key).unwrap(); }
            }
            mgr.commit(w).unwrap();
        }
        let guard = guard.unwrap_or_else(|| mgr.begin_snapshot(INVALID_XID).unwrap());

        // record what the protected reader sees, vacuum, then re-check.
        let before: Vec<(u8, Option<i32>)> =
            (0u8..5).map(|k| (k, store.get(&k, &guard).unwrap())).collect();
        store.vacuum_to_horizon().unwrap();
        for (k, expected) in before {
            prop_assert_eq!(store.get(&k, &guard).unwrap(), expected);
        }
    }
}
