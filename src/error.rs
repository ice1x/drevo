//! Top-level error types for drevo.

use crate::storage::StorageError;

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
}

/// Convenience type alias for drevo operations.
pub type Result<T> = std::result::Result<T, DrevoError>;
