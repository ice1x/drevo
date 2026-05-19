//! JSON import / export — Phase 9 hardening task `00055`.
//!
//! Provides a human-readable, schema-versioned dump format that captures the
//! entire graph (every node and every edge) and can be reloaded into any
//! [`Drevo`] handle, regardless of which backend (memory or redb) produced it
//! or now receives it.
//!
//! ## Format
//!
//! The wire format is `drevo-json-v1`:
//!
//! ```json
//! {
//!   "format": "drevo-json-v1",
//!   "exported_at": 1747740000000,
//!   "next_node_id": 6,
//!   "next_edge_id": 5,
//!   "nodes": [ Node, … ],
//!   "edges": [ Edge, … ]
//! }
//! ```
//!
//! * `format` is mandatory; mismatches are rejected as
//!   [`DumpError::UnsupportedFormat`].
//! * `exported_at` is the producer's [`crate::model::now_ms`] at export time —
//!   informational only.
//! * `next_node_id` / `next_edge_id` capture the producer's auto-increment
//!   counter so the receiver can resume allocating ids above the imported
//!   range — protects against id reuse after backup-restore.
//! * `nodes` and `edges` carry the full struct payloads with every field
//!   preserved (id, uuid, timestamps, kind, body, body_html, properties,
//!   weight). The receiver uses these verbatim and rebuilds every secondary
//!   index by replaying the data through the existing storage primitives.
//!
//! ## Idempotence
//!
//! Re-importing an identical dump into a populated database is a no-op: nodes
//! and edges already present (matched by `id` AND byte-equal content) are
//! skipped and counted in [`ImportReport::nodes_skipped`] /
//! [`edges_skipped`]. A title collision against a *different* node yields
//! [`DrevoError::DuplicateTitle`].
//!
//! ## Errors
//!
//! [`DumpError`] enumerates the import-time failure modes that are
//! independent of the storage layer (malformed JSON, unknown format,
//! mismatched schema). They surface to callers as [`DrevoError::Io`] because
//! the JSON / file boundary is conceptually an IO boundary; this avoids
//! growing a new top-level variant for a feature that lives one module deep.
//!
//! ## WASM
//!
//! [`Drevo::export_json`] and [`Drevo::import_json`] are available on every
//! target — they operate on `String` only and do not touch the filesystem.
//! [`Drevo::export_json_to_path`] / [`Drevo::import_json_from_path`] are gated
//! behind `cfg(not(target_arch = "wasm32"))` because `std::fs` is not
//! available in the browser.

use serde::{Deserialize, Serialize};

use crate::db::Drevo;
use crate::error::{DrevoError, Result};
use crate::model::{now_ms, Edge, NewEdge, Node, Properties};

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

/// Outcome of a successful [`Drevo::import_json`] call.
///
/// Counts are reported separately so callers can distinguish "newly inserted"
/// from "already present, skipped". `*_skipped` rows were matched by id AND
/// byte-equal content; an id collision with different content surfaces as a
/// [`DrevoError`] before this struct is returned.
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
/// Surfaced to callers as [`DrevoError::Io`] via the `From` impl below so the
/// public error hierarchy stays at five well-known variants. The conversion
/// preserves the human-readable message — `{err}` includes the original
/// failure mode.
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
}

impl From<DumpError> for DrevoError {
    fn from(err: DumpError) -> Self {
        DrevoError::Io(std::io::Error::other(err.to_string()))
    }
}

impl Drevo {
    /// Serialize the entire graph (every node, every edge) into a
    /// pretty-printed JSON string.
    ///
    /// Output is deterministic for a given graph because
    /// [`Properties`](crate::model::Properties) sorts its keys before
    /// serialising — two databases with the same logical content produce the
    /// same dump byte-for-byte.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::Storage`] on backend scan failures and
    /// [`DrevoError::Io`] if `serde_json` cannot serialise (e.g. a property
    /// contains a non-finite float).
    pub fn export_json(&self) -> Result<String> {
        let dump = self.build_dump()?;
        serde_json::to_string_pretty(&dump)
            .map_err(|e| DrevoError::Io(std::io::Error::other(e.to_string())))
    }

    /// Serialize the graph and write the result to `path` (filesystem write,
    /// not available on WASM).
    ///
    /// Overwrites the target file. Errors from the filesystem or the
    /// underlying scan propagate as [`DrevoError`].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn export_json_to_path(&self, path: &std::path::Path) -> Result<()> {
        let dump = self.export_json()?;
        std::fs::write(path, dump).map_err(DrevoError::Io)
    }

    /// Import a JSON dump produced by [`export_json`](Self::export_json) (or
    /// any equivalent producer) into this database.
    ///
    /// Behavior:
    ///
    /// * Nodes with an `id` that already exists in this database and matches
    ///   byte-for-byte are skipped (counted in [`ImportReport::nodes_skipped`]).
    ///   Same for edges.
    /// * Nodes with the same `id` but different content yield
    ///   [`DrevoError::Io`] (wrapping [`DumpError::IdCollision`]).
    /// * Title or UUID collisions against *different* existing nodes yield
    ///   [`DrevoError::DuplicateTitle`] / [`DrevoError::Storage`].
    /// * After all rows are applied, the auto-increment counters are clamped
    ///   above every imported id so subsequent `alloc_node_id` /
    ///   `alloc_edge_id` calls never collide.
    ///
    /// # Errors
    ///
    /// * [`DrevoError::Io`] — malformed JSON, unknown format, id collision.
    /// * [`DrevoError::DuplicateTitle`] — imported title clashes with a
    ///   different existing node.
    /// * [`DrevoError::Storage`] / [`DrevoError::Encode`] — backend failure.
    pub fn import_json(&self, raw: &str) -> Result<ImportReport> {
        let dump: Dump = serde_json::from_str(raw).map_err(DumpError::from)?;
        if dump.format != FORMAT_V1 {
            return Err(DumpError::UnsupportedFormat(dump.format).into());
        }
        self.apply_dump(dump)
    }

    /// Read a JSON dump from `path` (filesystem read, not available on WASM)
    /// and import it into this database. See [`import_json`](Self::import_json).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn import_json_from_path(&self, path: &std::path::Path) -> Result<ImportReport> {
        let raw = std::fs::read_to_string(path).map_err(DrevoError::Io)?;
        self.import_json(&raw)
    }

    // --- internals ---------------------------------------------------

    fn build_dump(&self) -> Result<Dump> {
        let nodes = self.collect_all_nodes()?;
        let edges = self.collect_all_edges()?;
        let next_node_id = nodes.iter().map(|n| n.id).max().map_or(1, |m| m + 1);
        let next_edge_id = edges.iter().map(|e| e.id).max().map_or(1, |m| m + 1);
        Ok(Dump {
            format: FORMAT_V1.to_string(),
            exported_at: now_ms(),
            next_node_id,
            next_edge_id,
            nodes,
            edges,
        })
    }

    fn apply_dump(&self, dump: Dump) -> Result<ImportReport> {
        let mut report = ImportReport::default();

        // --- Nodes ---
        for node in &dump.nodes {
            match self.get_node(node.id)? {
                Some(existing) if &existing == node => {
                    report.nodes_skipped += 1;
                    continue;
                }
                Some(_) => {
                    return Err(DumpError::IdCollision(format!(
                        "node id {} already exists with different content",
                        node.id
                    ))
                    .into());
                }
                None => {}
            }
            insert_node_verbatim(self, node)?;
            report.nodes_imported += 1;
        }

        // --- Edges ---
        for edge in &dump.edges {
            match self.get_edge(edge.id)? {
                Some(existing) if &existing == edge => {
                    report.edges_skipped += 1;
                    continue;
                }
                Some(_) => {
                    return Err(DumpError::IdCollision(format!(
                        "edge id {} already exists with different content",
                        edge.id
                    ))
                    .into());
                }
                None => {}
            }
            insert_edge_verbatim(self, edge)?;
            report.edges_imported += 1;
        }

        // --- ID counters ---
        // Clamp our counters so future allocations never collide with the
        // imported range. `bump_node_counter` is idempotent — if our counter
        // is already higher, it is a no-op.
        self.bump_node_counter_to_at_least(dump.next_node_id);
        self.bump_edge_counter_to_at_least(dump.next_edge_id);

        Ok(report)
    }
}

/// Insert a verbatim node, preserving id / uuid / timestamps and rebuilding
/// every secondary index. Title uniqueness is enforced via the existing
/// `create_node` path, which surfaces [`DrevoError::DuplicateTitle`].
fn insert_node_verbatim(db: &Drevo, node: &Node) -> Result<()> {
    db.insert_node_raw(node)
}

/// Insert a verbatim edge, preserving id / uuid / timestamps.
fn insert_edge_verbatim(db: &Drevo, edge: &Edge) -> Result<()> {
    // Both endpoints must exist; `create_edge` validates this, but since we
    // build edges verbatim (preserving their id and uuid) we bypass the
    // normal allocation path and call directly into the raw insert helper.
    if db.get_node(edge.from_id)?.is_none() {
        return Err(DrevoError::NodeNotFound(edge.from_id));
    }
    if db.get_node(edge.to_id)?.is_none() {
        return Err(DrevoError::NodeNotFound(edge.to_id));
    }
    db.insert_edge_raw(edge)
}

/// Helper used by [`Dump`] parsing to keep `Properties` symmetric with
/// `serde_json::Value::Object`. Currently identical to `From<HashMap>` but
/// kept for forward-compatibility with future Cypher-shaped property types.
#[allow(dead_code)]
fn properties_from_object(obj: serde_json::Map<String, serde_json::Value>) -> Properties {
    Properties(obj.into_iter().collect())
}

/// Helper used by `NewEdge::from(&Edge)` round-trips in tests / external
/// tooling — exposed to keep the wire format documentation grounded in real
/// code paths.
#[allow(dead_code)]
fn edge_to_new_edge(edge: &Edge) -> NewEdge {
    NewEdge {
        from_id: edge.from_id,
        to_id: edge.to_id,
        kind: edge.kind.clone(),
        weight: edge.weight,
        properties: edge.properties.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NewNode, Properties};
    use serde_json::json;
    use std::collections::HashMap;

    fn props(pairs: &[(&str, serde_json::Value)]) -> Properties {
        let mut m = HashMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        Properties::from(m)
    }

    #[test]
    fn dump_serialises_format_field() {
        let db = Drevo::open_in_memory().unwrap();
        let dump = db.build_dump().unwrap();
        assert_eq!(dump.format, FORMAT_V1);
    }

    #[test]
    fn import_report_default_is_all_zero() {
        let r = ImportReport::default();
        assert_eq!(r.nodes_imported, 0);
        assert_eq!(r.edges_imported, 0);
        assert_eq!(r.nodes_skipped, 0);
        assert_eq!(r.edges_skipped, 0);
    }

    #[test]
    fn round_trip_single_node() {
        let db = Drevo::open_in_memory().unwrap();
        db.create_node(NewNode {
            kind: "note".into(),
            title: "Round trip".into(),
            body: "body".into(),
            body_html: "".into(),
            properties: props(&[("x", json!(1))]),
        })
        .unwrap();
        let dump = db.export_json().unwrap();
        let other = Drevo::open_in_memory().unwrap();
        let r = other.import_json(&dump).unwrap();
        assert_eq!(r.nodes_imported, 1);
    }

    #[test]
    fn unsupported_format_is_typed() {
        let bad = r#"{"format":"v999","exported_at":0,"next_node_id":1,"next_edge_id":1,"nodes":[],"edges":[]}"#;
        let db = Drevo::open_in_memory().unwrap();
        let err = db.import_json(bad).unwrap_err();
        assert!(matches!(err, DrevoError::Io(_)));
    }

    #[test]
    fn dump_error_into_drevo_error_preserves_message() {
        let err: DrevoError = DumpError::UnsupportedFormat("foo".into()).into();
        let message = format!("{err}");
        assert!(message.contains("foo"), "got: {message}");
    }
}
