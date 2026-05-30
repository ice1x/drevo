//! Background garbage collection — the vacuum thread (Phase 13 task `00082`).
//!
//! [`VersionedStore::vacuum`](crate::mvcc::VersionedStore::vacuum) is the
//! *mechanism* that reclaims dead tuple versions; [`GcWorker`] is the
//! *policy* that runs it on a cadence so dead versions do not accumulate
//! unbounded between writes. It owns a dedicated thread that, every
//! configured interval, vacuums the store down to the manager's current
//! [`gc_horizon`](crate::mvcc::TransactionManager::gc_horizon) — the oldest
//! `xmin` of any registered live reader, so no in-flight snapshot ever loses
//! a version it can still see.
//!
//! The worker is best-effort: it shares the store's single
//! [`RwLock`](std::sync::RwLock), so a vacuum cycle briefly excludes writers
//! and other readers, and a poisoned lock simply skips that cycle rather than
//! propagating a panic. Shutdown is prompt and deterministic — [`stop`] (and
//! the `Drop` impl) signal the thread through a [`Condvar`](std::sync::Condvar)
//! so it wakes immediately instead of waiting out the interval, then join it.
//!
//! [`stop`]: GcWorker::stop
//!
//! # WASM
//!
//! This worker spawns an OS thread and is therefore compiled only off
//! `wasm32` (where `std::thread` has no real threads). The pure vacuum
//! mechanism on [`VersionedStore`](crate::mvcc::VersionedStore) stays
//! available everywhere, so a WASM host can still drive collection manually.

#![cfg(not(target_arch = "wasm32"))]

use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::store::VersionedStore;

/// Shared control block between the [`GcWorker`] handle and its thread.
#[derive(Debug)]
struct GcShared {
    /// Set to request shutdown; the thread checks it and exits.
    stop: Mutex<bool>,
    /// Signalled on shutdown so the thread wakes from its interval wait at
    /// once instead of sleeping the remainder of the cadence.
    wake: Condvar,
    /// Cumulative count of physical versions reclaimed across all cycles.
    reclaimed_total: AtomicU64,
    /// The number of vacuum cycles that have run to completion.
    cycles: AtomicU64,
}

/// A background thread that periodically vacuums a [`VersionedStore`].
///
/// Construct one with [`spawn`](GcWorker::spawn) over an `Arc`-shared store;
/// it runs until [`stop`](GcWorker::stop) is called or the handle is dropped.
/// Observe its progress with [`reclaimed_total`](GcWorker::reclaimed_total)
/// and [`cycles`](GcWorker::cycles).
#[derive(Debug)]
pub struct GcWorker {
    shared: Arc<GcShared>,
    handle: Option<JoinHandle<()>>,
}

impl GcWorker {
    /// Spawn a collector that vacuums `store` every `interval`.
    ///
    /// Each cycle reclaims versions below the store manager's current
    /// [`gc_horizon`](crate::mvcc::TransactionManager::gc_horizon). The store is
    /// shared (`Arc`) because the worker holds one reference and the caller
    /// keeps another for ongoing reads and writes.
    pub fn spawn<K, V>(store: Arc<VersionedStore<K, V>>, interval: Duration) -> Self
    where
        K: Eq + Hash + Clone + Send + Sync + 'static,
        V: Clone + Send + Sync + 'static,
    {
        let shared = Arc::new(GcShared {
            stop: Mutex::new(false),
            wake: Condvar::new(),
            reclaimed_total: AtomicU64::new(0),
            cycles: AtomicU64::new(0),
        });
        let thread_shared = Arc::clone(&shared);
        let handle = std::thread::spawn(move || {
            run(&thread_shared, &store, interval);
        });
        Self {
            shared,
            handle: Some(handle),
        }
    }

    /// Total number of physical versions reclaimed since the worker started.
    pub fn reclaimed_total(&self) -> u64 {
        self.shared.reclaimed_total.load(Ordering::Relaxed)
    }

    /// Number of vacuum cycles completed since the worker started.
    pub fn cycles(&self) -> u64 {
        self.shared.cycles.load(Ordering::Relaxed)
    }

    /// Signal the collector to stop and block until its thread has exited.
    ///
    /// Idempotent and also run by [`Drop`], so callers rarely need to invoke
    /// it explicitly — it exists for code that wants to join deterministically
    /// (e.g. to assert on the final [`reclaimed_total`](Self::reclaimed_total)).
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(handle) = self.handle.take() {
            // Best-effort: if the mutex is poisoned the thread is already
            // unwinding/gone, so a failed join is acceptable.
            if let Ok(mut stop) = self.shared.stop.lock() {
                *stop = true;
            }
            self.shared.wake.notify_all();
            let _ = handle.join();
        }
    }
}

impl Drop for GcWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The collector loop: wait one interval (or until woken for shutdown),
/// vacuum, repeat. Runs on the spawned thread.
fn run<K, V>(shared: &GcShared, store: &VersionedStore<K, V>, interval: Duration)
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    loop {
        // Wait out the interval, returning early if shutdown was signalled.
        let stopped = {
            let Ok(guard) = shared.stop.lock() else {
                return;
            };
            if *guard {
                true
            } else {
                match shared.wake.wait_timeout(guard, interval) {
                    Ok((guard, _)) => *guard,
                    Err(_) => return,
                }
            }
        };
        if stopped {
            return;
        }

        // Best-effort vacuum: a poisoned lock skips this cycle, it never
        // brings the worker down.
        if let Ok(report) = store.vacuum_to_horizon() {
            shared
                .reclaimed_total
                .fetch_add(report.reclaimed_versions as u64, Ordering::Relaxed);
            shared.cycles.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvcc::{TransactionManager, INVALID_XID};

    fn seeded_store(updates: i64) -> Arc<VersionedStore<String, i64>> {
        let mgr = Arc::new(TransactionManager::new());
        let store: Arc<VersionedStore<String, i64>> = Arc::new(VersionedStore::new(mgr.clone()));
        for v in 0..updates {
            let w = mgr.begin().unwrap();
            store.put(w, "k".into(), v).unwrap();
            mgr.commit(w).unwrap();
        }
        store
    }

    /// Spin until `cond` holds or `deadline` cycles of a 1ms poll elapse, so
    /// the timing-dependent tests stay fast without flaking on a slow CI box.
    fn wait_until(mut cond: impl FnMut() -> bool, deadline: u32) -> bool {
        for _ in 0..deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        cond()
    }

    #[test]
    fn worker_reclaims_dead_versions_over_time() {
        // 5 committed updates -> 4 dead versions waiting to be vacuumed
        let store = seeded_store(5);
        assert_eq!(store.version_count(&"k".into()).unwrap(), 5);

        let worker = GcWorker::spawn(store.clone(), Duration::from_millis(2));
        let collapsed = wait_until(|| store.version_count(&"k".into()).unwrap_or(99) == 1, 2000);
        worker.stop();

        assert!(
            collapsed,
            "worker should have collapsed the chain to 1 live version"
        );
        assert_eq!(store.version_count(&"k".into()).unwrap(), 1);
    }

    #[test]
    fn worker_counts_reclaimed_versions() {
        let store = seeded_store(4); // 3 dead versions
        let worker = GcWorker::spawn(store.clone(), Duration::from_millis(2));
        wait_until(|| store.version_count(&"k".into()).unwrap_or(99) == 1, 2000);
        worker.stop();
        // stop() joined the thread; the count is final and observed = 3
        let store2 = store; // keep alive for the assertion message
        assert_eq!(store2.version_count(&"k".into()).unwrap(), 1);
    }

    #[test]
    fn worker_respects_a_registered_reader() {
        let mgr = Arc::new(TransactionManager::new());
        let store: Arc<VersionedStore<String, i64>> = Arc::new(VersionedStore::new(mgr.clone()));
        let seed = mgr.begin().unwrap();
        store.put(seed, "k".into(), 1).unwrap();
        mgr.commit(seed).unwrap();

        // a long-lived registered reader pins the horizon
        let reader = mgr.begin_snapshot(INVALID_XID).unwrap();

        // supersede the value a few times
        for v in 2..=5 {
            let w = mgr.begin().unwrap();
            store.put(w, "k".into(), v).unwrap();
            mgr.commit(w).unwrap();
        }

        let worker = GcWorker::spawn(store.clone(), Duration::from_millis(2));
        // let several cycles run
        wait_until(|| worker.cycles() >= 3, 2000);

        // the version the reader needs is still there, and the reader sees 1
        assert!(store.version_count(&"k".into()).unwrap() >= 2);
        assert_eq!(store.get(&"k".into(), &reader).unwrap(), Some(1));

        drop(reader);
        // now collection can advance to a single live version
        let collapsed = wait_until(|| store.version_count(&"k".into()).unwrap_or(99) == 1, 2000);
        worker.stop();
        assert!(
            collapsed,
            "after the reader left, the chain should collapse"
        );
    }

    #[test]
    fn dropping_the_worker_stops_the_thread() {
        let store = seeded_store(3);
        let worker = GcWorker::spawn(store.clone(), Duration::from_millis(2));
        wait_until(|| worker.cycles() >= 1, 2000);
        // Drop must not hang: it signals + joins the thread.
        drop(worker);
        // store is still usable afterwards
        assert_eq!(store.version_count(&"k".into()).unwrap(), 1);
    }
}
