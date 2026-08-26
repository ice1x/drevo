//! The `drevo-json-v1` dump wire format — the storage-agnostic interchange
//! types shared by every engine.
//!
//! A [`crate::dump::Dump`] is the whole graph (every node, every edge,
//! plus the id-allocation counters) in one serde-serializable value. It is the
//! read/write unit of the cross-engine migration seam: exporting from one engine
//! and applying to another moves a live graph between the KV-backed store and
//! the native engine without losing a byte.
//!
//! Only the **types** live here — [`crate::dump::Dump`],
//! [`crate::dump::ImportReport`],
//! [`crate::dump::DumpError`], and the
//! [`crate::dump::FORMAT_V1`] identifier — because the native engine
//! (in this crate) produces and consumes them directly. The KV-specific
//! machinery that renders
//! and parses the *file* formats (pretty JSON, GraphML XML) and the
//! filesystem/HTTP entry points stay in the main crate's `dump` module, which
//! re-exports these types so `crate::dump::Dump` keeps resolving there.

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::model::{Edge, Node};

/// Wire-format identifier for the v1 dump schema. Always emitted in the
/// `format` field; import refuses to load any other value.
pub const FORMAT_V1: &str = "drevo-json-v1";

/// Top-level wire format of a `drevo-json-v1` dump.
///
/// Public so external tooling (CLI dumpers, migration scripts) can parse a
/// dump without re-deriving the schema. Field order is fixed for forward-
/// compatibility — new fields will be added with serde defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dump {
    /// Schema identifier. MUST be [`FORMAT_V1`] on import.
    pub format: String,
    /// Producer's wall-clock at export time (Unix milliseconds). Informational.
    pub exported_at: i64,
    /// Producer's `next_node_id` counter — the receiver clamps its counter
    /// to be at least this value so freshly-allocated ids never collide with
    /// imported ones.
    pub next_node_id: u64,
    /// Producer's `next_edge_id` counter (see [`next_node_id`](Self::next_node_id)).
    pub next_edge_id: u64,
    /// Every node in the source database.
    pub nodes: Vec<Node>,
    /// Every edge in the source database.
    pub edges: Vec<Edge>,
}

/// Outcome of a successful dump import (`Drevo::import_json` /
/// `GraphEngine::apply_dump`).
///
/// Counts are reported separately so callers can distinguish "newly inserted"
/// from "already present, skipped". `*_skipped` rows were matched by id AND
/// byte-equal content; an id collision with different content surfaces as a
/// [`DumpError`] before this struct is returned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    /// Number of nodes inserted into the receiver during this import.
    pub nodes_imported: usize,
    /// Number of edges inserted into the receiver during this import.
    pub edges_imported: usize,
    /// Number of nodes that already existed with byte-equal content and were
    /// skipped (idempotent re-import).
    pub nodes_skipped: usize,
    /// Number of edges that already existed with byte-equal content and were
    /// skipped (idempotent re-import).
    pub edges_skipped: usize,
}

/// Import-time failure modes specific to the dump format.
///
/// Lifted to callers as a backend `Io` error via the `From` impls (this crate's
/// [`CoreError`] and the main crate's `DrevoError`) so the public error
/// hierarchies stay at their well-known variants. The conversion preserves the
/// human-readable message — `{err}` includes the original failure mode.
#[derive(Debug, thiserror::Error)]
pub enum DumpError {
    /// The JSON payload was malformed and `serde_json` could not parse it.
    #[error("malformed JSON dump: {0}")]
    Malformed(#[from] serde_json::Error),
    /// The `format` field is missing, empty, or names an unknown schema.
    #[error("unsupported dump format: {0:?} — expected drevo-json-v1")]
    UnsupportedFormat(String),
    /// An imported node / edge collides with an existing row that has the
    /// same id but different content.
    #[error("id collision on import: {0}")]
    IdCollision(String),
    /// A GraphML payload was not well-formed XML, was missing a required
    /// structural element (`<graphml>` / `<graph>` / a `<node>` id), or
    /// referenced an undeclared node from an `<edge>`.
    #[error("malformed GraphML: {0}")]
    MalformedGraphml(String),
}

impl From<DumpError> for CoreError {
    fn from(err: DumpError) -> Self {
        CoreError::Io(std::io::Error::other(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_error_renders_and_lifts_to_core_io() {
        let e = DumpError::IdCollision("node 5".into());
        assert_eq!(e.to_string(), "id collision on import: node 5");
        let core: CoreError = e.into();
        assert!(matches!(core, CoreError::Io(_)));
        assert_eq!(core.to_string(), "io error: id collision on import: node 5");
    }

    #[test]
    fn dump_round_trips_through_json() {
        let dump = Dump {
            format: FORMAT_V1.to_string(),
            exported_at: 123,
            next_node_id: 9,
            next_edge_id: 4,
            nodes: Vec::new(),
            edges: Vec::new(),
        };
        let json = serde_json::to_string(&dump).unwrap();
        let back: Dump = serde_json::from_str(&json).unwrap();
        assert_eq!(back.format, FORMAT_V1);
        assert_eq!(back.next_node_id, 9);
        assert_eq!(back.next_edge_id, 4);
    }

    #[test]
    fn import_report_default_is_all_zero() {
        assert_eq!(ImportReport::default(), ImportReport::default());
        let r = ImportReport::default();
        assert_eq!(
            r.nodes_imported + r.edges_imported + r.nodes_skipped + r.edges_skipped,
            0
        );
    }
}
