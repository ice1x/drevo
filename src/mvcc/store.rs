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
use super::transaction::{Snapshot, TransactionManager, Xid};
use super::version::Version;

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
