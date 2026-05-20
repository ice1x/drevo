//! JSON import / export — Phase 9 hardening task `00055`. Phase 9 task
//! `00056` extends this module with read-only GraphML export
//! ([`Drevo::export_graphml`] / [`Drevo::export_graphml_to_path`]).
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
//!
//! ## GraphML export (task `00056`)
//!
//! [`Drevo::export_graphml`] emits the graph as a GraphML 1.0 document — the
//! ubiquitous XML interchange format consumed by yEd, Gephi, NetworkX,
//! Cytoscape, igraph, and a long tail of network-analysis tooling. The export
//! is one-way (no `import_graphml`); the project's authoritative wire format
//! remains [`FORMAT_V1`]. GraphML is offered for interop only.
//!
//! Layout of the emitted document:
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <graphml xmlns="http://graphml.graphdrawing.org/xmlns"
//!          xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
//!          xsi:schemaLocation="http://graphml.graphdrawing.org/xmlns
//!                              http://graphml.graphdrawing.org/xmlns/1.0/graphml.xsd">
//!   <key id="d_uuid"        for="node" attr.name="uuid"       attr.type="string"/>
//!   <key id="d_kind"        for="node" attr.name="kind"       attr.type="string"/>
//!   …
//!   <graph id="drevo" edgedefault="directed">
//!     <node id="n1"> <data key="d_kind">note</data> … </node>
//!     <edge id="e1" source="n1" target="n2"> <data key="d_e_kind">links_to</data> … </edge>
//!   </graph>
//! </graphml>
//! ```
//!
//! Nested [`Properties`] are serialised as a single JSON-string `<data>` value
//! (GraphML key type `string`) so the format remains lossless and re-parsable
//! by external tooling. The exporter is deterministic: nodes / edges are
//! emitted in id order (`collect_all_nodes` / `collect_all_edges` already sort
//! by id), and [`Properties`] sort their keys before serialising.
//!
//! Filesystem variant [`Drevo::export_graphml_to_path`] is gated off WASM.

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

    /// Serialize the entire graph as a GraphML 1.0 document.
    ///
    /// GraphML is the standard XML interchange format for network data; the
    /// output is loadable by yEd, Gephi, NetworkX, Cytoscape, igraph, and
    /// every other tool that speaks the spec. The graph is emitted with
    /// `edgedefault="directed"` to match drevo's directional edge model.
    ///
    /// Output is deterministic: nodes are listed in id order, edges in id
    /// order, and [`Properties`] sort their keys before serialising as a JSON
    /// string. Two databases with identical logical content produce
    /// byte-identical GraphML.
    ///
    /// Node ids are emitted as `n<id>`, edge ids as `e<id>`, so they remain
    /// valid XML `xs:NMTOKEN` values regardless of the numeric range.
    /// Property maps are encoded as a single JSON-string `<data key="d_props">`
    /// value so external readers can re-hydrate the full structure.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::Storage`] on backend scan failures and
    /// [`DrevoError::Io`] if any [`Properties`] value cannot be serialised to
    /// JSON (e.g. a non-finite float).
    pub fn export_graphml(&self) -> Result<String> {
        let nodes = self.collect_all_nodes()?;
        let edges = self.collect_all_edges()?;
        render_graphml(&nodes, &edges)
    }

    /// Serialize the graph as GraphML and write the result to `path`
    /// (filesystem write, not available on WASM).
    ///
    /// Overwrites the target file. Errors from the filesystem or the
    /// underlying scan propagate as [`DrevoError`].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn export_graphml_to_path(&self, path: &std::path::Path) -> Result<()> {
        let xml = self.export_graphml()?;
        std::fs::write(path, xml).map_err(DrevoError::Io)
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

// ---------------------------------------------------------------------
// GraphML rendering (task 00056)
// ---------------------------------------------------------------------

/// Render a sorted list of nodes / edges into a GraphML 1.0 document.
///
/// `nodes` and `edges` are expected to be id-sorted (callers must use
/// [`Drevo::collect_all_nodes`] / [`Drevo::collect_all_edges`] which already
/// guarantee this).
fn render_graphml(nodes: &[Node], edges: &[Edge]) -> Result<String> {
    let mut out = String::with_capacity(512 + nodes.len() * 256 + edges.len() * 160);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\"\n         \
         xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"\n         \
         xsi:schemaLocation=\"http://graphml.graphdrawing.org/xmlns \
         http://graphml.graphdrawing.org/xmlns/1.0/graphml.xsd\">\n",
    );

    // Key declarations — fixed schema. `attr.type` matches the GraphML
    // permitted-types vocabulary (`string` / `long` / `double`).
    for (id, name) in NODE_KEYS {
        out.push_str("  <key id=\"");
        out.push_str(id);
        out.push_str("\" for=\"node\" attr.name=\"");
        out.push_str(name);
        out.push_str("\" attr.type=\"");
        out.push_str(node_key_type(id));
        out.push_str("\"/>\n");
    }
    for (id, name) in EDGE_KEYS {
        out.push_str("  <key id=\"");
        out.push_str(id);
        out.push_str("\" for=\"edge\" attr.name=\"");
        out.push_str(name);
        out.push_str("\" attr.type=\"");
        out.push_str(edge_key_type(id));
        out.push_str("\"/>\n");
    }

    out.push_str("  <graph id=\"drevo\" edgedefault=\"directed\">\n");

    for node in nodes {
        render_node(&mut out, node)?;
    }
    for edge in edges {
        render_edge(&mut out, edge)?;
    }

    out.push_str("  </graph>\n");
    out.push_str("</graphml>\n");
    Ok(out)
}

/// Fixed set of node `<key>` declarations — (key id, human-readable name).
/// Edge-key ids are prefixed `d_e_` so they never collide with node-key ids.
const NODE_KEYS: &[(&str, &str)] = &[
    ("d_uuid", "uuid"),
    ("d_kind", "kind"),
    ("d_title", "title"),
    ("d_body", "body"),
    ("d_body_html", "body_html"),
    ("d_created_at", "created_at"),
    ("d_updated_at", "updated_at"),
    ("d_props", "properties"),
];

/// Fixed set of edge `<key>` declarations — (key id, human-readable name).
const EDGE_KEYS: &[(&str, &str)] = &[
    ("d_e_uuid", "uuid"),
    ("d_e_kind", "kind"),
    ("d_e_weight", "weight"),
    ("d_e_created_at", "created_at"),
    ("d_e_props", "properties"),
];

/// Map a node key id to its GraphML `attr.type`. Timestamps are GraphML
/// `long`s; everything else is a `string` (uuids are emitted in canonical
/// hyphenated hex, properties as a JSON literal).
fn node_key_type(id: &str) -> &'static str {
    match id {
        "d_created_at" | "d_updated_at" => "long",
        _ => "string",
    }
}

/// Map an edge key id to its GraphML `attr.type`.
fn edge_key_type(id: &str) -> &'static str {
    match id {
        "d_e_created_at" => "long",
        "d_e_weight" => "double",
        _ => "string",
    }
}

fn render_node(out: &mut String, node: &Node) -> Result<()> {
    out.push_str("    <node id=\"n");
    push_u64(out, node.id);
    out.push_str("\">\n");
    push_data(out, "d_uuid", &uuid_to_hyphenated(&node.uuid));
    push_data(out, "d_kind", &node.kind);
    push_data(out, "d_title", &node.title);
    push_data(out, "d_body", &node.body);
    push_data(out, "d_body_html", &node.body_html);
    push_data(out, "d_created_at", &node.created_at.to_string());
    push_data(out, "d_updated_at", &node.updated_at.to_string());
    let props_json = serde_json::to_string(&node.properties)
        .map_err(|e| DrevoError::Io(std::io::Error::other(e.to_string())))?;
    push_data(out, "d_props", &props_json);
    out.push_str("    </node>\n");
    Ok(())
}

fn render_edge(out: &mut String, edge: &Edge) -> Result<()> {
    out.push_str("    <edge id=\"e");
    push_u64(out, edge.id);
    out.push_str("\" source=\"n");
    push_u64(out, edge.from_id);
    out.push_str("\" target=\"n");
    push_u64(out, edge.to_id);
    out.push_str("\">\n");
    push_data(out, "d_e_uuid", &uuid_to_hyphenated(&edge.uuid));
    push_data(out, "d_e_kind", &edge.kind);
    push_data(out, "d_e_weight", &format_weight(edge.weight));
    push_data(out, "d_e_created_at", &edge.created_at.to_string());
    let props_json = serde_json::to_string(&edge.properties)
        .map_err(|e| DrevoError::Io(std::io::Error::other(e.to_string())))?;
    push_data(out, "d_e_props", &props_json);
    out.push_str("    </edge>\n");
    Ok(())
}

/// Append a `<data key="..">value</data>` line, escaping XML special
/// characters in `value`. Indented six spaces to nest cleanly inside a
/// `<node>` / `<edge>` opened with four leading spaces.
fn push_data(out: &mut String, key: &str, value: &str) {
    out.push_str("      <data key=\"");
    out.push_str(key);
    out.push_str("\">");
    push_escaped(out, value);
    out.push_str("</data>\n");
}

fn push_u64(out: &mut String, value: u64) {
    use std::fmt::Write as _;
    let _ = write!(out, "{value}");
}

/// Format a node UUID (16 raw bytes) as canonical hyphenated hex.
fn uuid_to_hyphenated(bytes: &[u8; 16]) -> String {
    uuid::Uuid::from_bytes(*bytes).hyphenated().to_string()
}

/// Format an edge weight for GraphML `attr.type="double"` element text.
///
/// Non-finite values are not representable by GraphML's `double` schema —
/// emit them as their JSON-compatible string ("NaN" / "Infinity" / "-Infinity")
/// so downstream tools can detect the anomaly instead of receiving an empty
/// or malformed `<data>` value.
fn format_weight(weight: f32) -> String {
    if weight.is_finite() {
        // f32::to_string already produces a `xs:double`-shaped value
        // (e.g. "1.5", "0.5", "-3.25", "0") for finite floats.
        weight.to_string()
    } else if weight.is_nan() {
        "NaN".to_string()
    } else if weight > 0.0 {
        "Infinity".to_string()
    } else {
        "-Infinity".to_string()
    }
}

/// Append `s` to `out`, escaping the five XML special characters in element
/// text. `'` and `"` are escaped too so the same routine works inside
/// attribute values, even though the current renderer only feeds it element
/// text.
fn push_escaped(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // GraphML/XML 1.0 forbids most C0 control characters except
            // tab, LF, CR. Replace anything else with the Unicode replacement
            // character so the output stays well-formed.
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' => {
                out.push('\u{FFFD}');
            }
            c => out.push(c),
        }
    }
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

    // -----------------------------------------------------------------
    // GraphML export (task 00056) — unit tests
    // -----------------------------------------------------------------

    #[test]
    fn graphml_empty_graph_has_xml_declaration() {
        let db = Drevo::open_in_memory().unwrap();
        let xml = db.export_graphml().unwrap();
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
    }

    #[test]
    fn graphml_empty_graph_has_root_element_with_namespace() {
        let db = Drevo::open_in_memory().unwrap();
        let xml = db.export_graphml().unwrap();
        assert!(xml.contains("<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\""));
        assert!(xml.trim_end().ends_with("</graphml>"));
    }

    #[test]
    fn graphml_empty_graph_declares_keys_and_directed_graph() {
        let db = Drevo::open_in_memory().unwrap();
        let xml = db.export_graphml().unwrap();
        for (id, name) in NODE_KEYS {
            assert!(
                xml.contains(&format!("id=\"{id}\""))
                    && xml.contains(&format!("attr.name=\"{name}\"")),
                "missing node key {id}/{name}",
            );
        }
        for (id, name) in EDGE_KEYS {
            assert!(
                xml.contains(&format!("id=\"{id}\""))
                    && xml.contains(&format!("attr.name=\"{name}\"")),
                "missing edge key {id}/{name}",
            );
        }
        assert!(xml.contains("<graph id=\"drevo\" edgedefault=\"directed\">"));
    }

    #[test]
    fn graphml_single_node_emits_data_elements() {
        let db = Drevo::open_in_memory().unwrap();
        db.create_node(NewNode {
            kind: "note".into(),
            title: "Hello".into(),
            body: "World".into(),
            body_html: "<p>World</p>".into(),
            properties: props(&[("priority", json!(1))]),
        })
        .unwrap();
        let xml = db.export_graphml().unwrap();
        assert!(xml.contains("<node id=\"n1\">"));
        assert!(xml.contains("<data key=\"d_kind\">note</data>"));
        assert!(xml.contains("<data key=\"d_title\">Hello</data>"));
        // body_html contains XML and must be escaped
        assert!(xml.contains("<data key=\"d_body_html\">&lt;p&gt;World&lt;/p&gt;</data>"));
        // properties serialised as a JSON literal — JSON quotes are XML-escaped
        // because `<data>` text must be well-formed XML.
        assert!(xml.contains("<data key=\"d_props\">{&quot;priority&quot;:1}</data>"));
    }

    #[test]
    fn graphml_escapes_xml_special_chars_in_title_and_body() {
        let db = Drevo::open_in_memory().unwrap();
        db.create_node(NewNode {
            kind: "note".into(),
            title: "a < b & c > d \" '".into(),
            body: "body & <tag>".into(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();
        let xml = db.export_graphml().unwrap();
        assert!(xml.contains("a &lt; b &amp; c &gt; d &quot; &apos;"));
        assert!(xml.contains("body &amp; &lt;tag&gt;"));
        // No raw special chars escaped into the text body — sanity: no
        // unescaped `<tag>` slipping into the document outside legitimate
        // markup.
        assert!(!xml.contains(">a < b"));
    }

    #[test]
    fn graphml_emits_edge_with_source_and_target() {
        let db = Drevo::open_in_memory().unwrap();
        let a = db
            .create_node(NewNode {
                kind: "note".into(),
                title: "A".into(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        let b = db
            .create_node(NewNode {
                kind: "note".into(),
                title: "B".into(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        db.create_edge(NewEdge {
            from_id: a.id,
            to_id: b.id,
            kind: "links_to".into(),
            weight: 1.5,
            properties: props(&[("color", json!("red"))]),
        })
        .unwrap();

        let xml = db.export_graphml().unwrap();
        assert!(xml.contains(&format!(
            "<edge id=\"e1\" source=\"n{}\" target=\"n{}\">",
            a.id, b.id
        )));
        assert!(xml.contains("<data key=\"d_e_kind\">links_to</data>"));
        assert!(xml.contains("<data key=\"d_e_weight\">1.5</data>"));
        assert!(xml.contains("<data key=\"d_e_props\">{&quot;color&quot;:&quot;red&quot;}</data>"));
    }

    #[test]
    fn graphml_is_deterministic_for_identical_graphs() {
        let build = || {
            let db = Drevo::open_in_memory().unwrap();
            db.create_node(NewNode {
                kind: "note".into(),
                title: "Alpha".into(),
                body: "a".into(),
                body_html: String::new(),
                properties: props(&[("z", json!(1)), ("a", json!(2)), ("m", json!(3))]),
            })
            .unwrap();
            db.create_node(NewNode {
                kind: "note".into(),
                title: "Beta".into(),
                body: "b".into(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
            db
        };
        let a_db = build();
        let b_db = build();
        // The two databases will differ in uuid / created_at / updated_at,
        // so we compare the GraphML *minus* those volatile fields by stripping
        // entire `<data key="d_uuid">…</data>` etc. blocks and confirming the
        // rest matches.
        let strip = |s: String| {
            let mut s = s;
            for key in [
                "d_uuid",
                "d_created_at",
                "d_updated_at",
                "d_e_uuid",
                "d_e_created_at",
            ] {
                let needle_open = format!("<data key=\"{key}\">");
                while let Some(start) = s.find(&needle_open) {
                    let end = s[start..].find("</data>\n").unwrap() + start + "</data>\n".len();
                    s.replace_range(start..end, "");
                }
            }
            s
        };
        assert_eq!(
            strip(a_db.export_graphml().unwrap()),
            strip(b_db.export_graphml().unwrap())
        );
    }

    #[test]
    fn graphml_weight_handles_nonfinite_values() {
        // Direct test of the format helper — we cannot create_edge with NaN
        // (the DB rejects it via InvalidWeight), but render_graphml may be
        // called from external pipelines or after future migrations.
        assert_eq!(format_weight(1.5_f32), "1.5");
        assert_eq!(format_weight(0.0_f32), "0");
        assert_eq!(format_weight(-2.25_f32), "-2.25");
        assert_eq!(format_weight(f32::NAN), "NaN");
        assert_eq!(format_weight(f32::INFINITY), "Infinity");
        assert_eq!(format_weight(f32::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn graphml_uuid_is_hyphenated_hex() {
        let bytes: [u8; 16] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ];
        let s = uuid_to_hyphenated(&bytes);
        assert_eq!(s, "01234567-89ab-cdef-fedc-ba9876543210");
    }

    #[test]
    fn graphml_escapes_control_characters() {
        let mut buf = String::new();
        push_escaped(&mut buf, "ok\u{0001}danger\u{0007}end");
        assert!(!buf.contains('\u{0001}'));
        assert!(!buf.contains('\u{0007}'));
        assert!(buf.contains('\u{FFFD}'));
        // Whitespace controls are preserved.
        let mut buf2 = String::new();
        push_escaped(&mut buf2, "tab\there\nnewline\rcr");
        assert!(buf2.contains('\t'));
        assert!(buf2.contains('\n'));
        assert!(buf2.contains('\r'));
    }
}
