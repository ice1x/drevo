//! Top-level error types for drevo.

use crate::storage::StorageError;
use crate::vector::VectorError;

/// Errors that can occur during drevo operations.
///
/// Two-layer architecture (per `drevo-architecture` §"Error Propagation"):
/// `StorageError → DrevoError → ApiError → HTTP 5xx`. Each variant either
/// wraps a lower-layer error (`Storage`, `Encode`, `Decode`, `Io`) or
/// describes a database-level semantic failure (`NodeNotFound`,
/// `EdgeNotFound`, `DuplicateTitle`, `Locked`).
///
/// The pre-`00104` stringly-typed `Serialization(String)` was split into
/// structured `Encode(EncodeError)` / `Decode(DecodeError)` variants so
/// the HTTP layer can distinguish encode failures (programmer bug) from
/// decode failures (corrupt persisted bytes) programmatically.
#[derive(Debug, thiserror::Error)]
pub enum DrevoError {
    /// An error from the underlying storage layer.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// A bincode encode (serialization) error occurred while writing a
    /// node or edge to the backend.
    #[error("encode error: {0}")]
    Encode(#[from] bincode::error::EncodeError),

    /// A bincode decode (deserialization) error occurred while reading a
    /// node or edge from the backend.
    #[error("decode error: {0}")]
    Decode(#[from] bincode::error::DecodeError),

    /// The requested node was not found.
    #[error("node not found: {0}")]
    NodeNotFound(u64),

    /// The requested edge was not found.
    #[error("edge not found: {0}")]
    EdgeNotFound(u64),

    /// A node with the given title already exists.
    #[error("duplicate title: {0}")]
    DuplicateTitle(String),

    /// The database file is locked by another process.
    #[error("database locked")]
    Locked,

    /// An edge weight failed validation: it is not a finite `f32`
    /// (NaN, +Inf, or -Inf are rejected at `create_edge` / `update_edge`).
    ///
    /// `drevo-database` §"Edge" defines `weight: f32` for Dijkstra ranking;
    /// `Edge` derives `PartialEq` which `f32::NAN != f32::NAN` would break.
    /// Cross-link: `audit/AUDIT-model.md` F4.
    #[error("invalid edge weight: {0} — weight must be a finite f32")]
    InvalidWeight(f32),

    /// An I/O error occurred.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON (de)serialization error occurred while encoding a property
    /// value for the persistent property index (Phase 14 task `00088`).
    /// `serde_json::Value`s reachable from `Properties` are always
    /// encodable, so this is effectively unreachable in practice; it
    /// exists so the index layer can stay panic-free per `drevo-rust`
    /// §"Error handling".
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A vector operation failed — a malformed embedding, a dimension
    /// mismatch, or a zero-magnitude operand. Raised by the Phase 12
    /// persistence layer (`00078`) when a stored [`crate::vector::Vector`]
    /// cannot be inserted into the HNSW index it is rebuilding, and
    /// available to any caller that lifts a
    /// [`VectorError`] into the database error
    /// channel. Maps to a Bolt `SEMANTIC_ERROR` on the wire.
    #[error("vector error: {0}")]
    Vector(#[from] VectorError),

    /// A second explicit transaction (`Drevo::tx_begin`) was requested
    /// while one is already active or being rolled back. The MVP
    /// undo-log layer (Phase 11 task `00072`) allows only a single
    /// in-flight transaction per [`crate::db::Drevo`] handle —
    /// concurrent isolation lands with MVCC (`00080`–`00084`). Maps to
    /// the Bolt status code `Neo.TransientError.Transaction.Outdated`.
    #[error("transaction already active")]
    TransactionAlreadyActive,

    /// `Drevo::tx_commit` / `Drevo::tx_rollback` was called outside of
    /// an explicit transaction. The Bolt session machinery (Phase 11
    /// task `00072`) guards this at the protocol layer; surfacing the
    /// distinct variant keeps non-Bolt callers honest. Maps to
    /// `Neo.ClientError.Transaction.TransactionNotFound` on the wire.
    #[error("no active transaction")]
    NoActiveTransaction,

    /// The database on disk uses an older adjacency layout than this build
    /// and must be migrated before it can be opened (#243 slice 2).
    ///
    /// Raised by [`crate::db::Drevo::open`] when the file's adjacency index
    /// predates the kind-in-key layout (`found_major < required_major`). The
    /// fix is an explicit, reversible migration — run `drevo migrate up`
    /// (which backs up to GraphML first) or call
    /// [`crate::db::Drevo::migrate_adjacency`] — which rewrites the index and
    /// re-stamps the on-disk format version. The graph data itself is never at
    /// risk: the migration rebuilds a derived index from the intact node/edge
    /// records. Maps to HTTP 503 and the Bolt status
    /// `Neo.TransientError.Database.Unavailable`.
    #[error(
        "database needs migration: on-disk adjacency format v{found_major} \
         predates this build's v{required_major} — run `drevo migrate up` \
         (backs up first) to upgrade"
    )]
    NeedsMigration {
        /// The adjacency-layout major version found on disk.
        found_major: u32,
        /// The adjacency-layout major version this build requires.
        required_major: u32,
    },
}

/// Convenience type alias for drevo operations.
pub type Result<T> = std::result::Result<T, DrevoError>;

/// Lift a storage-agnostic [`drevo_core::error::CoreError`] into the main
/// crate's richer [`DrevoError`].
///
/// The six shared variants map **one-to-one** (so a `NodeNotFound` raised deep
/// in the native engine still surfaces as `DrevoError::NodeNotFound`, which the
/// migrate seam and CRUD tests match on). The catch-all
/// [`CoreError::Backend`](drevo_core::error::CoreError::Backend) — which only a
/// concrete backend produces, never the native engine — degrades to
/// [`DrevoError::Io`], carrying its rendered message. This is the direction used
/// whenever a core-returning call (the `GraphEngine` seam, a dump helper) is `?`
/// -lifted into a `DrevoError` context.
impl From<drevo_core::error::CoreError> for DrevoError {
    fn from(err: drevo_core::error::CoreError) -> Self {
        use drevo_core::error::CoreError as C;
        match err {
            C::NodeNotFound(id) => DrevoError::NodeNotFound(id),
            C::EdgeNotFound(id) => DrevoError::EdgeNotFound(id),
            C::DuplicateTitle(t) => DrevoError::DuplicateTitle(t),
            C::InvalidWeight(w) => DrevoError::InvalidWeight(w),
            C::Io(e) => DrevoError::Io(e),
            C::Json(e) => DrevoError::Json(e),
            C::Backend(msg) => DrevoError::Io(std::io::Error::other(msg)),
        }
    }
}

/// Lower a [`DrevoError`] into the storage-agnostic
/// [`drevo_core::error::CoreError`].
///
/// This is the direction a concrete backend (the KV-backed `Drevo`) uses when it
/// implements a `drevo-core` seam whose signature speaks `CoreError`: the six
/// shared variants map one-to-one, and every backend-specific variant
/// (`Storage`, `Encode`, `Decode`, `Vector`, the transaction states,
/// `NeedsMigration`) — which has no structured home in the core — collapses into
/// [`CoreError::Backend`](drevo_core::error::CoreError::Backend), preserving the
/// rendered message. Round-tripping a *shared* variant through both impls is
/// lossless; a backend-specific one degrades to `Backend` then to
/// `DrevoError::Io`, which is acceptable because those never flow *up* through
/// the seam in a variant-matched path (they are matched only on the inherent KV
/// API, which never crosses the trait).
impl From<DrevoError> for drevo_core::error::CoreError {
    fn from(err: DrevoError) -> Self {
        use drevo_core::error::CoreError as C;
        match err {
            DrevoError::NodeNotFound(id) => C::NodeNotFound(id),
            DrevoError::EdgeNotFound(id) => C::EdgeNotFound(id),
            DrevoError::DuplicateTitle(t) => C::DuplicateTitle(t),
            DrevoError::InvalidWeight(w) => C::InvalidWeight(w),
            DrevoError::Io(e) => C::Io(e),
            DrevoError::Json(e) => C::Json(e),
            // No structured counterpart in the core — keep the message.
            other => C::Backend(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drevo_core::error::CoreError;

    #[test]
    fn shared_variants_round_trip_losslessly_core_to_drevo_to_core() {
        // Each shared variant survives CoreError → DrevoError → CoreError with
        // its payload intact — the property the migrate seam and the native
        // engine rely on so `DrevoError::NodeNotFound(id)` stays matchable.
        let cases = [
            CoreError::NodeNotFound(42),
            CoreError::EdgeNotFound(7),
            CoreError::DuplicateTitle("Existing".into()),
            CoreError::InvalidWeight(1.5),
        ];
        for original in cases {
            let rendered = original.to_string();
            let back: CoreError = DrevoError::from(original).into();
            assert_eq!(back.to_string(), rendered);
        }
    }

    #[test]
    fn shared_variants_map_one_to_one_core_to_drevo() {
        assert!(matches!(
            DrevoError::from(CoreError::NodeNotFound(99)),
            DrevoError::NodeNotFound(99)
        ));
        assert!(matches!(
            DrevoError::from(CoreError::EdgeNotFound(99)),
            DrevoError::EdgeNotFound(99)
        ));
        assert!(matches!(
            DrevoError::from(CoreError::DuplicateTitle("Dup".into())),
            DrevoError::DuplicateTitle(t) if t == "Dup"
        ));
        assert!(matches!(
            DrevoError::from(CoreError::InvalidWeight(2.0)),
            DrevoError::InvalidWeight(_)
        ));
    }

    #[test]
    fn backend_specific_drevo_variants_collapse_to_core_backend() {
        // A variant with no structured core counterpart becomes `Backend`,
        // keeping its message; it must not be silently dropped.
        let core: CoreError = DrevoError::Locked.into();
        assert!(matches!(core, CoreError::Backend(_)));
        assert_eq!(core.to_string(), "backend error: database locked");

        let core: CoreError = DrevoError::TransactionAlreadyActive.into();
        assert!(matches!(core, CoreError::Backend(ref m) if m == "transaction already active"));
    }

    #[test]
    fn core_backend_lifts_to_drevo_io() {
        let drevo: DrevoError = CoreError::Backend("scan failed".into()).into();
        assert!(matches!(drevo, DrevoError::Io(_)));
        assert_eq!(drevo.to_string(), "io error: scan failed");
    }
}
