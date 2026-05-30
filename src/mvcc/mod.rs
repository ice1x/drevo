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
//! # What lands later in Phase 13
//!
//! * **`00082` garbage collection** — reclaim versions older than the
//!   oldest live snapshot's
//!   [`Snapshot::xmin`](crate::mvcc::Snapshot::xmin) horizon.
//! * **`00083` optimistic concurrency control** — detect two writers
//!   retiring the same live version (a write-write conflict) and force a
//!   retry instead of silently losing an update.
//! * **`00084` isolation levels** — choose between re-snapshotting per
//!   statement (Read Committed) and once per transaction (Snapshot
//!   Isolation), plus a serializable check.
//!
//! These all build directly on the primitives defined here; this task is
//! deliberately the visibility core and nothing more.

/// Recoverable error type for the MVCC layer
/// ([`MvccError`](crate::mvcc::MvccError)).
pub mod error;
/// Multi-version key-value store
/// ([`VersionedStore`](crate::mvcc::VersionedStore)).
pub mod store;
/// Transaction ids, the commit log, and snapshots
/// ([`TransactionManager`](crate::mvcc::TransactionManager),
/// [`Snapshot`](crate::mvcc::Snapshot), [`Xid`](crate::mvcc::Xid),
/// [`XidStatus`](crate::mvcc::XidStatus)).
pub mod transaction;
/// Tuple versioning — the `xmin` / `xmax`-stamped
/// [`Version`](crate::mvcc::Version).
pub mod version;

pub use error::{MvccError, Result};
pub use store::VersionedStore;
pub use transaction::{Snapshot, TransactionManager, Xid, XidStatus, INVALID_XID};
pub use version::Version;
