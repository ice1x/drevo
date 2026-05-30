//! Error type for the MVCC layer.
//!
//! Phase 13 task `00081` keeps the multi-version concurrency primitives in
//! their own error channel rather than reaching straight for
//! [`crate::error::DrevoError`]. The MVCC engine is not yet wired into the
//! [`crate::db::Drevo`] mutation paths (that lands with the optimistic
//! concurrency control of task `00083` and the isolation levels of
//! `00084`), so a self-contained [`MvccError`] keeps this task's blast
//! radius to the new module and leaves the crate-wide error enum untouched.

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
}

/// Convenience alias for fallible MVCC operations.
pub type Result<T> = std::result::Result<T, MvccError>;
