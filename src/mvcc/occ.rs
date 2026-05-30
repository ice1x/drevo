//! Optimistic concurrency control — retry semantics (Phase 13 task `00083`).
//!
//! Detection lives one layer down: [`VersionedStore::put_checked`] and
//! [`VersionedStore::delete_checked`] refuse a write whose key was
//! concurrently modified, returning
//! [`MvccError::WriteConflict`]. This module
//! supplies the other half OCC needs — the *retry loop* that turns a detected
//! conflict into forward progress instead of a propagated error.
//!
//! [`run_with_retry`] runs a transaction body optimistically: it begins a
//! transaction, captures a fresh snapshot, and runs the body. If the body
//! returns a [`WriteConflict`](crate::mvcc::MvccError::WriteConflict) the
//! transaction is aborted and the whole thing is retried against a brand-new
//! snapshot — which now observes the commit that won the previous race, so the
//! next attempt builds on it rather than colliding again. A body that succeeds
//! is committed; any other error aborts and propagates unchanged. After
//! `max_retries` failed attempts the loop gives up with
//! [`MvccError::RetriesExhausted`].
//!
//! This is the classic first-updater-wins discipline: under contention one
//! writer commits and the rest re-derive their changes from the new state.
//! Because each retry re-snapshots, a workload of independent increments
//! converges to the correct total even when every writer initially raced off
//! the same starting value.
//!
//! [`VersionedStore::put_checked`]: crate::mvcc::VersionedStore::put_checked
//! [`VersionedStore::delete_checked`]: crate::mvcc::VersionedStore::delete_checked

use std::sync::Arc;

use super::error::{MvccError, Result};
use super::transaction::{Snapshot, TransactionManager, Xid};

/// Run `body` as an optimistic transaction, retrying on write-write conflict.
///
/// Each attempt allocates a fresh [`Xid`] from `mgr`, captures a snapshot for
/// it, and invokes `body(xid, &snapshot)`. The body performs its reads and
/// conflict-checked writes (via
/// [`put_checked`](crate::mvcc::VersionedStore::put_checked) /
/// [`delete_checked`](crate::mvcc::VersionedStore::delete_checked)) using that
/// `xid` and `snapshot`, and returns its result value.
///
/// * The body returns `Ok(value)` → the transaction is **committed** and
///   `value` is returned.
/// * The body returns
///   [`Err(WriteConflict)`](crate::mvcc::MvccError::WriteConflict) → the transaction
///   is **aborted** and the attempt is retried against a new snapshot, up to
///   `max_retries` extra attempts (so `max_retries + 1` attempts total).
/// * The body returns any other `Err` → the transaction is aborted and the
///   error propagates unchanged.
///
/// When every attempt conflicts the call fails with
/// [`MvccError::RetriesExhausted`]
/// carrying the attempt count. `max_retries == 0` means "try exactly once,
/// never retry".
///
/// # Errors
///
/// * [`MvccError::RetriesExhausted`] if
///   every attempt hit a conflict.
/// * Any non-conflict error returned by `body` (including
///   [`MvccError::LockPoisoned`]), propagated
///   after aborting the in-flight transaction.
pub fn run_with_retry<F, T>(
    mgr: &Arc<TransactionManager>,
    max_retries: usize,
    mut body: F,
) -> Result<T>
where
    F: FnMut(Xid, &Snapshot) -> Result<T>,
{
    let mut attempts = 0usize;
    loop {
        attempts += 1;
        let xid = mgr.begin()?;
        let snapshot = mgr.snapshot(xid)?;
        match body(xid, &snapshot) {
            Ok(value) => {
                mgr.commit(xid)?;
                return Ok(value);
            }
            Err(MvccError::WriteConflict { .. }) => {
                // Roll back this doomed attempt so it never becomes visible,
                // then loop to re-snapshot against the winner's commit.
                mgr.abort(xid)?;
                if attempts > max_retries {
                    return Err(MvccError::RetriesExhausted { attempts });
                }
            }
            Err(other) => {
                // Best-effort rollback; surface the original cause regardless.
                let _ = mgr.abort(xid);
                return Err(other);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvcc::{TransactionManager, VersionedStore};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn setup() -> (Arc<TransactionManager>, VersionedStore<String, i64>) {
        let mgr = Arc::new(TransactionManager::new());
        let store = VersionedStore::new(mgr.clone());
        (mgr, store)
    }

    #[test]
    fn body_that_succeeds_commits_and_returns_value() {
        let (mgr, store) = setup();
        let out = run_with_retry(&mgr, 3, |xid, snap| {
            store.put_checked(xid, snap, "k".into(), 42)?;
            Ok("done")
        })
        .unwrap();
        assert_eq!(out, "done");
        // committed -> visible to a fresh reader
        let snap = mgr.snapshot(0).unwrap();
        assert_eq!(store.get(&"k".into(), &snap).unwrap(), Some(42));
    }

    #[test]
    fn body_runs_exactly_once_when_no_conflict() {
        let (mgr, store) = setup();
        let calls = AtomicUsize::new(0);
        run_with_retry(&mgr, 5, |xid, snap| {
            calls.fetch_add(1, Ordering::SeqCst);
            store.put_checked(xid, snap, "k".into(), 1)
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn non_conflict_error_propagates_and_aborts() {
        let (mgr, _store) = setup();
        let captured_xid = AtomicUsize::new(0);
        let err = run_with_retry(&mgr, 5, |xid, _snap| -> Result<()> {
            captured_xid.store(xid as usize, Ordering::SeqCst);
            Err(MvccError::NotInProgress(999))
        })
        .unwrap_err();
        assert!(matches!(err, MvccError::NotInProgress(999)));
        // the attempt's xid was aborted, not left in progress
        let xid = captured_xid.load(Ordering::SeqCst) as Xid;
        assert_eq!(mgr.status(xid).unwrap(), crate::mvcc::XidStatus::Aborted);
    }

    #[test]
    fn retries_until_body_stops_conflicting() {
        let (mgr, _store) = setup();
        // First two attempts simulate a conflict, the third succeeds.
        let attempt = AtomicUsize::new(0);
        let total = run_with_retry(&mgr, 5, |_xid, _snap| {
            let n = attempt.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err(MvccError::WriteConflict { conflicting: 1 })
            } else {
                Ok(n)
            }
        })
        .unwrap();
        assert_eq!(total, 2); // succeeded on the third attempt (index 2)
        assert_eq!(attempt.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn exhausting_retries_reports_attempt_count() {
        let (mgr, _store) = setup();
        let calls = AtomicUsize::new(0);
        let err = run_with_retry(&mgr, 2, |_xid, _snap| -> Result<()> {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(MvccError::WriteConflict { conflicting: 7 })
        })
        .unwrap_err();
        // max_retries == 2 -> 3 attempts total (initial + 2 retries)
        assert!(matches!(err, MvccError::RetriesExhausted { attempts: 3 }));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn zero_retries_tries_exactly_once() {
        let (mgr, _store) = setup();
        let calls = AtomicUsize::new(0);
        let err = run_with_retry(&mgr, 0, |_xid, _snap| -> Result<()> {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(MvccError::WriteConflict { conflicting: 1 })
        })
        .unwrap_err();
        assert!(matches!(err, MvccError::RetriesExhausted { attempts: 1 }));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn each_retry_observes_the_previous_winner() {
        // A real lost-update guard: two bodies both read the same start value
        // and increment. The retry must re-read the committed result so the
        // final value reflects BOTH increments, not just one.
        let (mgr, store) = setup();
        let seed = mgr.begin().unwrap();
        store.put(seed, "counter".into(), 0).unwrap();
        mgr.commit(seed).unwrap();

        // T1 reads 0, writes 1, commits.
        run_with_retry(&mgr, 3, |xid, snap| {
            let cur = store.get(&"counter".into(), snap).unwrap().unwrap();
            store.put_checked(xid, snap, "counter".into(), cur + 1)
        })
        .unwrap();
        // T2 reads (now 1), writes 2, commits.
        run_with_retry(&mgr, 3, |xid, snap| {
            let cur = store.get(&"counter".into(), snap).unwrap().unwrap();
            store.put_checked(xid, snap, "counter".into(), cur + 1)
        })
        .unwrap();

        let snap = mgr.snapshot(0).unwrap();
        assert_eq!(store.get(&"counter".into(), &snap).unwrap(), Some(2));
    }
}
