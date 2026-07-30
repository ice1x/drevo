//! Multi-version concurrency control (MVCC) — Phase 13 task `00081`.
//!
//! This module is the foundation of drevo's concurrency story: the point
//! where readers stop blocking writers *logically*, not just physically.
//! Task `00080` (read-write separation) let many readers share the storage
//! lock; this task adds the visibility machinery that lets a reader and a
//! writer touch the *same* key at the same time and each see a consistent
//! world.
//!
//! It models MVCC the way PostgreSQL does, in three cooperating pieces:
//!
//! * [`TransactionManager`](crate::mvcc::TransactionManager) — allocates
//!   monotonically increasing [`Xid`](crate::mvcc::Xid)s, records each one's
//!   [`XidStatus`](crate::mvcc::XidStatus) (in-progress / committed /
//!   aborted) in a commit log, and captures point-in-time
//!   [`Snapshot`](crate::mvcc::Snapshot)s.
//! * [`Version`](crate::mvcc::Version) — an immutable value stamped with the
//!   `xmin` that created it and the `xmax` that retired it. Storage is
//!   append-only: an update retires the old version and appends a new one;
//!   nothing is mutated in place.
//! * [`VersionedStore`](crate::mvcc::VersionedStore) — a key → version-chain
//!   map whose [`get`] resolves a key against a snapshot and whose [`put`] /
//!   [`delete`] append versions on behalf of a transaction.
//!
//! [`get`]: crate::mvcc::VersionedStore::get
//! [`put`]: crate::mvcc::VersionedStore::put
//! [`delete`]: crate::mvcc::VersionedStore::delete
//!
//! # The visibility rule
//!
//! Every read reduces to one predicate — is a version visible to a
//! snapshot? A version is visible iff the transaction that **created** it is
//! visible to the snapshot and the transaction that **deleted** it is not:
//!
//! ```text
//! visible(v, snap) = snap.sees_effect(v.xmin) && !snap.sees_effect(v.xmax)
//! ```
//!
//! and an effect (`sees_effect`) is visible iff the transaction is the
//! snapshot's own, or it had committed before the snapshot was taken and was
//! not still in progress at that moment. A transaction that was active when
//! the snapshot was captured stays invisible to it forever, which is what
//! makes the view stable (repeatable reads) under concurrent commits.
//!
//! # Garbage collection (task `00082`)
//!
//! Append-only storage means dead versions accumulate forever unless they
//! are reclaimed. A reader registers its snapshot with
//! [`TransactionManager::begin_snapshot`](crate::mvcc::TransactionManager::begin_snapshot),
//! which publishes the snapshot's `xmin` and hands back a
//! [`SnapshotGuard`](crate::mvcc::SnapshotGuard); the oldest registered
//! `xmin` is the [`gc_horizon`](crate::mvcc::TransactionManager::gc_horizon).
//! [`VersionedStore::vacuum`](crate::mvcc::VersionedStore::vacuum) physically
//! removes every version that is invisible to all readers at or above that
//! horizon — versions deleted/superseded by a committed transaction below it,
//! and versions created by an aborted transaction below it — while preserving
//! every live version and anything a registered reader can still see. The
//! background [`GcWorker`](crate::mvcc::GcWorker) thread runs that vacuum on a
//! cadence (native targets only; WASM hosts vacuum manually).
//!
//! # Optimistic concurrency control (task `00083`)
//!
//! Append-only writes do not by themselves stop two concurrent transactions
//! from each retiring the *same* live version and producing a lost update.
//! [`VersionedStore::put_checked`](crate::mvcc::VersionedStore::put_checked)
//! and [`delete_checked`](crate::mvcc::VersionedStore::delete_checked) detect
//! that race: a write whose key's chain head was concurrently modified is
//! refused with
//! [`MvccError::WriteConflict`](crate::mvcc::MvccError::WriteConflict)
//! (first-updater-wins). [`run_with_retry`](crate::mvcc::run_with_retry) wraps
//! the discipline into forward progress — it re-snapshots and replays the
//! transaction body until it commits or the retry budget is exhausted.
//!
//! # Configurable isolation levels (task `00084`)
//!
//! The primitives above give exactly one behaviour — snapshot isolation. The
//! [`isolation`](crate::mvcc::isolation) module turns that into the three configurable
//! [`IsolationLevel`](crate::mvcc::IsolationLevel)s an SQL-style engine offers,
//! by controlling *when* a transaction's read snapshot is captured and *what*
//! is validated at commit:
//!
//! * [`ReadCommitted`](crate::mvcc::IsolationLevel::ReadCommitted) re-snapshots
//!   before every statement (non-repeatable reads, latest committed data);
//! * [`SnapshotIsolation`](crate::mvcc::IsolationLevel::SnapshotIsolation)
//!   holds one snapshot for the transaction's life (repeatable reads);
//! * [`Serializable`](crate::mvcc::IsolationLevel::Serializable) adds
//!   commit-time read-set validation that refuses to commit when a key the
//!   transaction read was modified by a concurrent committer
//!   ([`MvccError::SerializationFailure`](crate::mvcc::MvccError::SerializationFailure)),
//!   closing the write-skew gap.
//!
//! [`Transaction`](crate::mvcc::Transaction) is the RAII handle that applies a
//! level's policy; [`run_transaction`](crate::mvcc::run_transaction) wraps the
//! begin → body → commit → retry loop. Write-write conflict detection (task
//! `00083`) is always on at every level.
//!
//! # What lands later in Phase 13
//!
//! * Wiring the standalone MVCC engine into the [`Drevo`](crate::db::Drevo)
//!   redb-backed mutation paths — the module remains a self-contained engine,
//!   as it has since `00081`.
//!
//! These all build directly on the primitives defined here.

/// Recoverable error type for the MVCC layer ([`MvccError`]).
pub mod error;
/// Background garbage collection thread ([`GcWorker`]).
#[cfg(not(target_arch = "wasm32"))]
pub mod gc;
/// Configurable transaction isolation levels
/// ([`IsolationLevel`], [`Transaction`], [`run_transaction`]).
pub mod isolation;
/// Optimistic concurrency control retry loop ([`run_with_retry`]).
pub mod occ;
/// Multi-version key-value store ([`VersionedStore`], [`VacuumReport`]).
pub mod store;
/// Transaction ids, the commit log, and snapshots
/// ([`TransactionManager`], [`Snapshot`], [`Xid`], [`XidStatus`]).
pub mod transaction;
/// Tuple versioning — the `xmin` / `xmax`-stamped [`Version`].
pub mod version;

pub use error::{MvccError, Result};
#[cfg(not(target_arch = "wasm32"))]
pub use gc::GcWorker;
pub use isolation::{run_transaction, IsolationLevel, Transaction};
pub use occ::run_with_retry;
pub use store::{VacuumReport, VersionedStore};
pub use transaction::{Snapshot, SnapshotGuard, TransactionManager, Xid, XidStatus, INVALID_XID};
pub use version::Version;
