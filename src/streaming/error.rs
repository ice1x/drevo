//! The standalone error channel for the streaming-ingestion engine.
//!
//! Like the replication engine (task `00095`), the authorization engine
//! (`00094`), and the query planner (`00085`–`00089`), streaming ingestion
//! keeps its own [`StreamError`] rather than adding a variant to the crate-wide
//! `DrevoError`: it is an opt-in substrate not yet wired into the executor /
//! HTTP / Bolt request path, so it has no reason to widen the core error type's
//! exhaustive match sites. A sink (the thing that actually mutates the graph)
//! reports failures as opaque strings which are wrapped in the
//! [`Sink`](StreamError::Sink) variant — this keeps the engine decoupled from
//! any one sink implementation's error type.

use crate::streaming::source::Offset;

/// A failure raised while consuming a stream of graph-update events.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// A message payload could not be decoded into an
    /// [`IngestEvent`](crate::streaming::IngestEvent). Carries the broker
    /// [`Offset`] so the offending record can be located, and a human-readable
    /// reason (the underlying JSON error rendered to text).
    #[error("malformed event at offset {offset}: {reason}")]
    Parse {
        /// The broker offset of the message that failed to parse.
        offset: Offset,
        /// A human-readable description of why decoding failed.
        reason: String,
    },

    /// The sink rejected an otherwise well-formed event — for example a
    /// duplicate title, a dangling edge endpoint, or a storage-layer failure.
    /// Carries the broker [`Offset`] and the sink's own error rendered to text.
    #[error("sink rejected event at offset {offset}: {reason}")]
    Sink {
        /// The broker offset of the message the sink rejected.
        offset: Offset,
        /// The sink's error, rendered to text.
        reason: String,
    },
}

/// Convenience alias for results from the streaming-ingestion engine.
pub type Result<T> = core::result::Result<T, StreamError>;
