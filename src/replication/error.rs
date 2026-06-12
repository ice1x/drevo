//! The standalone error channel for the replication engine.
//!
//! Like the authorization engine (task `00094`) and the query planner
//! (`00085`–`00089`), replication keeps its own [`ReplicationError`] rather
//! than adding a variant to the crate-wide `DrevoError`: it is an opt-in
//! substrate not yet wired into the executor / HTTP / Bolt request path, so it
//! has no reason to widen the core error type's exhaustive match sites. Backend
//! failures encountered while replaying the log are wrapped via the
//! [`Storage`](ReplicationError::Storage) variant.

use crate::replication::wal::Lsn;
use crate::storage::StorageError;

/// A failure raised by the replication engine.
#[derive(Debug, thiserror::Error)]
pub enum ReplicationError {
    /// A write was attempted against a read-only [`Replica`](crate::replication::Replica).
    /// Writes must be directed at the MAIN [`Primary`](crate::replication::Primary).
    #[error("replica is read-only; writes must be directed at the MAIN node")]
    ReadOnly,

    /// A record arrived out of order: applying it would skip one or more
    /// sequence numbers. The replica must request the missing records before
    /// this one can be applied.
    #[error("log sequence gap: replica applied up to LSN {applied}, expected LSN {expected} next, but received LSN {got}")]
    LsnGap {
        /// The highest [`Lsn`] the replica had applied.
        applied: Lsn,
        /// The [`Lsn`] the replica expected next (`applied + 1`).
        expected: Lsn,
        /// The (too-high) [`Lsn`] that was received.
        got: Lsn,
    },

    /// The replica has fallen behind the MAIN node's retained log: a
    /// checkpoint dropped records it had not yet applied, so it can no longer
    /// be served incrementally and must perform a full resync.
    #[error("replica at LSN {requested} is behind the retained log (earliest retained: {earliest:?}); a full resync is required")]
    Truncated {
        /// The [`Lsn`] the replica asked to stream from.
        requested: Lsn,
        /// The earliest [`Lsn`] still retained by the MAIN node, or `None` if
        /// the log is empty.
        earliest: Option<Lsn>,
    },

    /// The storage backend rejected a mutation while the replica was replaying
    /// the log.
    #[error("storage error while applying replicated mutation: {0}")]
    Storage(#[from] StorageError),
}

/// Convenience alias for results from the replication engine.
pub type Result<T> = core::result::Result<T, ReplicationError>;
