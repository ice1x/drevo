use std::fmt;

/// Errors that can occur during storage operations.
///
/// This enum is intentionally **closed** — every distinct failure mode the
/// storage layer can produce is a typed variant. Stringly-typed catch-alls
/// (`Backend(String)`, `Serialization(String)`) were removed in audit
/// task `00104` so that callers (and the HTTP layer in particular) can
/// programmatically distinguish backend, encode, decode, and lock failures.
///
/// Sub-types of `redb::Error` (`redb::TableError`, `redb::CommitError`,
/// `redb::StorageError`, `redb::TransactionError`, `redb::DatabaseError`)
/// all convert to `StorageError::Redb` via the `?` operator (only when the
/// `redb-backend` feature is enabled) — see the explicit `From` impls below.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The requested key was not found.
    #[error("key not found: {}", DisplayBytes(.0))]
    NotFound(Vec<u8>),

    /// An I/O error occurred in the underlying backend.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A bincode encode (serialization) error occurred while writing a
    /// snapshot to disk or producing on-the-wire bytes.
    #[error("encode error: {0}")]
    Encode(#[from] bincode::error::EncodeError),

    /// A bincode decode (deserialization) error occurred while loading a
    /// snapshot or parsing on-the-wire bytes.
    #[error("decode error: {0}")]
    Decode(#[from] bincode::error::DecodeError),

    /// A redb-backed storage operation failed.
    ///
    /// Wraps the upstream `redb::Error` directly so callers can match on
    /// the exact failure mode (table missing, txn aborted, I/O, etc.)
    /// instead of parsing a string. Boxed because `redb::Error` is a
    /// large enum (~160 bytes) and inflating every `StorageError` to that
    /// size triggers clippy's `result_large_err` lint at every `?` site.
    #[cfg(feature = "redb-backend")]
    #[error("redb error: {0}")]
    Redb(Box<redb::Error>),

    /// A `Mutex` or `RwLock` protecting backend state was poisoned by a
    /// previous panic.
    ///
    /// The backend is in an unrecoverable state — the only sane response
    /// is to log and discard the backend handle. Distinguished from
    /// `StorageError::Redb` (which only exists with the `redb-backend`
    /// feature) because a poisoned lock is structural, not a
    /// backend-internal I/O or transaction failure.
    #[error("lock poisoned")]
    LockPoisoned,

    /// `StorageBackend::compact` was called on a backend whose internal
    /// handle is shared with another owner (e.g. an outstanding
    /// `Arc<Database>` clone on the redb backend). Reclaiming free pages
    /// in redb requires exclusive `&mut Database` access via
    /// `Arc::get_mut`; when that fails the operator must drop the extra
    /// reference and retry. Surfaced by Phase 9 task `00054`.
    #[error("compact requires exclusive backend access")]
    CompactNotExclusive,

    /// The on-disk file was written by a drevo build with a newer,
    /// layout-incompatible **major** format version (or carries an
    /// unparseable format marker). Opening it is refused rather than risk
    /// silently misreading or corrupting the data. `found` is the raw
    /// version string recorded in the file (e.g. `"2.0"`); this build reads
    /// any file whose major version is `<= supported_major`. See the
    /// on-disk format-version guarantee (issue #48).
    #[error(
        "incompatible on-disk format: file reports version {found:?}, \
         this build supports major format version {supported_major}"
    )]
    IncompatibleFormat {
        /// Raw `MAJOR.MINOR` version string read from the file (or the
        /// unparseable marker bytes rendered lossily).
        found: String,
        /// Highest on-disk major version this build can read.
        supported_major: u32,
    },

    /// An online [`shrink_rebuild`](crate::storage::StorageBackend::shrink_rebuild)
    /// produced a compacted copy that failed its self-diagnostic: the row counts
    /// of the rebuilt file did not match the source (a duplicate-key collision or
    /// short read that would have silently dropped data). The diagnostic runs
    /// **before** the live file is touched, so the rebuild is discarded and the
    /// live database is left completely untouched — **no data is lost**.
    /// `expected` / `got` are `(data_rows, meta_rows)`.
    #[error(
        "shrink verification failed: rebuilt file has {got:?} rows, expected {expected:?}; \
         the live database was left untouched"
    )]
    ShrinkVerificationFailed {
        /// `(data_rows, meta_rows)` streamed from the source into the rebuild.
        expected: (u64, u64),
        /// `(data_rows, meta_rows)` actually found in the rebuilt file.
        got: (u64, u64),
    },
}

// Lift redb sub-error types into `StorageError::Redb` so `?` works at every
// redb call site without an intermediate `.map_err(...)`. Each redb sub-type
// already implements `From<X> for redb::Error` upstream; we box the result.
#[cfg(feature = "redb-backend")]
impl From<redb::Error> for StorageError {
    fn from(e: redb::Error) -> Self {
        Self::Redb(Box::new(e))
    }
}

#[cfg(feature = "redb-backend")]
impl From<redb::TableError> for StorageError {
    fn from(e: redb::TableError) -> Self {
        Self::Redb(Box::new(e.into()))
    }
}

#[cfg(feature = "redb-backend")]
impl From<redb::CommitError> for StorageError {
    fn from(e: redb::CommitError) -> Self {
        Self::Redb(Box::new(e.into()))
    }
}

#[cfg(feature = "redb-backend")]
impl From<redb::TransactionError> for StorageError {
    fn from(e: redb::TransactionError) -> Self {
        Self::Redb(Box::new(e.into()))
    }
}

#[cfg(feature = "redb-backend")]
impl From<redb::DatabaseError> for StorageError {
    fn from(e: redb::DatabaseError) -> Self {
        Self::Redb(Box::new(e.into()))
    }
}

#[cfg(feature = "redb-backend")]
impl From<redb::StorageError> for StorageError {
    fn from(e: redb::StorageError) -> Self {
        Self::Redb(Box::new(e.into()))
    }
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
