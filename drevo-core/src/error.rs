//! The storage-agnostic error type for the drevo graph core.
//!
//! [`crate::error::CoreError`] is the error channel of everything that lives (or will live,
//! in later extraction slices) in `drevo-core`: the native graph engine, its
//! indexes, and the cross-engine dump/migration seam. It carries only failures
//! that make sense *without* naming a concrete backend — the graph-semantic
//! ones (`NodeNotFound`, `EdgeNotFound`, `DuplicateTitle`, `InvalidWeight`) plus
//! the two ubiquitous serialization boundaries (`Io`, `Json`).
//!
//! Backend-specific failures a concrete engine hits — KV storage errors, vector
//! index errors, transaction-state errors — have no structured home here; a
//! backend lifts them into the opaque [`crate::error::CoreError::Backend`]
//! catch-all, carrying the lower layer's rendered message.
//!
//! # Relationship to `drevo::DrevoError`
//!
//! The main `drevo` crate keeps its richer [`DrevoError`] (which additionally
//! wraps `StorageError`, `VectorError`, the bincode codecs, and the
//! transaction / migration states). The two convert **structurally** in both
//! directions — the six shared variants map one-to-one, and everything with no
//! counterpart degrades to [`crate::error::CoreError::Backend`]
//! (going down) or `DrevoError::Io` (coming back up). Those `From` impls live in the main crate
//! (`src/error.rs`), next to `DrevoError`, since only it can name both types.
//!
//! [`DrevoError`]: https://docs.rs/drevo/latest/drevo/error/enum.DrevoError.html

use thiserror::Error;

/// Errors from the storage-agnostic graph core.
///
/// The variant set is deliberately a *subset* of the main crate's `DrevoError`:
/// exactly the failures the native engine, its indexes, and the dump seam can
/// raise without depending on a KV store, a vector index, or the HTTP/Bolt
/// layers. See the [module docs](self) for how it relates to `DrevoError`.
#[derive(Debug, Error)]
pub enum CoreError {
    /// The requested node was not found.
    #[error("node not found: {0}")]
    NodeNotFound(u64),

    /// The requested edge was not found.
    #[error("edge not found: {0}")]
    EdgeNotFound(u64),

    /// A node with the given title already exists (title uniqueness).
    #[error("duplicate title: {0}")]
    DuplicateTitle(String),

    /// An edge weight failed validation: it is not a finite `f32`
    /// (NaN, +Inf, or -Inf are rejected at edge create / update). `Edge`
    /// derives `PartialEq`, which `f32::NAN != f32::NAN` would break.
    #[error("invalid edge weight: {0} — weight must be a finite f32")]
    InvalidWeight(f32),

    /// An I/O error occurred (e.g. reading or writing a dump file).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON (de)serialization error occurred while encoding a property value
    /// or a dump record.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A backend-specific failure from a concrete engine — a KV storage error,
    /// a vector-index error, a transaction-state error — that has no structured
    /// counterpart in the storage-agnostic core. Carries the lower layer's
    /// rendered message so nothing is lost on the wire, even though the
    /// structured variant is not preserved.
    #[error("backend error: {0}")]
    Backend(String),
}

/// Convenience alias for fallible core operations.
pub type Result<T> = std::result::Result<T, CoreError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_matches_the_drevo_error_wording() {
        // The rendered strings are asserted here so the two crates' error
        // messages stay identical across the seam (the HTTP/Bolt layers surface
        // them verbatim).
        assert_eq!(CoreError::NodeNotFound(7).to_string(), "node not found: 7");
        assert_eq!(CoreError::EdgeNotFound(9).to_string(), "edge not found: 9");
        assert_eq!(
            CoreError::DuplicateTitle("Dup".into()).to_string(),
            "duplicate title: Dup"
        );
        assert_eq!(
            CoreError::InvalidWeight(f32::NAN).to_string(),
            "invalid edge weight: NaN — weight must be a finite f32"
        );
        assert_eq!(
            CoreError::Backend("scan failed".into()).to_string(),
            "backend error: scan failed"
        );
    }

    #[test]
    fn io_and_json_lift_through_the_question_mark_operator() {
        fn io() -> Result<()> {
            Err(std::io::Error::other("boom"))?;
            Ok(())
        }
        fn json() -> Result<()> {
            let _: serde_json::Value = serde_json::from_str("{ not json")?;
            Ok(())
        }
        assert!(matches!(io(), Err(CoreError::Io(_))));
        assert!(matches!(json(), Err(CoreError::Json(_))));
    }
}
