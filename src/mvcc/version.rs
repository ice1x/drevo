//! Tuple versioning — the `xmin` / `xmax` header carried by every stored
//! value.
//!
//! An MVCC store never overwrites or deletes in place. Instead each logical
//! key owns a *chain* of [`Version`]s, and every version is stamped with:
//!
//! * `xmin` — the id of the transaction that **created** it, and
//! * `xmax` — the id of the transaction that **superseded or deleted** it,
//!   or [`INVALID_XID`] while the version is still live.
//!
//! A reader then resolves a key by walking the chain newest-first and
//! returning the first version that is [`visible`](Version::is_visible)
//! under its [`Snapshot`]. Writers append; they never mutate value bytes.

use super::error::Result;
use super::transaction::{Snapshot, TransactionManager, Xid, INVALID_XID};

/// A single immutable version of a value, tagged with the transactions that
/// created and (optionally) retired it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version<V> {
    xmin: Xid,
    xmax: Xid,
    value: V,
}

impl<V> Version<V> {
    /// Create a live version authored by transaction `xmin`. Its `xmax` is
    /// [`INVALID_XID`] (not yet superseded).
    pub fn new(xmin: Xid, value: V) -> Self {
        Self {
            xmin,
            xmax: INVALID_XID,
            value,
        }
    }

    /// The id of the transaction that created this version.
    pub fn xmin(&self) -> Xid {
        self.xmin
    }

    /// The id of the transaction that superseded or deleted this version,
    /// or [`INVALID_XID`] if it is still live.
    pub fn xmax(&self) -> Xid {
        self.xmax
    }

    /// `true` if no transaction has yet stamped this version's `xmax`.
    /// A live version is the candidate a writer updates or deletes.
    pub fn is_live(&self) -> bool {
        self.xmax == INVALID_XID
    }

    /// Borrow the stored value.
    pub fn value(&self) -> &V {
        &self.value
    }

    /// Stamp this version as retired by transaction `xid` (sets `xmax`).
    /// Called by the store when an update or delete supersedes it.
    pub(super) fn mark_deleted(&mut self, xid: Xid) {
        self.xmax = xid;
    }

    /// Decide whether this version is visible under `snapshot`.
    ///
    /// A version is visible iff its creator's effect is visible **and** its
    /// deleter's effect is *not* — i.e. the row had been inserted, and not
    /// yet deleted, as far as this snapshot can tell:
    ///
    /// ```text
    /// visible = snapshot.sees_effect(xmin) && !snapshot.sees_effect(xmax)
    /// ```
    ///
    /// (`sees_effect(INVALID_XID)` is `false`, so a live version — `xmax ==
    /// INVALID_XID` — is visible whenever its creator is.)
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`](super::MvccError::LockPoisoned) if the
    /// manager's lock is poisoned.
    pub fn is_visible(&self, snapshot: &Snapshot, mgr: &TransactionManager) -> Result<bool> {
        if !snapshot.sees_effect(self.xmin, mgr)? {
            return Ok(false);
        }
        Ok(!snapshot.sees_effect(self.xmax, mgr)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_version_is_live() {
        let v = Version::new(1, "hello");
        assert!(v.is_live());
        assert_eq!(v.xmax(), INVALID_XID);
        assert_eq!(v.xmin(), 1);
        assert_eq!(*v.value(), "hello");
    }

    #[test]
    fn mark_deleted_sets_xmax_and_clears_live() {
        let mut v = Version::new(1, 42);
        v.mark_deleted(7);
        assert!(!v.is_live());
        assert_eq!(v.xmax(), 7);
    }

    #[test]
    fn live_version_visible_once_creator_commits() {
        let m = TransactionManager::new();
        let w = m.begin().unwrap();
        let v = Version::new(w, "x");
        // before commit: a fresh reader cannot see an in-progress writer
        let outside = m.snapshot(INVALID_XID).unwrap();
        assert!(!v.is_visible(&outside, &m).unwrap());
        // the writer sees its own write
        let mine = m.snapshot(w).unwrap();
        assert!(v.is_visible(&mine, &m).unwrap());
        // after commit a new snapshot sees it
        m.commit(w).unwrap();
        let after = m.snapshot(INVALID_XID).unwrap();
        assert!(v.is_visible(&after, &m).unwrap());
    }

    #[test]
    fn deleted_version_invisible_once_deleter_commits() {
        let m = TransactionManager::new();
        let creator = m.begin().unwrap();
        m.commit(creator).unwrap();
        let mut v = Version::new(creator, "x");
        let deleter = m.begin().unwrap();
        v.mark_deleted(deleter);
        // deleter still in progress: outside readers still see the row
        let mid = m.snapshot(INVALID_XID).unwrap();
        assert!(v.is_visible(&mid, &m).unwrap());
        // deleter sees its own delete (row gone for it)
        let deleter_snap = m.snapshot(deleter).unwrap();
        assert!(!v.is_visible(&deleter_snap, &m).unwrap());
        // after deleter commits, new snapshots no longer see the row
        m.commit(deleter).unwrap();
        let after = m.snapshot(INVALID_XID).unwrap();
        assert!(!v.is_visible(&after, &m).unwrap());
    }

    #[test]
    fn aborted_delete_keeps_version_visible() {
        let m = TransactionManager::new();
        let creator = m.begin().unwrap();
        m.commit(creator).unwrap();
        let mut v = Version::new(creator, "x");
        let deleter = m.begin().unwrap();
        v.mark_deleted(deleter);
        m.abort(deleter).unwrap(); // delete rolled back
        let after = m.snapshot(INVALID_XID).unwrap();
        assert!(v.is_visible(&after, &m).unwrap());
    }
}
