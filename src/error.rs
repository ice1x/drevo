//! Top-level error types for drevo.

use crate::storage::StorageError;

/// Errors that can occur during drevo operations.
#[derive(Debug, thiserror::Error)]
pub enum DrevoError {
    /// An error from the underlying storage layer.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// A serialization or deserialization error.
    #[error("serialization error: {0}")]
    Serialization(String),

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

    /// An I/O error occurred.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience type alias for drevo operations.
pub type Result<T> = std::result::Result<T, DrevoError>;
