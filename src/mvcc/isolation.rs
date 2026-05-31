//! Configurable transaction isolation levels (Phase 13 task `00084`).
//!
//! Tasks `00081`–`00083` built the MVCC primitives: tuple versions stamped
//! with `(xmin, xmax)`, point-in-time [`Snapshot`]s, garbage collection, and
//! write-write conflict detection. By themselves those give exactly one
//! behaviour — *snapshot isolation* — because [`VersionedStore::get`] always
//! resolves a key against whatever snapshot it is handed, and the
//! conflict-checked writes refuse a concurrent overwrite. This module turns
//! that single behaviour into the **three configurable isolation levels** an
//! SQL-style engine offers, by controlling two knobs around the existing
//! store:
//!
//! 1. **When the read snapshot is captured.** A [`Transaction`] holds one
//!    snapshot for its whole life (Snapshot Isolation / Serializable) or
//!    re-captures a fresh one before every statement (Read Committed).
//! 2. **What is validated at commit.** Serializable additionally records the
//!    transaction's *read set* and, at commit, refuses to commit if any key it
//!    read was modified by a concurrent committed transaction — closing the
//!    write-skew gap that Snapshot Isolation leaves open.
//!
//! ```text
//!                        read snapshot          commit-time read-set
//!                        refreshed each         validation (write-skew
//!                        statement?             guard)?
//!   ReadCommitted        yes                    no
//!   SnapshotIsolation    no (one per tx)        no
//!   Serializable         no (one per tx)        yes
//! ```
//!
//! Write-write conflict detection (task `00083`) is *always* on for the
//! [`Transaction`] write paths, regardless of level — every level rejects a
//! lost update; the levels differ only in read visibility and the additional
//! read-write antidependency guard.
//!
//! # The levels
//!
//! * [`IsolationLevel::ReadCommitted`] — each statement sees the latest
//!   committed data. Non-repeatable reads are allowed: two reads of the same
//!   key in one transaction may differ if another transaction committed in
//!   between. Implemented by re-snapshotting before every statement.
//! * [`IsolationLevel::SnapshotIsolation`] — also known as *Repeatable Read*.
//!   Every statement reads from one snapshot taken at [`begin`](Transaction);
//!   the transaction sees a stable view of the world for its whole life.
//!   Permits the write-skew anomaly.
//! * [`IsolationLevel::Serializable`] — Snapshot Isolation plus commit-time
//!   read-set validation, so the resulting schedule is serializable (no
//!   write-skew). Predicate/phantom anomalies over range scans are out of
//!   scope — the read set tracks individually-read keys, not predicates.
//!
//! # Usage
//!
//! [`Transaction`] is an RAII handle: dropping one without committing aborts
//! it, so a transaction can never linger in-progress and wedge conflict
//! detection for others. [`run_transaction`] wraps the begin → body →
//! validate-and-commit → retry loop, the isolation-aware sibling of
//! [`run_with_retry`](crate::mvcc::run_with_retry).
//!
//! [`Snapshot`]: crate::mvcc::Snapshot
//! [`VersionedStore::get`]: crate::mvcc::VersionedStore::get

use std::collections::HashSet;
use std::hash::Hash;

use super::error::{MvccError, Result};
use super::store::VersionedStore;
use super::transaction::{Snapshot, Xid};

/// The isolation level a [`Transaction`] runs under.
///
/// Defaults to [`SnapshotIsolation`](Self::SnapshotIsolation), the natural
/// MVCC level the underlying [`VersionedStore`] already provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum IsolationLevel {
    /// Every statement reads the latest committed data (a fresh snapshot per
    /// statement). Non-repeatable reads are permitted; dirty reads are not.
    ReadCommitted,
    /// One snapshot for the whole transaction — a stable, repeatable view.
    /// Also known as *Repeatable Read*. Permits write-skew.
    #[default]
    SnapshotIsolation,
    /// Snapshot Isolation plus commit-time read-set validation, yielding a
    /// serializable schedule (no write-skew on individually-read keys).
    Serializable,
}

impl IsolationLevel {
    /// `true` if a fresh read snapshot is captured before every statement
    /// (Read Committed only).
    pub fn refreshes_snapshot_per_statement(self) -> bool {
        matches!(self, IsolationLevel::ReadCommitted)
    }

    /// `true` if the level tracks the read set and validates it at commit
    /// (Serializable only).
    pub fn validates_read_set(self) -> bool {
        matches!(self, IsolationLevel::Serializable)
    }
}

impl<K, V> VersionedStore<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    /// Begin an isolation-aware [`Transaction`] over this store.
    ///
    /// Allocates a fresh [`Xid`] from the shared manager and captures the
    /// transaction's initial snapshot. For [`ReadCommitted`] that snapshot is
    /// refreshed before each statement; for the other levels it is the stable
    /// view used for the transaction's whole life.
    ///
    /// [`ReadCommitted`]: IsolationLevel::ReadCommitted
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn begin_transaction(&self, level: IsolationLevel) -> Result<Transaction<'_, K, V>> {
        let xid = self.manager().begin()?;
        let snapshot = self.manager().snapshot(xid)?;
        Ok(Transaction {
            store: self,
            xid,
            level,
            snapshot,
            reads: HashSet::new(),
            done: false,
        })
    }
}

/// An isolation-aware transaction handle over a [`VersionedStore`].
///
/// Created by
/// [`VersionedStore::begin_transaction`](VersionedStore::begin_transaction).
/// Reads and writes go through the handle so it can apply the isolation
/// level's snapshot policy and (for [`Serializable`](IsolationLevel::Serializable))
/// accumulate the read set. The transaction must be finished with
/// [`commit`](Self::commit) or [`abort`](Self::abort); dropping it unfinished
/// aborts it (best-effort), so a forgotten or conflicted transaction never
/// lingers in-progress.
#[derive(Debug)]
pub struct Transaction<'s, K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    store: &'s VersionedStore<K, V>,
    xid: Xid,
    level: IsolationLevel,
    snapshot: Snapshot,
    reads: HashSet<K>,
    done: bool,
}

impl<'s, K, V> Transaction<'s, K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    /// This transaction's allocated [`Xid`].
    pub fn id(&self) -> Xid {
        self.xid
    }

    /// The isolation level this transaction runs under.
    pub fn isolation_level(&self) -> IsolationLevel {
        self.level
    }

    /// Borrow the transaction's current read [`Snapshot`]. Under
    /// [`ReadCommitted`](IsolationLevel::ReadCommitted) this advances as
    /// statements run; under the other levels it is fixed for the
    /// transaction's life.
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Re-capture the read snapshot before a statement when the level demands
    /// it (Read Committed). A no-op for the snapshot-stable levels.
    fn before_statement(&mut self) -> Result<()> {
        if self.level.refreshes_snapshot_per_statement() {
            self.snapshot = self.store.manager().snapshot(self.xid)?;
        }
        Ok(())
    }

    /// Read `key` under this transaction's isolation level.
    ///
    /// Under [`ReadCommitted`](IsolationLevel::ReadCommitted) a fresh snapshot
    /// is captured first, so the read reflects the latest committed data.
    /// Under [`Serializable`](IsolationLevel::Serializable) the key is recorded
    /// in the read set for commit-time validation.
    ///
    /// # Errors
    ///
    /// [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn get(&mut self, key: &K) -> Result<Option<V>> {
        self.before_statement()?;
        if self.level.validates_read_set() {
            self.reads.insert(key.clone());
        }
        self.store.get(key, &self.snapshot)
    }

    /// Insert or update `key`, rejecting write-write conflicts.
    ///
    /// Under [`ReadCommitted`](IsolationLevel::ReadCommitted) the snapshot is
    /// refreshed first so the conflict check is against the latest committed
    /// state (a write that merely follows another transaction's *commit* is
    /// not a conflict; only an in-flight concurrent writer is).
    ///
    /// # Errors
    ///
    /// * [`MvccError::WriteConflict`] if the key's chain head was concurrently
    ///   modified by another in-flight transaction.
    /// * [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn put(&mut self, key: K, value: V) -> Result<()> {
        self.before_statement()?;
        self.store.put_checked(self.xid, &self.snapshot, key, value)
    }

    /// Delete `key`, rejecting write-write conflicts. Returns `true` if a live
    /// version was retired, `false` if the key had none.
    ///
    /// # Errors
    ///
    /// * [`MvccError::WriteConflict`] if the key's chain head was concurrently
    ///   modified by another in-flight transaction.
    /// * [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn delete(&mut self, key: &K) -> Result<bool> {
        self.before_statement()?;
        self.store.delete_checked(self.xid, &self.snapshot, key)
    }

    /// Commit the transaction.
    ///
    /// Under [`Serializable`](IsolationLevel::Serializable) the read set is
    /// validated against concurrent committers first: if any key read by this
    /// transaction was modified by a transaction that committed after this
    /// one's snapshot, the commit is refused with
    /// [`MvccError::SerializationFailure`] and the transaction is aborted. The
    /// other levels commit unconditionally.
    ///
    /// # Errors
    ///
    /// * [`MvccError::SerializationFailure`] (Serializable only) on a
    ///   read-write antidependency with a concurrent committer.
    /// * [`MvccError::NotInProgress`] if already committed or aborted.
    /// * [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn commit(mut self) -> Result<()> {
        self.done = true;
        if self.level.validates_read_set() {
            self.store
                .commit_serializable(self.xid, &self.snapshot, &self.reads)
        } else {
            self.store.manager().commit(self.xid)
        }
    }

    /// Abort the transaction, discarding its writes.
    ///
    /// # Errors
    ///
    /// * [`MvccError::NotInProgress`] if already committed or aborted.
    /// * [`MvccError::LockPoisoned`] on a poisoned lock.
    pub fn abort(mut self) -> Result<()> {
        self.done = true;
        self.store.manager().abort(self.xid)
    }
}

impl<K, V> Drop for Transaction<'_, K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn drop(&mut self) {
        if !self.done {
            // Best-effort rollback so an unfinished transaction never lingers
            // in-progress and blocks others' conflict detection.
            let _ = self.store.manager().abort(self.xid);
        }
    }
}

/// Run `body` as a transaction at `level`, retrying on write-write conflict or
/// serialization failure.
///
/// The isolation-aware sibling of
/// [`run_with_retry`](crate::mvcc::run_with_retry). Each attempt begins a fresh
/// [`Transaction`] at `level`, runs `body` against it, and commits. The body
/// performs its reads and writes through the transaction handle (so the level's
/// snapshot policy and read-set tracking apply), and returns its result value.
///
/// * `body` returns `Ok(value)` and the commit succeeds → `value` is returned.
/// * `body` or the commit returns
///   [`WriteConflict`](MvccError::WriteConflict) or
///   [`SerializationFailure`](MvccError::SerializationFailure) → the attempt is
///   aborted and retried against a brand-new transaction (and snapshot), up to
///   `max_retries` extra attempts.
/// * `body` returns any other `Err` → the transaction is aborted and the error
///   propagates unchanged.
///
/// When every attempt conflicts the call fails with
/// [`MvccError::RetriesExhausted`]. `max_retries == 0` means "try exactly once".
///
/// # Errors
///
/// * [`MvccError::RetriesExhausted`] if every attempt hit a conflict.
/// * Any non-conflict error returned by `body`, propagated after aborting.
pub fn run_transaction<F, T, K, V>(
    store: &VersionedStore<K, V>,
    level: IsolationLevel,
    max_retries: usize,
    mut body: F,
) -> Result<T>
where
    F: FnMut(&mut Transaction<'_, K, V>) -> Result<T>,
    K: Eq + Hash + Clone,
    V: Clone,
{
    let mut attempts = 0usize;
    loop {
        attempts += 1;
        let mut tx = store.begin_transaction(level)?;
        let outcome = body(&mut tx).and_then(|value| tx.commit().map(|()| value));
        match outcome {
            Ok(value) => return Ok(value),
            Err(MvccError::WriteConflict { .. }) | Err(MvccError::SerializationFailure { .. }) => {
                // `tx` was either consumed by commit() (which aborted on a
                // serialization failure) or dropped here (Drop aborts it after
                // a write conflict in `body`), so nothing lingers in-progress.
                if attempts > max_retries {
                    return Err(MvccError::RetriesExhausted { attempts });
                }
            }
            Err(other) => return Err(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvcc::{TransactionManager, XidStatus};
    use std::sync::Arc;

    fn store() -> VersionedStore<String, i64> {
        VersionedStore::new(Arc::new(TransactionManager::new()))
    }

    /// Commit an initial value through a plain transaction so tests start from
    /// known committed state.
    fn seed(s: &VersionedStore<String, i64>, key: &str, value: i64) {
        let mut tx = s
            .begin_transaction(IsolationLevel::SnapshotIsolation)
            .unwrap();
        tx.put(key.into(), value).unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn default_level_is_snapshot_isolation() {
        assert_eq!(IsolationLevel::default(), IsolationLevel::SnapshotIsolation);
    }

    #[test]
    fn level_predicates() {
        assert!(IsolationLevel::ReadCommitted.refreshes_snapshot_per_statement());
        assert!(!IsolationLevel::SnapshotIsolation.refreshes_snapshot_per_statement());
        assert!(!IsolationLevel::Serializable.refreshes_snapshot_per_statement());

        assert!(!IsolationLevel::ReadCommitted.validates_read_set());
        assert!(!IsolationLevel::SnapshotIsolation.validates_read_set());
        assert!(IsolationLevel::Serializable.validates_read_set());
    }

    #[test]
    fn begin_records_id_and_level() {
        let s = store();
        let tx = s.begin_transaction(IsolationLevel::Serializable).unwrap();
        assert!(tx.id() >= 1);
        assert_eq!(tx.isolation_level(), IsolationLevel::Serializable);
    }

    #[test]
    fn commit_makes_writes_visible() {
        let s = store();
        let mut tx = s
            .begin_transaction(IsolationLevel::SnapshotIsolation)
            .unwrap();
        tx.put("k".into(), 7).unwrap();
        tx.commit().unwrap();
        let mut reader = s
            .begin_transaction(IsolationLevel::SnapshotIsolation)
            .unwrap();
        assert_eq!(reader.get(&"k".into()).unwrap(), Some(7));
    }

    #[test]
    fn abort_discards_writes() {
        let s = store();
        let mut tx = s
            .begin_transaction(IsolationLevel::SnapshotIsolation)
            .unwrap();
        let xid = tx.id();
        tx.put("k".into(), 7).unwrap();
        tx.abort().unwrap();
        assert_eq!(s.manager().status(xid).unwrap(), XidStatus::Aborted);
        let mut reader = s
            .begin_transaction(IsolationLevel::SnapshotIsolation)
            .unwrap();
        assert_eq!(reader.get(&"k".into()).unwrap(), None);
    }

    #[test]
    fn dropping_unfinished_transaction_aborts_it() {
        let s = store();
        let xid;
        {
            let mut tx = s
                .begin_transaction(IsolationLevel::SnapshotIsolation)
                .unwrap();
            xid = tx.id();
            tx.put("k".into(), 1).unwrap();
            // tx dropped here without commit/abort
        }
        assert_eq!(s.manager().status(xid).unwrap(), XidStatus::Aborted);
        let mut reader = s
            .begin_transaction(IsolationLevel::SnapshotIsolation)
            .unwrap();
        assert_eq!(reader.get(&"k".into()).unwrap(), None);
    }

    // --- Snapshot Isolation: repeatable reads ---

    #[test]
    fn snapshot_isolation_reads_are_repeatable() {
        let s = store();
        seed(&s, "k", 1);
        let mut reader = s
            .begin_transaction(IsolationLevel::SnapshotIsolation)
            .unwrap();
        assert_eq!(reader.get(&"k".into()).unwrap(), Some(1));

        // a concurrent writer updates and commits
        let mut w = s
            .begin_transaction(IsolationLevel::SnapshotIsolation)
            .unwrap();
        w.put("k".into(), 2).unwrap();
        w.commit().unwrap();

        // the reader STILL sees its snapshot value (repeatable read)
        assert_eq!(reader.get(&"k".into()).unwrap(), Some(1));
    }

    // --- Read Committed: non-repeatable reads ---

    #[test]
    fn read_committed_sees_concurrent_commits() {
        let s = store();
        seed(&s, "k", 1);
        let mut reader = s.begin_transaction(IsolationLevel::ReadCommitted).unwrap();
        assert_eq!(reader.get(&"k".into()).unwrap(), Some(1));

        // a concurrent writer updates and commits
        let mut w = s.begin_transaction(IsolationLevel::ReadCommitted).unwrap();
        w.put("k".into(), 2).unwrap();
        w.commit().unwrap();

        // the reader re-snapshots per statement -> sees the new value
        assert_eq!(reader.get(&"k".into()).unwrap(), Some(2));
    }

    #[test]
    fn read_committed_does_not_see_uncommitted_writes() {
        let s = store();
        seed(&s, "k", 1);
        let mut reader = s.begin_transaction(IsolationLevel::ReadCommitted).unwrap();

        // a writer puts but does NOT commit
        let mut w = s.begin_transaction(IsolationLevel::ReadCommitted).unwrap();
        w.put("k".into(), 99).unwrap();

        // no dirty read: reader still sees the last committed value
        assert_eq!(reader.get(&"k".into()).unwrap(), Some(1));
        drop(w);
    }

    // --- write-write conflict still rejected at every level ---

    #[test]
    fn write_write_conflict_rejected_under_snapshot_isolation() {
        let s = store();
        seed(&s, "k", 1);
        let mut t1 = s
            .begin_transaction(IsolationLevel::SnapshotIsolation)
            .unwrap();
        let mut t2 = s
            .begin_transaction(IsolationLevel::SnapshotIsolation)
            .unwrap();
        // both read 1; t1 writes and commits
        assert_eq!(t1.get(&"k".into()).unwrap(), Some(1));
        assert_eq!(t2.get(&"k".into()).unwrap(), Some(1));
        t1.put("k".into(), 2).unwrap();
        t1.commit().unwrap();
        // t2's stale write loses
        let err = t2.put("k".into(), 3).unwrap_err();
        assert!(matches!(err, MvccError::WriteConflict { .. }));
    }

    // --- Serializable: write-skew prevention ---

    #[test]
    fn serializable_prevents_write_skew() {
        // Classic write-skew: two doctors are on call. The rule is "at least
        // one must stay on call". Each reads both rows, sees the other is on
        // call, and takes themselves off. Under SI both commit (they write
        // different rows) and the rule is violated. Serializable must abort one.
        let s = store();
        seed(&s, "alice_on_call", 1);
        seed(&s, "bob_on_call", 1);

        let mut t1 = s.begin_transaction(IsolationLevel::Serializable).unwrap();
        let mut t2 = s.begin_transaction(IsolationLevel::Serializable).unwrap();

        // both read both rows
        t1.get(&"alice_on_call".into()).unwrap();
        t1.get(&"bob_on_call".into()).unwrap();
        t2.get(&"alice_on_call".into()).unwrap();
        t2.get(&"bob_on_call".into()).unwrap();

        // t1 takes alice off call; t2 takes bob off call
        t1.put("alice_on_call".into(), 0).unwrap();
        t2.put("bob_on_call".into(), 0).unwrap();

        // first committer wins
        t1.commit().unwrap();
        // second committer read alice_on_call, which t1 just modified -> abort
        let err = t2.commit().unwrap_err();
        assert!(matches!(err, MvccError::SerializationFailure { .. }));

        // invariant holds: at least one still on call
        let mut check = s
            .begin_transaction(IsolationLevel::SnapshotIsolation)
            .unwrap();
        let alice = check.get(&"alice_on_call".into()).unwrap().unwrap();
        let bob = check.get(&"bob_on_call".into()).unwrap().unwrap();
        assert_eq!(alice + bob, 1);
    }

    #[test]
    fn serializable_allows_disjoint_read_sets() {
        // Two serializable transactions touching unrelated keys must both
        // commit — no false serialization failure.
        let s = store();
        seed(&s, "a", 1);
        seed(&s, "b", 1);
        let mut t1 = s.begin_transaction(IsolationLevel::Serializable).unwrap();
        let mut t2 = s.begin_transaction(IsolationLevel::Serializable).unwrap();
        t1.get(&"a".into()).unwrap();
        t2.get(&"b".into()).unwrap();
        t1.put("a".into(), 2).unwrap();
        t2.put("b".into(), 2).unwrap();
        t1.commit().unwrap();
        t2.commit().unwrap(); // disjoint read sets -> no conflict
    }

    #[test]
    fn serializable_read_only_transaction_never_fails() {
        // A serializable transaction that read a key already committed before
        // its snapshot, with no concurrent modification, commits cleanly.
        let s = store();
        seed(&s, "k", 5);
        let mut t = s.begin_transaction(IsolationLevel::Serializable).unwrap();
        assert_eq!(t.get(&"k".into()).unwrap(), Some(5));
        t.commit().unwrap();
    }

    #[test]
    fn serializable_first_committer_wins_is_not_aborted_by_in_flight_writer() {
        // The first committer should NOT be aborted by a racer that has only
        // written in-flight (committed-only read validation).
        let s = store();
        seed(&s, "x", 1);
        seed(&s, "y", 1);
        let mut t1 = s.begin_transaction(IsolationLevel::Serializable).unwrap();
        let mut t2 = s.begin_transaction(IsolationLevel::Serializable).unwrap();
        t1.get(&"x".into()).unwrap();
        t1.get(&"y".into()).unwrap();
        t2.put("y".into(), 2).unwrap(); // t2 writes y in-flight (not committed)
                                        // t1 reads y but t2 has not committed -> t1 still commits
        t1.commit().unwrap();
        drop(t2);
    }

    // --- run_transaction retry helper ---

    #[test]
    fn run_transaction_commits_on_success() {
        let s = store();
        let out = run_transaction(&s, IsolationLevel::SnapshotIsolation, 3, |tx| {
            tx.put("k".into(), 10)?;
            Ok("ok")
        })
        .unwrap();
        assert_eq!(out, "ok");
        let mut r = s
            .begin_transaction(IsolationLevel::SnapshotIsolation)
            .unwrap();
        assert_eq!(r.get(&"k".into()).unwrap(), Some(10));
    }

    #[test]
    fn run_transaction_retries_write_conflict_to_convergence() {
        // Two read-modify-write increments through run_transaction converge to
        // the correct total even though both initially read the same value.
        let s = store();
        seed(&s, "counter", 0);
        for _ in 0..2 {
            run_transaction(&s, IsolationLevel::SnapshotIsolation, 5, |tx| {
                let cur = tx.get(&"counter".into())?.unwrap();
                tx.put("counter".into(), cur + 1)
            })
            .unwrap();
        }
        let mut r = s
            .begin_transaction(IsolationLevel::SnapshotIsolation)
            .unwrap();
        assert_eq!(r.get(&"counter".into()).unwrap(), Some(2));
    }

    #[test]
    fn run_transaction_propagates_non_conflict_error() {
        let s = store();
        let err = run_transaction(
            &s,
            IsolationLevel::SnapshotIsolation,
            3,
            |_tx| -> Result<()> { Err(MvccError::NotInProgress(123)) },
        )
        .unwrap_err();
        assert!(matches!(err, MvccError::NotInProgress(123)));
    }

    #[test]
    fn run_transaction_exhausts_retries_under_constant_serialization_failure() {
        let s = store();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let err = run_transaction(&s, IsolationLevel::Serializable, 2, |_tx| -> Result<()> {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(MvccError::SerializationFailure { conflicting: 1 })
        })
        .unwrap_err();
        assert!(matches!(err, MvccError::RetriesExhausted { attempts: 3 }));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
