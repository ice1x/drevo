//! Transaction identifiers, status tracking, and point-in-time snapshots.
//!
//! This is the bookkeeping half of drevo's MVCC engine (Phase 13 task
//! `00081`). It models the same three pieces PostgreSQL uses:
//!
//! * a monotonic **transaction id** ([`Xid`]) allocator,
//! * a **commit log** mapping every allocated id to its [`XidStatus`]
//!   (in-progress / committed / aborted), and
//! * a **[`Snapshot`]** — the `(xmin, xmax, active-set)` triple captured at
//!   a point in time that decides, together with the commit log, which
//!   tuple versions a reader is allowed to see.
//!
//! Tuple storage and the visibility predicate live in the sibling
//! [`Version`](crate::mvcc::Version) and
//! [`VersionedStore`](crate::mvcc::VersionedStore) types; this module owns
//! only *who ran when and whether they committed*.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, RwLock};

use super::error::{MvccError, Result};

/// A monotonically increasing transaction identifier.
///
/// Real transactions start at `1`; [`INVALID_XID`] (`0`) is reserved as a
/// sentinel meaning "no transaction" — it is the `xmax` of a live tuple
/// version (one that has never been deleted) and is never handed out by
/// [`TransactionManager::begin`].
pub type Xid = u64;

/// The reserved sentinel transaction id (`0`).
///
/// Used as the `xmax` of a version that is still live and as the "no owner"
/// marker. [`TransactionManager::begin`] always returns a value `>= 1`, so a
/// real transaction can never collide with it.
pub const INVALID_XID: Xid = 0;

/// The commit-log state of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XidStatus {
    /// The transaction has begun but has neither committed nor aborted.
    /// Its writes are visible only to itself.
    InProgress,
    /// The transaction committed. Its writes are visible to every snapshot
    /// taken after it committed (and to snapshots that did not capture it
    /// as still-active).
    Committed,
    /// The transaction aborted (or was never recorded). Its writes are
    /// invisible to everyone.
    Aborted,
}

/// Allocates transaction ids and records their commit status.
///
/// Cloneable handles share the same underlying log when wrapped in an
/// `Arc`; the manager itself is `Send + Sync` so it can sit behind one.
///
/// # Concurrency
///
/// All mutation goes through a single [`RwLock`]. Allocation and
/// commit/abort take the write lock (short critical sections — a counter
/// bump and a map insert); [`status`](Self::status) and
/// [`snapshot`](Self::snapshot) take the read lock so many readers can
/// capture snapshots concurrently.
#[derive(Debug)]
pub struct TransactionManager {
    inner: RwLock<Registry>,
}

#[derive(Debug)]
struct Registry {
    /// The next id to hand out. All ids `>= next` are "in the future"
    /// relative to any snapshot taken now.
    next: Xid,
    /// Commit status of every id allocated so far. Entries below the
    /// global snapshot horizon could be pruned by the garbage collector
    /// (Phase 13 task `00082`); today they are retained.
    status: HashMap<Xid, XidStatus>,
    /// A multiset of the `xmin` of every *registered* live snapshot (one
    /// captured via [`begin_snapshot`](TransactionManager::begin_snapshot)),
    /// keyed by `xmin` with a reference count. The smallest key is the GC
    /// horizon: no version a registered reader can still see lies below it.
    /// Plain [`snapshot`](TransactionManager::snapshot)s are *not* tracked
    /// here — they are one-shot reads the collector need not respect.
    active_snaps: BTreeMap<Xid, u64>,
}

impl TransactionManager {
    /// Create an empty manager whose first allocated id will be `1`.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Registry {
                next: 1,
                status: HashMap::new(),
                active_snaps: BTreeMap::new(),
            }),
        }
    }

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, Registry>> {
        self.inner.write().map_err(|_| MvccError::LockPoisoned)
    }

    fn read(&self) -> Result<std::sync::RwLockReadGuard<'_, Registry>> {
        self.inner.read().map_err(|_| MvccError::LockPoisoned)
    }

    /// Begin a new transaction, returning its freshly allocated [`Xid`].
    ///
    /// The id is recorded as [`XidStatus::InProgress`] and will not be
    /// reused.
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] if the internal lock was poisoned by a
    /// panic in another thread.
    pub fn begin(&self) -> Result<Xid> {
        let mut reg = self.write()?;
        let xid = reg.next;
        reg.next += 1;
        reg.status.insert(xid, XidStatus::InProgress);
        Ok(xid)
    }

    /// Mark an in-progress transaction as committed.
    ///
    /// # Errors
    ///
    /// * [`MvccError::NotInProgress`] if `xid` is not currently in progress
    ///   (never allocated, or already finished).
    /// * [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn commit(&self, xid: Xid) -> Result<()> {
        self.finish(xid, XidStatus::Committed)
    }

    /// Mark an in-progress transaction as aborted.
    ///
    /// # Errors
    ///
    /// * [`MvccError::NotInProgress`] if `xid` is not currently in progress.
    /// * [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn abort(&self, xid: Xid) -> Result<()> {
        self.finish(xid, XidStatus::Aborted)
    }

    fn finish(&self, xid: Xid, to: XidStatus) -> Result<()> {
        let mut reg = self.write()?;
        match reg.status.get(&xid) {
            Some(XidStatus::InProgress) => {
                reg.status.insert(xid, to);
                Ok(())
            }
            _ => Err(MvccError::NotInProgress(xid)),
        }
    }

    /// Look up the commit status of a transaction id.
    ///
    /// An id that was never allocated reports [`XidStatus::Aborted`] — its
    /// (non-existent) writes must never be considered visible. [`INVALID_XID`]
    /// likewise reports `Aborted`.
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn status(&self, xid: Xid) -> Result<XidStatus> {
        if xid == INVALID_XID {
            return Ok(XidStatus::Aborted);
        }
        Ok(self
            .read()?
            .status
            .get(&xid)
            .copied()
            .unwrap_or(XidStatus::Aborted))
    }

    /// Capture a [`Snapshot`] of the transaction state as of *now*.
    ///
    /// `my_xid` is the id of the transaction on whose behalf the snapshot is
    /// taken (so it can see its own uncommitted writes); pass
    /// [`INVALID_XID`] for a read-only observer with no transaction of its
    /// own.
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn snapshot(&self, my_xid: Xid) -> Result<Snapshot> {
        let reg = self.read()?;
        let xmax = reg.next;
        let active: BTreeSet<Xid> = reg
            .status
            .iter()
            .filter(|(_, s)| matches!(s, XidStatus::InProgress))
            .map(|(x, _)| *x)
            .collect();
        let xmin = active.iter().copied().next().unwrap_or(xmax);
        Ok(Snapshot {
            xmin,
            xmax,
            active,
            my_xid,
        })
    }

    /// Capture a snapshot **and register it** so the garbage collector
    /// (Phase 13 task `00082`) will not reclaim any version this reader can
    /// still see.
    ///
    /// Returns a [`SnapshotGuard`] that publishes the snapshot's `xmin` into
    /// the manager's active set for as long as it lives and removes it again
    /// on drop. While any guard with a given `xmin` is alive,
    /// [`gc_horizon`](Self::gc_horizon) will not advance past it. Use this for
    /// long-lived readers; the cheaper untracked [`snapshot`](Self::snapshot)
    /// is fine for one-shot reads completed before the next vacuum.
    ///
    /// The receiver is `&Arc<Self>` so the returned guard can own a manager
    /// handle and therefore be `Send + 'static` — movable into reader threads.
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn begin_snapshot(self: &Arc<Self>, my_xid: Xid) -> Result<SnapshotGuard> {
        let snap = self.snapshot(my_xid)?;
        let xmin = snap.xmin;
        *self.write()?.active_snaps.entry(xmin).or_insert(0) += 1;
        Ok(SnapshotGuard {
            mgr: Arc::clone(self),
            snap,
            xmin,
        })
    }

    /// Drop one registration of a snapshot whose horizon was `xmin`. Called
    /// by [`SnapshotGuard`]'s destructor. A poisoned lock is swallowed: there
    /// is nothing a destructor can do about it, and over-retaining a horizon
    /// entry only delays GC, it never corrupts visibility.
    fn release_snapshot(&self, xmin: Xid) {
        if let Ok(mut reg) = self.write() {
            if let Some(count) = reg.active_snaps.get_mut(&xmin) {
                *count -= 1;
                if *count == 0 {
                    reg.active_snaps.remove(&xmin);
                }
            }
        }
    }

    /// The garbage-collection horizon: the oldest `xmin` of any registered
    /// live snapshot, or the next id to be allocated (one past the highest
    /// allocated id) when none are registered.
    ///
    /// Every version retired by a committed transaction strictly below this
    /// value is invisible to every registered reader and so may be reclaimed
    /// by the vacuum the
    /// [`VersionedStore::vacuum`](super::store::VersionedStore::vacuum)
    /// reclamation pass performs.
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn gc_horizon(&self) -> Result<Xid> {
        let reg = self.read()?;
        Ok(reg.active_snaps.keys().next().copied().unwrap_or(reg.next))
    }
}

/// An RAII handle to a registered [`Snapshot`].
///
/// Created by [`TransactionManager::begin_snapshot`]. It dereferences to the
/// underlying [`Snapshot`] (so it can be passed straight to
/// [`VersionedStore::get`](super::store::VersionedStore::get) via deref
/// coercion) and, while alive, pins the garbage-collection horizon at or
/// below its snapshot's `xmin`. Dropping it releases that pin.
#[derive(Debug)]
pub struct SnapshotGuard {
    mgr: Arc<TransactionManager>,
    snap: Snapshot,
    xmin: Xid,
}

impl SnapshotGuard {
    /// Borrow the underlying [`Snapshot`].
    pub fn snapshot(&self) -> &Snapshot {
        &self.snap
    }
}

impl std::ops::Deref for SnapshotGuard {
    type Target = Snapshot;

    fn deref(&self) -> &Snapshot {
        &self.snap
    }
}

impl Drop for SnapshotGuard {
    fn drop(&mut self) {
        self.mgr.release_snapshot(self.xmin);
    }
}

impl Default for TransactionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// A point-in-time view of which transactions had completed.
///
/// Captured by [`TransactionManager::snapshot`]. A snapshot plus the
/// manager's commit log is enough to decide, deterministically and
/// stably, whether any tuple version is visible — see
/// [`Snapshot::sees_effect`] and
/// [`Version::is_visible`](super::version::Version::is_visible).
///
/// The three fields are the classic PostgreSQL triple:
///
/// * `xmin` — the lowest still-active id. Every id `< xmin` had already
///   finished (committed or aborted) when the snapshot was taken.
/// * `xmax` — the next id to be allocated. Every id `>= xmax` started
///   *after* the snapshot and is therefore invisible.
/// * `active` — the ids that were in progress at snapshot time. They stay
///   invisible to this snapshot **even if they commit later**, which is
///   exactly what gives snapshot isolation its stable view.
#[derive(Debug, Clone)]
pub struct Snapshot {
    xmin: Xid,
    xmax: Xid,
    active: BTreeSet<Xid>,
    my_xid: Xid,
}

impl Snapshot {
    /// The snapshot horizon: every id strictly below this had finished
    /// when the snapshot was taken. The garbage collector (Phase 13 task
    /// `00082`) uses the minimum live `xmin` across all snapshots to decide
    /// which dead versions can be reclaimed.
    pub fn xmin(&self) -> Xid {
        self.xmin
    }

    /// One past the highest id that existed when the snapshot was taken.
    /// Any id `>= xmax` is in the snapshot's future and invisible.
    pub fn xmax(&self) -> Xid {
        self.xmax
    }

    /// The id of the transaction this snapshot belongs to, or
    /// [`INVALID_XID`] for a read-only observer.
    pub fn my_xid(&self) -> Xid {
        self.my_xid
    }

    /// Decide whether the *effect* of transaction `xid` (a row this
    /// transaction created or deleted) is visible under this snapshot.
    ///
    /// The rules, applied in order:
    ///
    /// 1. [`INVALID_XID`] has no effect — not visible.
    /// 2. The snapshot's own transaction always sees its own writes.
    /// 3. An id `>= xmax` started after the snapshot — not visible.
    /// 4. An id captured in the `active` set was in progress at snapshot
    ///    time — not visible (and stays so even after it commits).
    /// 5. Otherwise the id had completed before the snapshot; it is
    ///    visible iff the commit log records it [`XidStatus::Committed`].
    ///
    /// Steps 3–5 only ever consult the commit log for ids that had already
    /// finished at snapshot time, whose status is final — so the result is
    /// stable for the life of the snapshot regardless of later commits.
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] if the manager's lock is poisoned.
    pub fn sees_effect(&self, xid: Xid, mgr: &TransactionManager) -> Result<bool> {
        if xid == INVALID_XID {
            return Ok(false);
        }
        if xid == self.my_xid {
            return Ok(true);
        }
        if xid >= self.xmax {
            return Ok(false);
        }
        if self.active.contains(&xid) {
            return Ok(false);
        }
        Ok(matches!(mgr.status(xid)?, XidStatus::Committed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_xid_is_one_and_increases() {
        let m = TransactionManager::new();
        assert_eq!(m.begin().unwrap(), 1);
        assert_eq!(m.begin().unwrap(), 2);
        assert_eq!(m.begin().unwrap(), 3);
    }

    #[test]
    fn fresh_xid_is_in_progress() {
        let m = TransactionManager::new();
        let x = m.begin().unwrap();
        assert_eq!(m.status(x).unwrap(), XidStatus::InProgress);
    }

    #[test]
    fn commit_and_abort_record_terminal_status() {
        let m = TransactionManager::new();
        let a = m.begin().unwrap();
        let b = m.begin().unwrap();
        m.commit(a).unwrap();
        m.abort(b).unwrap();
        assert_eq!(m.status(a).unwrap(), XidStatus::Committed);
        assert_eq!(m.status(b).unwrap(), XidStatus::Aborted);
    }

    #[test]
    fn double_finish_is_rejected() {
        let m = TransactionManager::new();
        let x = m.begin().unwrap();
        m.commit(x).unwrap();
        assert!(matches!(m.commit(x), Err(MvccError::NotInProgress(_))));
        assert!(matches!(m.abort(x), Err(MvccError::NotInProgress(_))));
    }

    #[test]
    fn finishing_unknown_xid_is_rejected() {
        let m = TransactionManager::new();
        assert!(matches!(m.commit(999), Err(MvccError::NotInProgress(999))));
    }

    #[test]
    fn unknown_and_invalid_xid_report_aborted() {
        let m = TransactionManager::new();
        assert_eq!(m.status(INVALID_XID).unwrap(), XidStatus::Aborted);
        assert_eq!(m.status(12345).unwrap(), XidStatus::Aborted);
    }

    #[test]
    fn snapshot_captures_active_and_bounds() {
        let m = TransactionManager::new();
        let a = m.begin().unwrap(); // 1, stays active
        let b = m.begin().unwrap(); // 2, will commit
        m.commit(b).unwrap();
        let c = m.begin().unwrap(); // 3, stays active
        let snap = m.snapshot(INVALID_XID).unwrap();
        assert_eq!(snap.xmax(), 4); // next id after a,b,c
        assert_eq!(snap.xmin(), a); // lowest still-active is a==1
        assert!(snap.active.contains(&a));
        assert!(snap.active.contains(&c));
        assert!(!snap.active.contains(&b)); // committed -> not active
    }

    #[test]
    fn snapshot_xmin_equals_xmax_when_nothing_active() {
        let m = TransactionManager::new();
        let a = m.begin().unwrap();
        m.commit(a).unwrap();
        let snap = m.snapshot(INVALID_XID).unwrap();
        assert_eq!(snap.xmin(), snap.xmax());
    }

    #[test]
    fn sees_effect_invalid_xid_never_visible() {
        let m = TransactionManager::new();
        let snap = m.snapshot(INVALID_XID).unwrap();
        assert!(!snap.sees_effect(INVALID_XID, &m).unwrap());
    }

    #[test]
    fn sees_effect_own_writes_visible() {
        let m = TransactionManager::new();
        let me = m.begin().unwrap();
        let snap = m.snapshot(me).unwrap();
        // my own in-progress write is visible to me
        assert!(snap.sees_effect(me, &m).unwrap());
    }

    #[test]
    fn sees_effect_committed_before_snapshot_visible() {
        let m = TransactionManager::new();
        let w = m.begin().unwrap();
        m.commit(w).unwrap();
        let snap = m.snapshot(INVALID_XID).unwrap();
        assert!(snap.sees_effect(w, &m).unwrap());
    }

    #[test]
    fn sees_effect_active_at_snapshot_invisible_even_after_later_commit() {
        let m = TransactionManager::new();
        let w = m.begin().unwrap();
        let snap = m.snapshot(INVALID_XID).unwrap(); // w active here
        m.commit(w).unwrap(); // commits AFTER the snapshot
        assert!(!snap.sees_effect(w, &m).unwrap());
    }

    #[test]
    fn sees_effect_future_xid_invisible() {
        let m = TransactionManager::new();
        let snap = m.snapshot(INVALID_XID).unwrap(); // xmax == 1
        let future = m.begin().unwrap(); // 1, allocated after snapshot
        m.commit(future).unwrap();
        assert!(!snap.sees_effect(future, &m).unwrap());
    }

    #[test]
    fn sees_effect_aborted_invisible() {
        let m = TransactionManager::new();
        let w = m.begin().unwrap();
        m.abort(w).unwrap();
        let snap = m.snapshot(INVALID_XID).unwrap();
        assert!(!snap.sees_effect(w, &m).unwrap());
    }

    #[test]
    fn gc_horizon_is_next_when_no_snapshots_registered() {
        let m = TransactionManager::new();
        // nothing allocated yet -> next is 1
        assert_eq!(m.gc_horizon().unwrap(), 1);
        let a = m.begin().unwrap();
        m.commit(a).unwrap();
        // an untracked plain snapshot does NOT hold the horizon back
        let _plain = m.snapshot(INVALID_XID).unwrap();
        assert_eq!(m.gc_horizon().unwrap(), 2); // next id after a==1
    }

    #[test]
    fn registered_snapshot_pins_horizon_until_dropped() {
        let m = Arc::new(TransactionManager::new());
        // a long-running reader still in progress -> its xmin is the live tx
        let reader_tx = m.begin().unwrap(); // 1, stays active
        let guard = m.begin_snapshot(reader_tx).unwrap();
        // bump the clock with more committed work
        for _ in 0..5 {
            let w = m.begin().unwrap();
            m.commit(w).unwrap();
        }
        // horizon pinned at the registered reader's xmin (==1)
        assert_eq!(m.gc_horizon().unwrap(), reader_tx);
        drop(guard);
        // once released, the horizon jumps forward to `next`
        let next = m.snapshot(INVALID_XID).unwrap().xmax();
        assert_eq!(m.gc_horizon().unwrap(), next);
    }

    #[test]
    fn horizon_is_minimum_across_multiple_registered_snapshots() {
        let m = Arc::new(TransactionManager::new());
        // an old reader registers while tx 1 is the lowest active id
        let old_tx = m.begin().unwrap(); // 1, active
        let old = m.begin_snapshot(old_tx).unwrap();
        assert_eq!(old.xmin(), old_tx);
        // tx 1 finishes; a newer reader now captures a higher xmin
        m.commit(old_tx).unwrap();
        let mid_tx = m.begin().unwrap(); // 2, active
        let mid = m.begin_snapshot(mid_tx).unwrap();
        assert_eq!(mid.xmin(), mid_tx);

        assert_eq!(m.gc_horizon().unwrap(), old_tx); // smallest registered xmin wins
        drop(old);
        assert_eq!(m.gc_horizon().unwrap(), mid_tx); // now the next-oldest
        drop(mid);
    }

    #[test]
    fn duplicate_xmin_snapshots_refcount_correctly() {
        let m = Arc::new(TransactionManager::new());
        let reader_tx = m.begin().unwrap(); // 1, active -> both guards share xmin 1
        let g1 = m.begin_snapshot(reader_tx).unwrap();
        let g2 = m.begin_snapshot(reader_tx).unwrap();
        assert_eq!(m.gc_horizon().unwrap(), reader_tx);
        drop(g1);
        // one guard still holds xmin 1 -> horizon unchanged
        assert_eq!(m.gc_horizon().unwrap(), reader_tx);
        drop(g2);
        assert_eq!(
            m.gc_horizon().unwrap(),
            m.snapshot(INVALID_XID).unwrap().xmax()
        );
    }

    #[test]
    fn snapshot_guard_derefs_to_snapshot() {
        let m = Arc::new(TransactionManager::new());
        let w = m.begin().unwrap();
        m.commit(w).unwrap();
        let guard = m.begin_snapshot(INVALID_XID).unwrap();
        // deref gives access to Snapshot methods
        assert_eq!(guard.xmax(), guard.snapshot().xmax());
    }
}
