//! Error type for the MVCC layer.
//!
//! Phase 13 task `00081` keeps the multi-version concurrency primitives in
//! their own error channel rather than reaching straight for
//! [`crate::error::DrevoError`]. The MVCC engine is still a standalone module
//! — task `00083` adds the optimistic-concurrency-control conflict and retry
//! variants here, and full wiring into the [`crate::db::Drevo`] mutation
//! paths lands with the isolation levels of `00084`. A self-contained
//! [`MvccError`] keeps each task's blast radius to the new module and leaves
//! the crate-wide error enum untouched.

/// Errors raised by the MVCC transaction manager and versioned store.
#[derive(Debug, thiserror::Error)]
pub enum MvccError {
    /// A lock guarding MVCC state was poisoned by a panic in another
    /// thread while it held the lock in write mode.
    ///
    /// Mirrors [`crate::storage::StorageError::LockPoisoned`]: the engine
    /// surfaces the poison as a recoverable error instead of propagating
    /// the panic, so a single panicking writer cannot wedge the whole
    /// store.
    #[error("mvcc lock poisoned")]
    LockPoisoned,

    /// [`TransactionManager::commit`] / [`TransactionManager::abort`] was
    /// called for a transaction id that is not currently in progress —
    /// either it was never allocated, or it has already been committed or
    /// aborted (double-finish).
    ///
    /// [`TransactionManager::commit`]: super::TransactionManager::commit
    /// [`TransactionManager::abort`]: super::TransactionManager::abort
    #[error("transaction {0} is not in progress")]
    NotInProgress(super::Xid),

    /// A write-write conflict detected by the optimistic concurrency control
    /// of task `00083`: the writer tried to update or delete a key whose
    /// chain head had already been modified by a *concurrent* transaction
    /// (one still in progress, or one that committed after the writer's
    /// snapshot was taken). First-updater-wins — the second writer must abort
    /// and retry against a fresh snapshot rather than silently lose the
    /// concurrent update.
    ///
    /// `conflicting` is the id of the transaction that won the race.
    #[error("write-write conflict: key was concurrently modified by transaction {conflicting}")]
    WriteConflict {
        /// The concurrent transaction that already modified the chain head.
        conflicting: super::Xid,
    },

    /// A serialization failure detected by the **Serializable** isolation
    /// level of task `00084`: at commit time a key this transaction *read*
    /// was found to have been modified by another transaction that committed
    /// after this transaction's snapshot was taken. Snapshot Isolation alone
    /// would let this through as a write-skew anomaly; the serializable
    /// read-set validation refuses it (first-committer-wins) so the schedule
    /// stays serializable. The loser must abort and retry against a fresh
    /// snapshot — see [`run_transaction`](super::isolation::run_transaction).
    ///
    /// Distinct from [`WriteConflict`](Self::WriteConflict), which is a
    /// write-write race on a key this transaction *wrote*; this is a
    /// read-write antidependency on a key this transaction only *read*.
    ///
    /// `conflicting` is the id of the transaction that won the race.
    #[error(
        "serialization failure: a key read by this transaction was concurrently \
         modified by committed transaction {conflicting}"
    )]
    SerializationFailure {
        /// The concurrent transaction that modified a key in the read set.
        conflicting: super::Xid,
    },

    /// [`run_with_retry`](super::occ::run_with_retry) /
    /// [`run_transaction`](super::isolation::run_transaction) exhausted its
    /// retry budget: every attempt hit a
    /// [`WriteConflict`](Self::WriteConflict) or a
    /// [`SerializationFailure`](Self::SerializationFailure) under sustained
    /// contention. Carries how many attempts were made.
    #[error("optimistic retry gave up after {attempts} attempt(s) under contention")]
    RetriesExhausted {
        /// The total number of attempts made before giving up.
        attempts: usize,
    },
}

/// Convenience alias for fallible MVCC operations.
pub type Result<T> = std::result::Result<T, MvccError>;
