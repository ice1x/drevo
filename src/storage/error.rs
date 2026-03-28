use std::fmt;

/// Errors that can occur during storage operations.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The requested key was not found.
    #[error("key not found: {}", DisplayBytes(.0))]
    NotFound(Vec<u8>),

    /// An I/O error occurred in the underlying backend.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A serialization or deserialization error occurred.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// A backend-specific error that doesn't fit other categories.
    #[error("backend error: {0}")]
    Backend(String),
}

/// Helper to display byte slices in error messages.
struct DisplayBytes<'a>(&'a [u8]);

impl fmt::Display for DisplayBytes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match std::str::from_utf8(self.0) {
            Ok(s) => write!(f, "{s}"),
            Err(_) => write!(f, "{:?}", self.0),
        }
    }
}

/// Convenience type alias for storage operations.
pub type Result<T> = std::result::Result<T, StorageError>;
