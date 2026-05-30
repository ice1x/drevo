//! A multi-version key-value store — the engine that ties transaction
//! snapshots and tuple versioning together.
//!
//! [`VersionedStore`] keeps, for every key, an append-only chain of
//! [`Version`]s. Writes never overwrite: an update retires the current live
//! version (stamps its `xmax`) and appends a fresh one; a delete only stamps
//! the `xmax`. Reads resolve a key against a [`Snapshot`], walking the chain
//! newest-first and returning the first visible version. The upshot is the
//! defining MVCC property — a reader holding an older snapshot keeps seeing
//! the data as of that snapshot even while writers move on, so **readers and
//! writers never see each other's in-flight state**.
//!
//! # Scope (Phase 13 task `00081`)
//!
//! This is the logical visibility engine. Two concurrent writers that retire
//! the *same* live version produce a lost update; detecting that as a
//! write-write conflict is the optimistic-concurrency-control work of task
//! `00083`. Reclaiming versions no live snapshot can reach is the garbage
//! collection of task `00082`. The physical index is guarded by a single
//! [`RwLock`](std::sync::RwLock); lock-free reads are a later performance
//! concern. What ships
//! here is correct snapshot-isolated visibility, which everything above
//! builds on.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, RwLock};

use super::error::{MvccError, Result};
use super::transaction::{Snapshot, TransactionManager, Xid, XidStatus, INVALID_XID};
use super::version::Version;

/// The outcome of a [`VersionedStore::vacuum`] pass.
///
/// Returned so callers (notably the background
/// [`GcWorker`](super::gc::GcWorker)) can observe how much dead state a cycle
/// reclaimed without instrumenting the store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VacuumReport {
    /// The number of physical [`Version`]s removed from version chains.
    pub reclaimed_versions: usize,
    /// The number of keys whose chains became empty and were dropped from the
    /// index entirely (their last visible version had been deleted).
    pub emptied_keys: usize,
}

/// An MVCC key-value store sharing a [`TransactionManager`].
///
/// `K` must be hashable; `V` is cloned out on read. Construct one over a
/// manager handle, then drive it with transaction ids obtained from that
/// same manager.
#[derive(Debug)]
pub struct VersionedStore<K, V> {
    mgr: Arc<TransactionManager>,
    chains: RwLock<HashMap<K, Vec<Version<V>>>>,
}

impl<K, V> VersionedStore<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    /// Create an empty store backed by `mgr`. Transaction ids passed to
    /// [`put`](Self::put) / [`delete`](Self::delete) and snapshots passed to
    /// [`get`](Self::get) must come from this manager.
    pub fn new(mgr: Arc<TransactionManager>) -> Self {
        Self {
            mgr,
            chains: RwLock::new(HashMap::new()),
        }
    }

    /// The transaction manager this store shares. Use it to
    /// [`begin`](TransactionManager::begin) writers and capture
    /// [`snapshot`](TransactionManager::snapshot)s for readers.
    pub fn manager(&self) -> &Arc<TransactionManager> {
        &self.mgr
    }

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, HashMap<K, Vec<Version<V>>>>> {
        self.chains.write().map_err(|_| MvccError::LockPoisoned)
    }

    fn read(&self) -> Result<std::sync::RwLockReadGuard<'_, HashMap<K, Vec<Version<V>>>>> {
        self.chains.read().map_err(|_| MvccError::LockPoisoned)
    }

    /// Insert or update `key` on behalf of transaction `xid`.
    ///
    /// The current live version of the key (if any) is stamped with `xmax =
    /// xid` and a new live version carrying `value` is appended. The new
    /// value becomes visible to other transactions only once `xid` commits;
    /// `xid` itself sees it immediately.
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn put(&self, xid: Xid, key: K, value: V) -> Result<()> {
        let mut chains = self.write()?;
        let chain = chains.entry(key).or_default();
        if let Some(live) = chain.iter_mut().rev().find(|v| v.is_live()) {
            live.mark_deleted(xid);
        }
        chain.push(Version::new(xid, value));
        Ok(())
    }

    /// Delete `key` on behalf of transaction `xid`.
    ///
    /// Stamps the current live version with `xmax = xid`. Returns `true` if a
    /// live version existed to retire, `false` if the key had no live version
    /// (already deleted or never present). The deletion becomes visible to
    /// other transactions only once `xid` commits.
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn delete(&self, xid: Xid, key: &K) -> Result<bool> {
        let mut chains = self.write()?;
        let Some(chain) = chains.get_mut(key) else {
            return Ok(false);
        };
        match chain.iter_mut().rev().find(|v| v.is_live()) {
            Some(live) => {
                live.mark_deleted(xid);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Decide whether a write by `writer` against `chain` collides with a
    /// concurrent transaction — the optimistic-concurrency-control check of
    /// task `00083`.
    ///
    /// Only the chain **head** (newest version) can carry a concurrent
    /// modification: a serial update retires the previous live version and
    /// appends a fresh one, so whoever last touched the key authored the
    /// head — either by creating it (the head is live) or by retiring it (the
    /// head is a tombstone). The "deciding" transaction is therefore the
    /// head's `xmin` when it is live, or its `xmax` when it has been deleted.
    ///
    /// That transaction conflicts with `writer` iff it is a *different*, still
    /// in-flight effect: not `writer` itself, not [`INVALID_XID`], not
    /// already visible to `writer`'s `snapshot` (a commit that landed before
    /// the snapshot is the row `writer` legitimately builds on, not a race),
    /// and not aborted (an aborted writer's mark is void). What survives all
    /// four filters is exactly an in-progress writer or one that committed
    /// *after* `writer`'s snapshot — the first-updater-wins loser.
    ///
    /// Returns `Some(xid)` naming the winner on conflict, or `None` when the
    /// write may proceed.
    fn conflicting_writer(
        &self,
        chain: &[Version<V>],
        writer: Xid,
        snapshot: &Snapshot,
    ) -> Result<Option<Xid>> {
        let Some(head) = chain.last() else {
            return Ok(None);
        };
        let decider = if head.is_live() {
            head.xmin()
        } else {
            head.xmax()
        };
        if decider == INVALID_XID || decider == writer {
            return Ok(None);
        }
        // A commit visible to our snapshot is the row we legitimately build
        // on, not a concurrent racer.
        if snapshot.sees_effect(decider, &self.mgr)? {
            return Ok(None);
        }
        // An aborted mark is void — the head is logically untouched by it.
        if matches!(self.mgr.status(decider)?, XidStatus::Aborted) {
            return Ok(None);
        }
        Ok(Some(decider))
    }

    /// Insert or update `key` on behalf of `xid`, **rejecting write-write
    /// conflicts** (task `00083`).
    ///
    /// Identical to [`put`](Self::put) except that, before mutating, it runs
    /// the optimistic-concurrency-control check against `xid`'s `snapshot`:
    /// if the key's chain head was concurrently modified by another in-flight
    /// transaction, the write is refused with
    /// [`MvccError::WriteConflict`] and the
    /// store is left untouched. The caller should abort `xid` and retry
    /// against a fresh snapshot — see
    /// [`run_with_retry`](crate::mvcc::run_with_retry).
    ///
    /// The check and the mutation happen under one acquisition of the store
    /// write lock, so two threads cannot both pass the check and then both
    /// append (no time-of-check/time-of-use gap).
    ///
    /// # Errors
    ///
    /// * [`MvccError::WriteConflict`] if the
    ///   chain head was concurrently modified.
    /// * [`MvccError::LockPoisoned`] on a
    ///   poisoned lock.
    pub fn put_checked(&self, xid: Xid, snapshot: &Snapshot, key: K, value: V) -> Result<()> {
        let mut chains = self.write()?;
        let chain = chains.entry(key).or_default();
        if let Some(winner) = self.conflicting_writer(chain, xid, snapshot)? {
            return Err(MvccError::WriteConflict {
                conflicting: winner,
            });
        }
        if let Some(live) = chain.iter_mut().rev().find(|v| v.is_live()) {
            live.mark_deleted(xid);
        }
        chain.push(Version::new(xid, value));
        Ok(())
    }

    /// Delete `key` on behalf of `xid`, **rejecting write-write conflicts**
    /// (task `00083`).
    ///
    /// Identical to [`delete`](Self::delete) except for the
    /// optimistic-concurrency-control check against `xid`'s `snapshot`: a
    /// concurrent modification of the chain head is refused with
    /// [`MvccError::WriteConflict`] and the
    /// store is left untouched. Returns `Ok(true)` if a live version was
    /// retired, `Ok(false)` if the key had no live version to delete. Like
    /// [`put_checked`](Self::put_checked), the check and the mutation share a
    /// single write-lock acquisition.
    ///
    /// # Errors
    ///
    /// * [`MvccError::WriteConflict`] if the
    ///   chain head was concurrently modified.
    /// * [`MvccError::LockPoisoned`] on a
    ///   poisoned lock.
    pub fn delete_checked(&self, xid: Xid, snapshot: &Snapshot, key: &K) -> Result<bool> {
        let mut chains = self.write()?;
        let Some(chain) = chains.get_mut(key) else {
            return Ok(false);
        };
        if let Some(winner) = self.conflicting_writer(chain, xid, snapshot)? {
            return Err(MvccError::WriteConflict {
                conflicting: winner,
            });
        }
        match chain.iter_mut().rev().find(|v| v.is_live()) {
            Some(live) => {
                live.mark_deleted(xid);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Read `key` as seen through `snapshot`.
    ///
    /// Walks the version chain newest-first and clones out the value of the
    /// first version visible to the snapshot, or `None` if the key has no
    /// version visible to it.
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn get(&self, key: &K, snapshot: &Snapshot) -> Result<Option<V>> {
        let chains = self.read()?;
        let Some(chain) = chains.get(key) else {
            return Ok(None);
        };
        for version in chain.iter().rev() {
            if version.is_visible(snapshot, &self.mgr)? {
                return Ok(Some(version.value().clone()));
            }
        }
        Ok(None)
    }

    /// All `(key, value)` pairs visible through `snapshot`, in arbitrary
    /// order. A convenience scan over [`get`](Self::get) for every key.
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn scan_visible(&self, snapshot: &Snapshot) -> Result<Vec<(K, V)>> {
        let chains = self.read()?;
        let mut out = Vec::new();
        for (key, chain) in chains.iter() {
            for version in chain.iter().rev() {
                if version.is_visible(snapshot, &self.mgr)? {
                    out.push((key.clone(), version.value().clone()));
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The number of physical versions retained for `key` (live plus dead),
    /// or `0` if the key is unknown. Exposed for the garbage collector
    /// (task `00082`) and for tests that assert chain growth.
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn version_count(&self, key: &K) -> Result<usize> {
        Ok(self.read()?.get(key).map_or(0, Vec::len))
    }

    /// Reclaim dead versions below `horizon`, vacuuming the store.
    ///
    /// Every chain is filtered by the per-version reclaimability rule:
    /// versions superseded or deleted by a committed transaction below the
    /// horizon, and versions created by an aborted transaction below it, are
    /// physically removed. A chain that becomes empty (its last live version
    /// was deleted and the delete is now below the horizon) is dropped from
    /// the index. Versions still reachable by some snapshot at or above the
    /// horizon — and every live version — are retained, so visibility is
    /// unchanged for any reader the horizon respects.
    ///
    /// `horizon` is normally [`TransactionManager::gc_horizon`]; pass a
    /// smaller value to vacuum more conservatively. The relative order of the
    /// surviving versions in each chain is preserved (newest stays last).
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn vacuum(&self, horizon: Xid) -> Result<VacuumReport> {
        let mut chains = self.write()?;
        let mut report = VacuumReport::default();
        let mut emptied: Vec<K> = Vec::new();

        for (key, chain) in chains.iter_mut() {
            let mut kept: Vec<Version<V>> = Vec::with_capacity(chain.len());
            for version in chain.drain(..) {
                if version.is_reclaimable(horizon, &self.mgr)? {
                    report.reclaimed_versions += 1;
                } else {
                    kept.push(version);
                }
            }
            if kept.is_empty() {
                emptied.push(key.clone());
            } else {
                *chain = kept;
            }
        }

        report.emptied_keys = emptied.len();
        for key in emptied {
            chains.remove(&key);
        }
        Ok(report)
    }

    /// Vacuum the store at the manager's current
    /// [`gc_horizon`](TransactionManager::gc_horizon) — the convenience
    /// entry point the background [`GcWorker`](super::gc::GcWorker) drives.
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn vacuum_to_horizon(&self) -> Result<VacuumReport> {
        let horizon = self.mgr.gc_horizon()?;
        self.vacuum(horizon)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvcc::INVALID_XID;

    fn store() -> VersionedStore<String, i64> {
        VersionedStore::new(Arc::new(TransactionManager::new()))
    }

    #[test]
    fn committed_write_is_visible_to_later_readers() {
        let s = store();
        let w = s.manager().begin().unwrap();
        s.put(w, "k".into(), 1).unwrap();
        s.manager().commit(w).unwrap();

        let snap = s.manager().snapshot(0).unwrap();
        assert_eq!(s.get(&"k".into(), &snap).unwrap(), Some(1));
    }

    #[test]
    fn uncommitted_write_is_hidden_from_other_readers() {
        let s = store();
        // a reader snapshot taken while w is in flight
        let w = s.manager().begin().unwrap();
        s.put(w, "k".into(), 1).unwrap();
        let other = s.manager().snapshot(0).unwrap();
        assert_eq!(s.get(&"k".into(), &other).unwrap(), None);
        // ...but the writer sees its own write
        let mine = s.manager().snapshot(w).unwrap();
        assert_eq!(s.get(&"k".into(), &mine).unwrap(), Some(1));
    }

    #[test]
    fn snapshot_is_stable_across_concurrent_commit() {
        let s = store();
        let w0 = s.manager().begin().unwrap();
        s.put(w0, "k".into(), 1).unwrap();
        s.manager().commit(w0).unwrap();

        // reader captures a snapshot seeing value 1
        let reader = s.manager().snapshot(0).unwrap();
        assert_eq!(s.get(&"k".into(), &reader).unwrap(), Some(1));

        // a second writer updates to 2 and commits
        let w1 = s.manager().begin().unwrap();
        s.put(w1, "k".into(), 2).unwrap();
        s.manager().commit(w1).unwrap();

        // the old reader STILL sees 1 (repeatable read)
        assert_eq!(s.get(&"k".into(), &reader).unwrap(), Some(1));
        // a fresh reader sees 2
        let fresh = s.manager().snapshot(0).unwrap();
        assert_eq!(s.get(&"k".into(), &fresh).unwrap(), Some(2));
    }

    #[test]
    fn update_retires_old_version_and_grows_chain() {
        let s = store();
        let w0 = s.manager().begin().unwrap();
        s.put(w0, "k".into(), 1).unwrap();
        s.manager().commit(w0).unwrap();
        let w1 = s.manager().begin().unwrap();
        s.put(w1, "k".into(), 2).unwrap();
        s.manager().commit(w1).unwrap();
        assert_eq!(s.version_count(&"k".into()).unwrap(), 2);
    }

    #[test]
    fn delete_hides_key_from_later_readers() {
        let s = store();
        let w0 = s.manager().begin().unwrap();
        s.put(w0, "k".into(), 1).unwrap();
        s.manager().commit(w0).unwrap();

        let d = s.manager().begin().unwrap();
        assert!(s.delete(d, &"k".into()).unwrap());
        s.manager().commit(d).unwrap();

        let snap = s.manager().snapshot(0).unwrap();
        assert_eq!(s.get(&"k".into(), &snap).unwrap(), None);
    }

    #[test]
    fn aborted_write_never_becomes_visible() {
        let s = store();
        let w = s.manager().begin().unwrap();
        s.put(w, "k".into(), 1).unwrap();
        s.manager().abort(w).unwrap();
        let snap = s.manager().snapshot(0).unwrap();
        assert_eq!(s.get(&"k".into(), &snap).unwrap(), None);
    }

    #[test]
    fn aborted_delete_leaves_value_visible() {
        let s = store();
        let w0 = s.manager().begin().unwrap();
        s.put(w0, "k".into(), 1).unwrap();
        s.manager().commit(w0).unwrap();
        let d = s.manager().begin().unwrap();
        s.delete(d, &"k".into()).unwrap();
        s.manager().abort(d).unwrap();
        let snap = s.manager().snapshot(0).unwrap();
        assert_eq!(s.get(&"k".into(), &snap).unwrap(), Some(1));
    }

    #[test]
    fn delete_missing_key_returns_false() {
        let s = store();
        let d = s.manager().begin().unwrap();
        assert!(!s.delete(d, &"nope".into()).unwrap());
    }

    #[test]
    fn scan_visible_returns_only_committed_live_rows() {
        let s = store();
        let w = s.manager().begin().unwrap();
        s.put(w, "a".into(), 1).unwrap();
        s.put(w, "b".into(), 2).unwrap();
        s.manager().commit(w).unwrap();
        // delete b, commit
        let d = s.manager().begin().unwrap();
        s.delete(d, &"b".into()).unwrap();
        s.manager().commit(d).unwrap();

        let snap = s.manager().snapshot(0).unwrap();
        let mut rows = s.scan_visible(&snap).unwrap();
        rows.sort();
        assert_eq!(rows, vec![("a".into(), 1)]);
    }

    #[test]
    fn vacuum_reclaims_superseded_versions_below_horizon() {
        let s = store();
        // three committed updates to the same key -> chain of 3
        for v in 1..=3 {
            let w = s.manager().begin().unwrap();
            s.put(w, "k".into(), v).unwrap();
            s.manager().commit(w).unwrap();
        }
        assert_eq!(s.version_count(&"k".into()).unwrap(), 3);

        // no readers registered -> horizon is `next`, all dead versions go
        let report = s.vacuum_to_horizon().unwrap();
        assert_eq!(report.reclaimed_versions, 2);
        assert_eq!(report.emptied_keys, 0);
        // only the live version survives
        assert_eq!(s.version_count(&"k".into()).unwrap(), 1);

        // and it is still the latest committed value
        let snap = s.manager().snapshot(0).unwrap();
        assert_eq!(s.get(&"k".into(), &snap).unwrap(), Some(3));
    }

    #[test]
    fn vacuum_preserves_versions_a_registered_reader_still_needs() {
        let s = store();
        let mgr = s.manager().clone();

        // seed value 1, committed
        let seed = mgr.begin().unwrap();
        s.put(seed, "k".into(), 1).unwrap();
        mgr.commit(seed).unwrap();

        // a long reader registers a snapshot seeing value 1
        let reader = mgr.begin_snapshot(INVALID_XID).unwrap();
        assert_eq!(s.get(&"k".into(), &reader).unwrap(), Some(1));

        // a writer supersedes it with value 2
        let w = mgr.begin().unwrap();
        s.put(w, "k".into(), 2).unwrap();
        mgr.commit(w).unwrap();

        // vacuum must NOT drop the old version: the registered reader sees it
        let report = s.vacuum_to_horizon().unwrap();
        assert_eq!(report.reclaimed_versions, 0);
        assert_eq!(s.version_count(&"k".into()).unwrap(), 2);
        assert_eq!(s.get(&"k".into(), &reader).unwrap(), Some(1));

        // once the reader leaves, the old version becomes collectable
        drop(reader);
        let report = s.vacuum_to_horizon().unwrap();
        assert_eq!(report.reclaimed_versions, 1);
        assert_eq!(s.version_count(&"k".into()).unwrap(), 1);
    }

    #[test]
    fn vacuum_drops_emptied_chain_after_committed_delete() {
        let s = store();
        let w = s.manager().begin().unwrap();
        s.put(w, "k".into(), 1).unwrap();
        s.manager().commit(w).unwrap();
        let d = s.manager().begin().unwrap();
        assert!(s.delete(d, &"k".into()).unwrap());
        s.manager().commit(d).unwrap();

        let report = s.vacuum_to_horizon().unwrap();
        assert_eq!(report.reclaimed_versions, 1);
        assert_eq!(report.emptied_keys, 1);
        assert_eq!(s.version_count(&"k".into()).unwrap(), 0);
    }

    #[test]
    fn vacuum_reclaims_aborted_writes() {
        let s = store();
        // committed live value
        let w = s.manager().begin().unwrap();
        s.put(w, "k".into(), 1).unwrap();
        s.manager().commit(w).unwrap();
        // an aborted update: retires the live version, appends a dead one
        let bad = s.manager().begin().unwrap();
        s.put(bad, "k".into(), 99).unwrap();
        s.manager().abort(bad).unwrap();
        assert_eq!(s.version_count(&"k".into()).unwrap(), 2);

        let report = s.vacuum_to_horizon().unwrap();
        // the aborted version is reclaimed; the original is restored to live
        // (its xmax points at an aborted tx, so it is not reclaimable)
        assert_eq!(report.reclaimed_versions, 1);
        assert_eq!(s.version_count(&"k".into()).unwrap(), 1);
        let snap = s.manager().snapshot(0).unwrap();
        assert_eq!(s.get(&"k".into(), &snap).unwrap(), Some(1));
    }

    // --- optimistic concurrency control (task `00083`) ---

    #[test]
    fn checked_put_on_fresh_key_succeeds() {
        let s = store();
        let w = s.manager().begin().unwrap();
        let snap = s.manager().snapshot(w).unwrap();
        assert!(s.put_checked(w, &snap, "k".into(), 1).is_ok());
    }

    #[test]
    fn serial_checked_updates_do_not_conflict() {
        let s = store();
        // first writer commits a value
        let w0 = s.manager().begin().unwrap();
        let snap0 = s.manager().snapshot(w0).unwrap();
        s.put_checked(w0, &snap0, "k".into(), 1).unwrap();
        s.manager().commit(w0).unwrap();
        // a later writer snapshots AFTER the commit -> sees it -> no conflict
        let w1 = s.manager().begin().unwrap();
        let snap1 = s.manager().snapshot(w1).unwrap();
        assert!(s.put_checked(w1, &snap1, "k".into(), 2).is_ok());
        s.manager().commit(w1).unwrap();
        assert_eq!(s.version_count(&"k".into()).unwrap(), 2);
    }

    #[test]
    fn concurrent_checked_update_conflicts_with_committed_winner() {
        let s = store();
        // seed a committed value
        let seed = s.manager().begin().unwrap();
        let seed_snap = s.manager().snapshot(seed).unwrap();
        s.put_checked(seed, &seed_snap, "k".into(), 1).unwrap();
        s.manager().commit(seed).unwrap();

        // T1 and T2 both snapshot the world seeing value 1
        let t1 = s.manager().begin().unwrap();
        let snap1 = s.manager().snapshot(t1).unwrap();
        let t2 = s.manager().begin().unwrap();
        let snap2 = s.manager().snapshot(t2).unwrap();

        // T1 updates and commits first
        s.put_checked(t1, &snap1, "k".into(), 2).unwrap();
        s.manager().commit(t1).unwrap();

        // T2's snapshot predates T1's commit -> it must lose
        let err = s.put_checked(t2, &snap2, "k".into(), 3).unwrap_err();
        assert!(matches!(err, MvccError::WriteConflict { conflicting } if conflicting == t1));
    }

    #[test]
    fn concurrent_checked_update_conflicts_with_in_progress_writer() {
        let s = store();
        let seed = s.manager().begin().unwrap();
        let seed_snap = s.manager().snapshot(seed).unwrap();
        s.put_checked(seed, &seed_snap, "k".into(), 1).unwrap();
        s.manager().commit(seed).unwrap();

        let t1 = s.manager().begin().unwrap();
        let snap1 = s.manager().snapshot(t1).unwrap();
        let t2 = s.manager().begin().unwrap();
        let snap2 = s.manager().snapshot(t2).unwrap();

        // T1 writes but has NOT committed; T2 still races it
        s.put_checked(t1, &snap1, "k".into(), 2).unwrap();
        let err = s.put_checked(t2, &snap2, "k".into(), 3).unwrap_err();
        assert!(matches!(err, MvccError::WriteConflict { conflicting } if conflicting == t1));
    }

    #[test]
    fn conflict_leaves_the_store_untouched() {
        let s = store();
        let seed = s.manager().begin().unwrap();
        let seed_snap = s.manager().snapshot(seed).unwrap();
        s.put_checked(seed, &seed_snap, "k".into(), 1).unwrap();
        s.manager().commit(seed).unwrap();

        let t1 = s.manager().begin().unwrap();
        let snap1 = s.manager().snapshot(t1).unwrap();
        let t2 = s.manager().begin().unwrap();
        let snap2 = s.manager().snapshot(t2).unwrap();
        s.put_checked(t1, &snap1, "k".into(), 2).unwrap();
        s.manager().commit(t1).unwrap();

        let before = s.version_count(&"k".into()).unwrap();
        let _ = s.put_checked(t2, &snap2, "k".into(), 3).unwrap_err();
        // the rejected write appended nothing and retired nothing
        assert_eq!(s.version_count(&"k".into()).unwrap(), before);
    }

    #[test]
    fn writer_may_update_its_own_uncommitted_write_without_conflict() {
        let s = store();
        let t = s.manager().begin().unwrap();
        let snap = s.manager().snapshot(t).unwrap();
        s.put_checked(t, &snap, "k".into(), 1).unwrap();
        // same transaction writes the key again -> the head is its own, no conflict
        assert!(s.put_checked(t, &snap, "k".into(), 2).is_ok());
    }

    #[test]
    fn aborted_concurrent_writer_does_not_conflict() {
        let s = store();
        let seed = s.manager().begin().unwrap();
        let seed_snap = s.manager().snapshot(seed).unwrap();
        s.put_checked(seed, &seed_snap, "k".into(), 1).unwrap();
        s.manager().commit(seed).unwrap();

        let t1 = s.manager().begin().unwrap();
        let snap1 = s.manager().snapshot(t1).unwrap();
        let t2 = s.manager().begin().unwrap();
        let snap2 = s.manager().snapshot(t2).unwrap();

        // T1 writes then ABORTS -> its mark on the head is void
        s.put_checked(t1, &snap1, "k".into(), 2).unwrap();
        s.manager().abort(t1).unwrap();

        // T2 may now proceed: the head's deciding tx is aborted
        assert!(s.put_checked(t2, &snap2, "k".into(), 3).is_ok());
    }

    #[test]
    fn checked_delete_detects_concurrent_update() {
        let s = store();
        let seed = s.manager().begin().unwrap();
        let seed_snap = s.manager().snapshot(seed).unwrap();
        s.put_checked(seed, &seed_snap, "k".into(), 1).unwrap();
        s.manager().commit(seed).unwrap();

        let t1 = s.manager().begin().unwrap();
        let snap1 = s.manager().snapshot(t1).unwrap();
        let t2 = s.manager().begin().unwrap();
        let snap2 = s.manager().snapshot(t2).unwrap();

        s.put_checked(t1, &snap1, "k".into(), 2).unwrap();
        s.manager().commit(t1).unwrap();

        // T2 tries to delete the row it saw as value 1 -> conflict
        let err = s.delete_checked(t2, &snap2, &"k".into()).unwrap_err();
        assert!(matches!(err, MvccError::WriteConflict { conflicting } if conflicting == t1));
    }

    #[test]
    fn concurrent_delete_then_update_conflicts() {
        let s = store();
        let seed = s.manager().begin().unwrap();
        let seed_snap = s.manager().snapshot(seed).unwrap();
        s.put_checked(seed, &seed_snap, "k".into(), 1).unwrap();
        s.manager().commit(seed).unwrap();

        let t1 = s.manager().begin().unwrap();
        let snap1 = s.manager().snapshot(t1).unwrap();
        let t2 = s.manager().begin().unwrap();
        let snap2 = s.manager().snapshot(t2).unwrap();

        // T1 deletes (head becomes a tombstone) and commits
        assert!(s.delete_checked(t1, &snap1, &"k".into()).unwrap());
        s.manager().commit(t1).unwrap();

        // T2's update races the committed delete -> conflict on the tombstone's xmax
        let err = s.put_checked(t2, &snap2, "k".into(), 9).unwrap_err();
        assert!(matches!(err, MvccError::WriteConflict { conflicting } if conflicting == t1));
    }

    #[test]
    fn checked_delete_of_missing_key_returns_false() {
        let s = store();
        let t = s.manager().begin().unwrap();
        let snap = s.manager().snapshot(t).unwrap();
        assert!(!s.delete_checked(t, &snap, &"nope".into()).unwrap());
    }

    #[test]
    fn vacuum_is_idempotent() {
        let s = store();
        for v in 1..=4 {
            let w = s.manager().begin().unwrap();
            s.put(w, "k".into(), v).unwrap();
            s.manager().commit(w).unwrap();
        }
        let first = s.vacuum_to_horizon().unwrap();
        assert_eq!(first.reclaimed_versions, 3);
        // a second pass finds nothing more to reclaim
        let second = s.vacuum_to_horizon().unwrap();
        assert_eq!(second.reclaimed_versions, 0);
        assert_eq!(second.emptied_keys, 0);
    }
}
