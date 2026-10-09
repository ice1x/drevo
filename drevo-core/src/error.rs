//! The error type of `drevo-core`.
//!
//! [`crate::error::CoreError`] is returned by the graph engine, its indexes and
//! the dump format. It names the graph-level failures (`NodeNotFound`,
//! `EdgeNotFound`, `DuplicateTitle`, `InvalidWeight`) plus the I/O and JSON
//! boundaries (`Io`, `Json`).
//!
//! Failures from layers built on top — vector indexes, transaction state in a
//! server — have no structured home here; they travel in the opaque
//! [`crate::error::CoreError::Backend`] variant, carrying the lower layer's
//! rendered message.
//!
//! # Relationship to drevo's `DrevoError`
//!
//! drevo's own error type, `DrevoError`, also covers vector indexes, codecs
//! and server-side transaction state. The two convert in both directions: the
//! shared variants map one-to-one, and anything without a counterpart becomes
//! [`crate::error::CoreError::Backend`] going down or `DrevoError::Io` coming
//! back up.

use thiserror::Error;

/// Errors from the graph engine, its indexes and the dump format.
///
/// See the [module docs](self) for how it relates to drevo's `DrevoError`.
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

    /// The durable store is already open by another handle or process.
    ///
    /// [`NativeGraph::open_durable`](crate::native::NativeGraph::open_durable)
    /// takes an exclusive advisory lock on the store's lock sidecar; a second
    /// open of the same path fails with this rather than letting two writers
    /// interleave WAL appends and corrupt the log. Maps to drevo's
    /// `DrevoError::Locked`. The advisory lock is released when the
    /// owning process dies, so a crash never leaves a stale lock behind.
    #[error("database locked")]
    Locked,

    /// A failure from a layer built on top — a vector-index error, a
    /// transaction-state error — that has no structured variant here. Carries
    /// the lower layer's
    /// rendered message so nothing is lost on the wire, even though the
    /// structured variant is not preserved.
    #[error("backend error: {0}")]
    Backend(String),

    /// Two-phase commit: a transaction is prepared and every other
    /// write is refused until it is resolved (the prepared fence). Retryable;
    /// carries the pending global transaction ids.
    #[error("writes are paused while prepared transaction(s) {} await resolution; retry", .0.join(", "))]
    PreparedTransactionPending(Vec<String>),
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
        assert_eq!(CoreError::Locked.to_string(), "database locked");
    }

    #[test]
    fn prepared_transaction_pending_names_the_gids() {
        assert_eq!(
            CoreError::PreparedTransactionPending(vec!["a".into(), "b".into()]).to_string(),
            "writes are paused while prepared transaction(s) a, b await resolution; retry"
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
