//! JSON import / export — Phase 9 hardening task `00055`. Phase 9 task
//! `00056` extends this module with read-only GraphML export
//! ([`crate::db::Drevo::export_graphml`] / `Drevo::export_graphml_to_path`
//! — the filesystem variant is gated off WASM).
//!
//! Provides a human-readable, schema-versioned dump format that captures the
//! entire graph (every node and every edge) and can be reloaded into any
//! [`crate::db::Drevo`] handle, regardless of which backend (memory or redb)
//! produced it or now receives it.
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
//!   [`crate::dump::DumpError::UnsupportedFormat`].
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
//! skipped and counted in [`crate::dump::ImportReport::nodes_skipped`] /
//! [`crate::dump::ImportReport::edges_skipped`]. A title collision against a
//! *different* node yields [`crate::error::DrevoError::DuplicateTitle`].
//!
//! ## Errors
//!
//! [`crate::dump::DumpError`] enumerates the import-time failure modes that
//! are independent of the storage layer (malformed JSON, unknown format,
//! mismatched schema). They surface to callers as
//! [`crate::error::DrevoError::Io`] because the JSON / file boundary is
//! conceptually an IO boundary; this avoids growing a new top-level variant
//! for a feature that lives one module deep.
//!
//! ## WASM
//!
//! [`crate::db::Drevo::export_json`] and [`crate::db::Drevo::import_json`] are
//! available on every target — they operate on `String` only and do not
//! touch the filesystem. `Drevo::export_json_to_path` /
//! `Drevo::import_json_from_path` are gated behind
//! `cfg(not(target_arch = "wasm32"))` because `std::fs` is not available in
//! the browser.
//!
//! ## GraphML export / import (tasks `00056` / `00057`)
//!
//! [`crate::db::Drevo::export_graphml`] emits the graph as a GraphML 1.0
//! document — the ubiquitous XML interchange format consumed by yEd, Gephi,
//! NetworkX, Cytoscape, igraph, and a long tail of network-analysis tooling.
//! [`crate::db::Drevo::import_graphml`] is its inverse: it parses a GraphML
//! document (drevo's own output, or any GraphML that follows the same
//! `<key>` / `<data>` conventions) back into a live database. The project's
//! authoritative wire format remains [`crate::dump::FORMAT_V1`]; GraphML is
//! offered for interop, and JSON stays the recommended backup channel.
//!
//! ### Import semantics
//!
//! * **Round-trip fidelity.** A document produced by `export_graphml` reloads
//!   verbatim: node/edge ids (`n<id>` / `e<id>`), uuids (`d_uuid`),
//!   timestamps (`d_created_at` / `d_updated_at`), kinds, titles, bodies and
//!   the JSON-encoded property maps are all preserved. Re-importing the same
//!   document is idempotent (rows are skipped, counted in
//!   [`crate::dump::ImportReport::nodes_skipped`] /
//!   [`crate::dump::ImportReport::edges_skipped`]), exactly like
//!   [`crate::db::Drevo::import_json`].
//! * **Interop tolerance.** GraphML from foreign tools rarely carries drevo's
//!   `d_*` keys. Data elements are therefore resolved by the `attr.name` of
//!   their `<key>` declaration, not the raw key id, so a foreign
//!   `attr.name="title"` maps onto [`crate::model::Node::title`]. Node ids
//!   that are not of the `n<u64>` form are remapped onto freshly-allocated
//!   ids (edges follow the remap); missing uuids/timestamps are generated at
//!   import time. Unrecognised `<data>` keys are folded into the node/edge
//!   property map so nothing is silently dropped.
//! * **Constraints.** Node titles must be unique (drevo's data-model
//!   invariant); an edge whose `source`/`target` names a node absent from the
//!   document is rejected as [`crate::dump::DumpError::MalformedGraphml`].
//!   Malformed XML, a missing `<graphml>`/`<graph>` element, or an id
//!   collision against different existing content surface as
//!   [`crate::error::DrevoError::Io`].
//!
//! The filesystem variant `Drevo::import_graphml_from_path` is gated off WASM.
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
//! Nested [`crate::model::Properties`] are serialised as a single
//! JSON-string `<data>` value (GraphML key type `string`) so the format
//! remains lossless and re-parsable by external tooling. The exporter is
//! deterministic: nodes / edges are emitted in id order
//! (`collect_all_nodes` / `collect_all_edges` already sort by id), and
//! [`crate::model::Properties`] sort their keys before serialising.
//!
//! Filesystem variant `Drevo::export_graphml_to_path` is gated off WASM.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::db::Drevo;
use crate::error::{DrevoError, Result};
use crate::model::{new_uuid_v7, now_ms, Edge, NewEdge, Node, Properties};

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
    /// A GraphML payload was not well-formed XML, was missing a required
    /// structural element (`<graphml>` / `<graph>` / a `<node>` id), or
    /// referenced an undeclared node from an `<edge>`.
    #[error("malformed GraphML: {0}")]
    MalformedGraphml(String),
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
    /// [`crate::model::Properties`] sorts its keys before
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

    /// Import a GraphML document produced by
    /// [`export_graphml`](Self::export_graphml) (or any GraphML that follows
    /// the same `<key>` / `<data>` conventions) into this database.
    ///
    /// The returned [`ImportReport`] separates newly-inserted rows from rows
    /// skipped because an identical id + content already exists — re-importing
    /// drevo's own export is therefore idempotent. See the module docs for the
    /// full round-trip / interop / constraint semantics.
    ///
    /// # Errors
    ///
    /// * [`DrevoError::Io`] — malformed XML, a missing `<graphml>`/`<graph>`
    ///   element, an `<edge>` referencing an undeclared node, or an id
    ///   collision against different existing content (all via
    ///   [`DumpError::MalformedGraphml`] / [`DumpError::IdCollision`]).
    /// * [`DrevoError::DuplicateTitle`] — an imported title clashes with a
    ///   different existing node.
    /// * [`DrevoError::Storage`] / [`DrevoError::Encode`] — backend failure.
    pub fn import_graphml(&self, xml: &str) -> Result<ImportReport> {
        let (nodes, edges) = self.graphml_to_records(xml)?;
        let next_node_id = nodes.iter().map(|n| n.id).max().map_or(1, |m| m + 1);
        let next_edge_id = edges.iter().map(|e| e.id).max().map_or(1, |m| m + 1);
        self.apply_dump(Dump {
            format: FORMAT_V1.to_string(),
            exported_at: now_ms(),
            next_node_id,
            next_edge_id,
            nodes,
            edges,
        })
    }

    /// Read a GraphML document from `path` (filesystem read, not available on
    /// WASM) and import it into this database. See
    /// [`import_graphml`](Self::import_graphml).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn import_graphml_from_path(&self, path: &std::path::Path) -> Result<ImportReport> {
        let raw = std::fs::read_to_string(path).map_err(DrevoError::Io)?;
        self.import_graphml(&raw)
    }

    // --- internals ---------------------------------------------------

    /// Parse a GraphML document into verbatim [`Node`] / [`Edge`] records
    /// ready to hand to [`apply_dump`](Self::apply_dump).
    ///
    /// Node ids of the form `n<u64>` (and edge ids `e<u64>`) are preserved;
    /// any other id is remapped onto a freshly-allocated id above both the
    /// preserved range and the ids already present in `self`, so a mixed
    /// document can never allocate over a preserved id. Edge `source`/`target`
    /// are resolved through the same node-id map.
    fn graphml_to_records(&self, xml: &str) -> Result<(Vec<Node>, Vec<Edge>)> {
        let roots = parse_xml(xml).map_err(DrevoError::from)?;
        let graphml = roots.iter().find(|e| e.name == "graphml").ok_or_else(|| {
            DrevoError::from(DumpError::MalformedGraphml(
                "no <graphml> root element".into(),
            ))
        })?;

        // Map each `<key id=…>` to its human-readable `attr.name` so `<data>`
        // elements can be interpreted by semantic name regardless of the id
        // scheme the producer chose.
        let mut keymap: HashMap<&str, &str> = HashMap::new();
        for k in graphml.children.iter().filter(|e| e.name == "key") {
            if let (Some(id), Some(name)) = (attr(&k.attrs, "id"), attr(&k.attrs, "attr.name")) {
                keymap.insert(id, name);
            }
        }

        let graph = graphml
            .children
            .iter()
            .find(|e| e.name == "graph")
            .ok_or_else(|| {
                DrevoError::from(DumpError::MalformedGraphml("no <graph> element".into()))
            })?;

        // --- Collect raw node / edge shells (document order) ---
        let mut raw_nodes: Vec<RawNode> = Vec::new();
        let mut raw_edges: Vec<RawEdge> = Vec::new();
        for child in &graph.children {
            match child.name.as_str() {
                "node" => {
                    let raw_id = attr(&child.attrs, "id").ok_or_else(|| {
                        DrevoError::from(DumpError::MalformedGraphml("<node> without id".into()))
                    })?;
                    raw_nodes.push(RawNode {
                        raw_id,
                        data: collect_data(child, &keymap),
                    });
                }
                "edge" => {
                    let source = attr(&child.attrs, "source").ok_or_else(|| {
                        DrevoError::from(DumpError::MalformedGraphml(
                            "<edge> without source".into(),
                        ))
                    })?;
                    let target = attr(&child.attrs, "target").ok_or_else(|| {
                        DrevoError::from(DumpError::MalformedGraphml(
                            "<edge> without target".into(),
                        ))
                    })?;
                    raw_edges.push(RawEdge {
                        raw_id: attr(&child.attrs, "id"),
                        source,
                        target,
                        data: collect_data(child, &keymap),
                    });
                }
                _ => {}
            }
        }

        // --- Assign final node ids (preserve `n<id>`, else allocate) ---
        let db_max_node = self
            .collect_all_nodes()?
            .iter()
            .map(|n| n.id)
            .max()
            .unwrap_or(0);
        let preserved_node: Vec<Option<u64>> = raw_nodes
            .iter()
            .map(|rn| parse_prefixed(rn.raw_id, 'n'))
            .collect();
        let max_preserved_node = preserved_node.iter().flatten().copied().max().unwrap_or(0);
        let mut next_alloc_node = db_max_node.max(max_preserved_node);
        let mut node_id_map: HashMap<&str, u64> = HashMap::new();
        for (rn, pres) in raw_nodes.iter().zip(preserved_node.iter()) {
            let id = match pres {
                Some(id) => *id,
                None => {
                    next_alloc_node += 1;
                    next_alloc_node
                }
            };
            node_id_map.insert(rn.raw_id, id);
        }

        let mut nodes = Vec::with_capacity(raw_nodes.len());
        for rn in &raw_nodes {
            let id = node_id_map[rn.raw_id];
            let mut kind = String::new();
            let mut title = String::new();
            let mut body = String::new();
            let mut body_html = String::new();
            let mut uuid: Option<[u8; 16]> = None;
            let mut created_at: Option<i64> = None;
            let mut updated_at: Option<i64> = None;
            let mut properties = Properties::default();
            for (name, value) in &rn.data {
                match name.as_str() {
                    "uuid" => uuid = parse_uuid(value),
                    "kind" => kind = value.clone(),
                    "title" => title = value.clone(),
                    "body" => body = value.clone(),
                    "body_html" => body_html = value.clone(),
                    "created_at" => created_at = value.parse::<i64>().ok(),
                    "updated_at" => updated_at = value.parse::<i64>().ok(),
                    "properties" => merge_properties(&mut properties, value),
                    other => fold_unknown_property(&mut properties, other, value),
                }
            }
            let created = created_at.unwrap_or_else(now_ms);
            nodes.push(Node {
                id,
                uuid: uuid.unwrap_or_else(new_uuid_v7),
                kind,
                title,
                body,
                body_html,
                created_at: created,
                updated_at: updated_at.unwrap_or(created),
                properties,
            });
        }

        // --- Assign final edge ids and resolve endpoints ---
        let db_max_edge = self
            .collect_all_edges()?
            .iter()
            .map(|e| e.id)
            .max()
            .unwrap_or(0);
        let preserved_edge: Vec<Option<u64>> = raw_edges
            .iter()
            .map(|re| re.raw_id.and_then(|s| parse_prefixed(s, 'e')))
            .collect();
        let max_preserved_edge = preserved_edge.iter().flatten().copied().max().unwrap_or(0);
        let mut next_alloc_edge = db_max_edge.max(max_preserved_edge);
        let mut edges = Vec::with_capacity(raw_edges.len());
        for (re, pres) in raw_edges.iter().zip(preserved_edge.iter()) {
            let from_id = *node_id_map.get(re.source).ok_or_else(|| {
                DrevoError::from(DumpError::MalformedGraphml(format!(
                    "edge source '{}' references an undeclared node",
                    re.source
                )))
            })?;
            let to_id = *node_id_map.get(re.target).ok_or_else(|| {
                DrevoError::from(DumpError::MalformedGraphml(format!(
                    "edge target '{}' references an undeclared node",
                    re.target
                )))
            })?;
            let eid = match pres {
                Some(id) => *id,
                None => {
                    next_alloc_edge += 1;
                    next_alloc_edge
                }
            };
            let mut kind = String::new();
            let mut uuid: Option<[u8; 16]> = None;
            let mut weight: Option<f32> = None;
            let mut created_at: Option<i64> = None;
            let mut properties = Properties::default();
            for (name, value) in &re.data {
                match name.as_str() {
                    "uuid" => uuid = parse_uuid(value),
                    "kind" => kind = value.clone(),
                    "weight" => weight = Some(parse_weight_value(value)),
                    "created_at" => created_at = value.parse::<i64>().ok(),
                    "properties" => merge_properties(&mut properties, value),
                    other => fold_unknown_property(&mut properties, other, value),
                }
            }
            edges.push(Edge {
                id: eid,
                uuid: uuid.unwrap_or_else(new_uuid_v7),
                from_id,
                to_id,
                kind,
                weight: weight.unwrap_or(1.0),
                created_at: created_at.unwrap_or_else(now_ms),
                properties,
            });
        }

        Ok((nodes, edges))
    }

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
        // Collect every node's storage writes and commit them in ONE
        // `put_batch` transaction. The per-record path used to `put` each of a
        // node's index entries individually — and FTS emits one entry per
        // trigram — so a text-heavy graph took hundreds of thousands of
        // per-record commits (one fsync each). Batching folds that into a
        // single fsync, turning a multi-minute restore/shrink into seconds.
        let mut node_writes: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut imported_nodes: Vec<&Node> = Vec::new();
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
            node_writes.extend(self.node_raw_entries(node)?);
            imported_nodes.push(node);
            report.nodes_imported += 1;
        }
        if !node_writes.is_empty() {
            self.backend().put_batch(&node_writes)?;
        }
        // #275: `node_raw_entries` no longer emits FTS entries (posting lists
        // need read-modify-write), so index the imported nodes' FTS in one
        // grouped, lock-guarded pass after their records are committed.
        if !imported_nodes.is_empty() {
            let docs: Vec<(u64, &str, &str, &crate::model::Properties)> = imported_nodes
                .iter()
                .map(|n| (n.id, n.title.as_str(), n.body.as_str(), &n.properties))
                .collect();
            self.fts_index_nodes(&docs)?;
        }

        // --- Edges ---
        // Nodes are already committed above, so the endpoint-existence checks
        // below see them. Edge writes are likewise batched into one commit.
        let mut edge_writes: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut imported_edges: Vec<&Edge> = Vec::new();
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
            // Both endpoints must exist (edges are inserted verbatim, bypassing
            // the allocating create path that would otherwise validate this).
            if self.get_node(edge.from_id)?.is_none() {
                return Err(DrevoError::NodeNotFound(edge.from_id));
            }
            if self.get_node(edge.to_id)?.is_none() {
                return Err(DrevoError::NodeNotFound(edge.to_id));
            }
            edge_writes.extend(self.edge_raw_entries(edge)?);
            imported_edges.push(edge);
            report.edges_imported += 1;
        }
        if !edge_writes.is_empty() {
            self.backend().put_batch(&edge_writes)?;
        }
        // #275: index the imported edges' `efts:` posting lists in one grouped,
        // lock-guarded pass (edge_raw_entries doesn't emit FTS — posting lists
        // need read-modify-write). This also gives shrunk/restored files their
        // relationship FTS, which the record-only import path omitted.
        if !imported_edges.is_empty() {
            let docs: Vec<(u64, &crate::model::Properties)> = imported_edges
                .iter()
                .map(|e| (e.id, &e.properties))
                .collect();
            self.efts_index_edges(&docs)?;
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

// ---------------------------------------------------------------------
// GraphML parsing (task 00057) — a small, dependency-free XML reader
// tailored to the GraphML the exporter emits. The workspace deliberately
// avoids a general XML crate ("embeddable, no external system deps"), and the
// exporter escapes every `<`/`>`/`&` in element text, so a structural
// scanner is safe: the only real tags inside the body are the GraphML ones.
// ---------------------------------------------------------------------

/// A minimal parsed XML element — just enough tree for the GraphML importer.
struct XmlElement {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<XmlElement>,
    text: String,
}

/// A `<node>` shell parsed from GraphML, before id allocation. `data` holds
/// `(semantic-name, value)` pairs resolved via the `<key>` declarations.
struct RawNode<'a> {
    raw_id: &'a str,
    data: Vec<(String, String)>,
}

/// An `<edge>` shell parsed from GraphML, before id allocation and endpoint
/// resolution. `raw_id` is optional (GraphML edges may omit an id).
struct RawEdge<'a> {
    raw_id: Option<&'a str>,
    source: &'a str,
    target: &'a str,
    data: Vec<(String, String)>,
}

/// Look up an attribute value by name (first match wins).
fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Collect a node/edge element's `<data key=…>value</data>` children, mapping
/// each key id to its semantic `attr.name` via `keymap` (falling back to the
/// raw key id when the producer declared no matching `<key>`).
fn collect_data(elem: &XmlElement, keymap: &HashMap<&str, &str>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for d in elem.children.iter().filter(|c| c.name == "data") {
        if let Some(key) = attr(&d.attrs, "key") {
            let semantic = keymap.get(key).copied().unwrap_or(key);
            out.push((semantic.to_string(), d.text.clone()));
        }
    }
    out
}

/// Parse a GraphML/XML document into its top-level elements. Skips the XML
/// declaration, comments, DOCTYPE, and processing instructions; unescapes
/// entity references in text and attribute values; understands CDATA.
fn parse_xml(input: &str) -> std::result::Result<Vec<XmlElement>, DumpError> {
    let mut roots: Vec<XmlElement> = Vec::new();
    let mut stack: Vec<XmlElement> = Vec::new();
    let bytes = input.as_bytes();
    let mut pos = 0usize;
    while pos < input.len() {
        let lt = match input[pos..].find('<') {
            Some(rel) => pos + rel,
            None => break,
        };
        if lt > pos {
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&xml_unescape(&input[pos..lt])?);
            }
        }
        let rest = &input[lt..];
        if rest.starts_with("<!--") {
            let end = input[lt + 4..]
                .find("-->")
                .ok_or_else(|| DumpError::MalformedGraphml("unterminated comment".into()))?;
            pos = lt + 4 + end + 3;
        } else if rest.starts_with("<![CDATA[") {
            let end = input[lt + 9..]
                .find("]]>")
                .ok_or_else(|| DumpError::MalformedGraphml("unterminated CDATA".into()))?;
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&input[lt + 9..lt + 9 + end]);
            }
            pos = lt + 9 + end + 3;
        } else if rest.starts_with("<?") {
            let end = input[lt + 2..].find("?>").ok_or_else(|| {
                DumpError::MalformedGraphml("unterminated processing instruction".into())
            })?;
            pos = lt + 2 + end + 2;
        } else if rest.starts_with("<!") {
            let end = input[lt..]
                .find('>')
                .ok_or_else(|| DumpError::MalformedGraphml("unterminated declaration".into()))?;
            pos = lt + end + 1;
        } else if rest.starts_with("</") {
            let end = input[lt..]
                .find('>')
                .ok_or_else(|| DumpError::MalformedGraphml("unterminated close tag".into()))?;
            let name = input[lt + 2..lt + end].trim();
            let elem = stack.pop().ok_or_else(|| {
                DumpError::MalformedGraphml(format!("unexpected close tag </{name}>"))
            })?;
            if elem.name != name {
                return Err(DumpError::MalformedGraphml(format!(
                    "mismatched close tag: expected </{}>, found </{name}>",
                    elem.name
                )));
            }
            match stack.last_mut() {
                Some(parent) => parent.children.push(elem),
                None => roots.push(elem),
            }
            pos = lt + end + 1;
        } else {
            let (gt, self_closing) = find_tag_end(bytes, lt)?;
            let inner_end = if self_closing { gt - 1 } else { gt };
            let (name, attrs) = parse_tag(&input[lt + 1..inner_end])?;
            let elem = XmlElement {
                name,
                attrs,
                children: Vec::new(),
                text: String::new(),
            };
            if self_closing {
                match stack.last_mut() {
                    Some(parent) => parent.children.push(elem),
                    None => roots.push(elem),
                }
            } else {
                stack.push(elem);
            }
            pos = gt + 1;
        }
    }
    if let Some(open) = stack.last() {
        return Err(DumpError::MalformedGraphml(format!(
            "unclosed element <{}>",
            open.name
        )));
    }
    Ok(roots)
}

/// Locate the `>` that closes the tag opened at byte `lt`, honouring quoted
/// attribute values (which may legally contain `>`). Returns the `>` index and
/// whether the tag is self-closing (`… />`). All structural characters
/// (`<>"'/`) are ASCII, so byte scanning is UTF-8-safe.
fn find_tag_end(bytes: &[u8], lt: usize) -> std::result::Result<(usize, bool), DumpError> {
    let mut i = lt + 1;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                b'"' | b'\'' => quote = Some(c),
                b'>' => {
                    let mut self_closing = false;
                    let mut k = i;
                    while k > lt + 1 {
                        k -= 1;
                        match bytes[k] {
                            b' ' | b'\t' | b'\n' | b'\r' => continue,
                            other => {
                                self_closing = other == b'/';
                                break;
                            }
                        }
                    }
                    return Ok((i, self_closing));
                }
                _ => {}
            },
        }
        i += 1;
    }
    Err(DumpError::MalformedGraphml("unterminated tag".into()))
}

/// Split a tag's interior (`name attr="v" …`, sans `<`, `>` and any trailing
/// `/`) into its element name and unescaped attribute pairs.
fn parse_tag(inner: &str) -> std::result::Result<(String, Vec<(String, String)>), DumpError> {
    let inner = inner.trim();
    let mut it = inner.splitn(2, char::is_whitespace);
    let name = it.next().unwrap_or("").trim().to_string();
    if name.is_empty() {
        return Err(DumpError::MalformedGraphml("empty tag name".into()));
    }
    let mut attrs = Vec::new();
    if let Some(rest) = it.next() {
        let mut s = rest.trim_start();
        while !s.is_empty() {
            let eq = s.find('=').ok_or_else(|| {
                DumpError::MalformedGraphml(format!("attribute without '=' in <{name}>"))
            })?;
            let aname = s[..eq].trim().to_string();
            let after_eq = s[eq + 1..].trim_start();
            let quote = after_eq.chars().next().ok_or_else(|| {
                DumpError::MalformedGraphml(format!("attribute '{aname}' missing value"))
            })?;
            if quote != '"' && quote != '\'' {
                return Err(DumpError::MalformedGraphml(format!(
                    "attribute '{aname}' value is not quoted"
                )));
            }
            let after_q = &after_eq[1..];
            let close = after_q.find(quote).ok_or_else(|| {
                DumpError::MalformedGraphml(format!("unterminated value for attribute '{aname}'"))
            })?;
            attrs.push((aname, xml_unescape(&after_q[..close])?));
            s = after_q[close + 1..].trim_start();
        }
    }
    Ok((name, attrs))
}

/// Inverse of the exporter's [`push_escaped`]: turn XML entity references back
/// into their characters. Handles the five predefined entities plus decimal
/// and hexadecimal numeric character references. A single left-to-right pass
/// so already-decoded output is never re-decoded.
fn xml_unescape(s: &str) -> std::result::Result<String, DumpError> {
    if !s.contains('&') {
        return Ok(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let semi = after
            .find(';')
            .ok_or_else(|| DumpError::MalformedGraphml("unterminated entity reference".into()))?;
        let entity = &after[1..semi];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                let code = u32::from_str_radix(&entity[2..], 16)
                    .map_err(|_| DumpError::MalformedGraphml(format!("bad char ref &{entity};")))?;
                out.push(char::from_u32(code).ok_or_else(|| {
                    DumpError::MalformedGraphml(format!("invalid code point &{entity};"))
                })?);
            }
            _ if entity.starts_with('#') => {
                let code = entity[1..]
                    .parse::<u32>()
                    .map_err(|_| DumpError::MalformedGraphml(format!("bad char ref &{entity};")))?;
                out.push(char::from_u32(code).ok_or_else(|| {
                    DumpError::MalformedGraphml(format!("invalid code point &{entity};"))
                })?);
            }
            other => {
                return Err(DumpError::MalformedGraphml(format!(
                    "unknown entity reference &{other};"
                )))
            }
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Parse a canonical hyphenated UUID (as emitted by [`uuid_to_hyphenated`])
/// back into raw bytes. Returns `None` on any malformed value so the caller
/// can fall back to generating a fresh uuid.
fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    uuid::Uuid::parse_str(s).ok().map(|u| *u.as_bytes())
}

/// Parse a `<prefix><u64>` id (e.g. `n42`, `e7`) into its numeric part.
/// Returns `None` for any other shape so the caller allocates a fresh id.
fn parse_prefixed(s: &str, prefix: char) -> Option<u64> {
    let rest = s.strip_prefix(prefix)?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<u64>().ok()
}

/// Inverse of [`format_weight`]: parse an edge-weight `<data>` value, decoding
/// the non-finite sentinels the exporter emits. Unparseable values default to
/// `1.0` (drevo's neutral edge weight).
fn parse_weight_value(s: &str) -> f32 {
    match s {
        "NaN" => f32::NAN,
        "Infinity" => f32::INFINITY,
        "-Infinity" => f32::NEG_INFINITY,
        other => other.parse::<f32>().unwrap_or(1.0),
    }
}

/// Merge a JSON-object `<data>` value (the `d_props` / `d_e_props` payload)
/// into `properties`. A value that is not a JSON object is stored verbatim
/// under a `"properties"` key so nothing is dropped.
fn merge_properties(properties: &mut Properties, value: &str) {
    match serde_json::from_str::<Properties>(value) {
        Ok(parsed) => {
            for (k, v) in parsed.0 {
                properties.0.insert(k, v);
            }
        }
        Err(_) => {
            properties.0.insert(
                "properties".to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }
}

/// Fold an unrecognised `<data>` key (foreign GraphML) into the property map,
/// parsing the value as JSON when possible and otherwise keeping it as a
/// string. Ensures interop imports never silently discard attributes.
fn fold_unknown_property(properties: &mut Properties, name: &str, value: &str) {
    let parsed = serde_json::from_str::<serde_json::Value>(value)
        .unwrap_or_else(|_| serde_json::Value::String(value.to_string()));
    properties.0.insert(name.to_string(), parsed);
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

    // -----------------------------------------------------------------
    // GraphML import (task 00057) — unit tests
    // -----------------------------------------------------------------

    #[test]
    fn xml_unescape_decodes_predefined_and_numeric_entities() {
        assert_eq!(
            xml_unescape("a &lt; b &amp; c &gt; d &quot; &apos;").unwrap(),
            "a < b & c > d \" '"
        );
        // A JSON literal round-trips through the escaper.
        assert_eq!(xml_unescape("{&quot;k&quot;:1}").unwrap(), "{\"k\":1}");
        // Numeric character references (decimal + hex).
        assert_eq!(xml_unescape("&#65;&#x42;&#x1F333;").unwrap(), "AB🌳");
        // No ampersand — identity fast path.
        assert_eq!(xml_unescape("plain text").unwrap(), "plain text");
    }

    #[test]
    fn xml_unescape_rejects_unknown_and_unterminated_entities() {
        assert!(xml_unescape("&bogus;").is_err());
        assert!(xml_unescape("a & b").is_err());
    }

    #[test]
    fn parse_prefixed_only_matches_prefix_plus_digits() {
        assert_eq!(parse_prefixed("n42", 'n'), Some(42));
        assert_eq!(parse_prefixed("e0", 'e'), Some(0));
        assert_eq!(parse_prefixed("node7", 'n'), None); // extra letters
        assert_eq!(parse_prefixed("n", 'n'), None); // no digits
        assert_eq!(parse_prefixed("x1", 'n'), None); // wrong prefix
    }

    #[test]
    fn parse_weight_value_inverts_format_weight() {
        assert_eq!(parse_weight_value("1.5"), 1.5_f32);
        assert_eq!(parse_weight_value("0"), 0.0_f32);
        assert!(parse_weight_value("NaN").is_nan());
        assert_eq!(parse_weight_value("Infinity"), f32::INFINITY);
        assert_eq!(parse_weight_value("-Infinity"), f32::NEG_INFINITY);
        assert_eq!(parse_weight_value("garbage"), 1.0_f32); // default
    }

    #[test]
    fn parse_xml_skips_declaration_comments_and_pi() {
        let xml = "<?xml version=\"1.0\"?>\n<!-- a comment -->\n<r a=\"1\"><c/></r>";
        let roots = parse_xml(xml).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "r");
        assert_eq!(attr(&roots[0].attrs, "a"), Some("1"));
        assert_eq!(roots[0].children.len(), 1);
        assert_eq!(roots[0].children[0].name, "c");
    }

    #[test]
    fn parse_xml_rejects_mismatched_and_unclosed_tags() {
        assert!(parse_xml("<a></b>").is_err());
        assert!(parse_xml("<a><b></a>").is_err());
        assert!(parse_xml("<a>").is_err());
    }

    #[test]
    fn graphml_round_trips_through_export_import() {
        let src = Drevo::open_in_memory().unwrap();
        let a = src
            .create_node(NewNode {
                kind: "note".into(),
                title: "Alpha".into(),
                body: "first".into(),
                body_html: "<p>first</p>".into(),
                properties: props(&[("n", json!(1)), ("s", json!("x"))]),
            })
            .unwrap();
        let b = src
            .create_node(NewNode {
                kind: "tag".into(),
                title: "Beta < & >".into(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        src.create_edge(NewEdge {
            from_id: a.id,
            to_id: b.id,
            kind: "links_to".into(),
            weight: 2.5,
            properties: props(&[("color", json!("red"))]),
        })
        .unwrap();

        let xml = src.export_graphml().unwrap();
        let dst = Drevo::open_in_memory().unwrap();
        let report = dst.import_graphml(&xml).unwrap();
        assert_eq!(report.nodes_imported, 2);
        assert_eq!(report.edges_imported, 1);

        // Re-exporting the destination yields byte-identical GraphML.
        assert_eq!(dst.export_graphml().unwrap(), xml);
    }

    #[test]
    fn graphml_import_preserves_ids_uuid_and_timestamps() {
        let src = Drevo::open_in_memory().unwrap();
        let n = src
            .create_node(NewNode {
                kind: "note".into(),
                title: "Keep Me".into(),
                body: "body".into(),
                body_html: String::new(),
                properties: props(&[("k", json!(9))]),
            })
            .unwrap();
        let original = src.get_node(n.id).unwrap().unwrap();

        let xml = src.export_graphml().unwrap();
        let dst = Drevo::open_in_memory().unwrap();
        dst.import_graphml(&xml).unwrap();

        let restored = dst.get_node(n.id).unwrap().unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn graphml_import_is_idempotent() {
        let src = Drevo::open_in_memory().unwrap();
        src.create_node(NewNode {
            kind: "note".into(),
            title: "Once".into(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();
        let xml = src.export_graphml().unwrap();

        let dst = Drevo::open_in_memory().unwrap();
        let first = dst.import_graphml(&xml).unwrap();
        assert_eq!(first.nodes_imported, 1);
        let second = dst.import_graphml(&xml).unwrap();
        assert_eq!(second.nodes_imported, 0);
        assert_eq!(second.nodes_skipped, 1);
    }

    #[test]
    fn graphml_import_rejects_malformed_xml() {
        let db = Drevo::open_in_memory().unwrap();
        let err = db.import_graphml("<graphml><graph><node id=").unwrap_err();
        assert!(matches!(err, DrevoError::Io(_)));
    }

    #[test]
    fn graphml_import_rejects_missing_graphml_root() {
        let db = Drevo::open_in_memory().unwrap();
        let err = db.import_graphml("<not-graphml/>").unwrap_err();
        assert!(matches!(err, DrevoError::Io(_)));
    }

    #[test]
    fn graphml_import_rejects_edge_to_undeclared_node() {
        let db = Drevo::open_in_memory().unwrap();
        let xml = "<graphml><graph>\
             <node id=\"n1\"><data key=\"kind\">note</data><data key=\"title\">A</data></node>\
             <edge id=\"e1\" source=\"n1\" target=\"n999\"/>\
             </graph></graphml>";
        let err = db.import_graphml(xml).unwrap_err();
        assert!(matches!(err, DrevoError::Io(_)));
    }

    #[test]
    fn graphml_import_foreign_document_allocates_ids_and_maps_attr_names() {
        // A foreign GraphML: string node ids, keys referenced by declared
        // `attr.name`, no uuids/timestamps. drevo must allocate ids, remap the
        // edge endpoints, and interpret data by attr.name.
        let db = Drevo::open_in_memory().unwrap();
        let xml = "<?xml version=\"1.0\"?>\
             <graphml>\
             <key id=\"k0\" for=\"node\" attr.name=\"title\" attr.type=\"string\"/>\
             <key id=\"k1\" for=\"node\" attr.name=\"kind\" attr.type=\"string\"/>\
             <key id=\"k2\" for=\"node\" attr.name=\"weight_of_life\" attr.type=\"string\"/>\
             <graph edgedefault=\"directed\">\
             <node id=\"alice\"><data key=\"k0\">Alice</data><data key=\"k1\">person</data><data key=\"k2\">42</data></node>\
             <node id=\"bob\"><data key=\"k0\">Bob</data><data key=\"k1\">person</data></node>\
             <edge source=\"alice\" target=\"bob\"><data key=\"k1\">knows</data></edge>\
             </graph></graphml>";
        let report = db.import_graphml(xml).unwrap();
        assert_eq!(report.nodes_imported, 2);
        assert_eq!(report.edges_imported, 1);

        // Ids were allocated 1,2; title/kind mapped via attr.name; the
        // unrecognised `weight_of_life` key folded into properties.
        let alice = db.get_node(1).unwrap().unwrap();
        assert_eq!(alice.title, "Alice");
        assert_eq!(alice.kind, "person");
        assert_eq!(alice.properties.get("weight_of_life"), Some(&json!(42)));

        let edges = db.collect_all_edges().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from_id, 1);
        assert_eq!(edges[0].to_id, 2);
        assert_eq!(edges[0].kind, "knows");
        assert_eq!(edges[0].weight, 1.0); // default weight
    }
}
