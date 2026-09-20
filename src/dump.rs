//! JSON import / export — Phase 9 hardening task `00055`. Phase 9 task
//! `00056` extends this module with read-only GraphML export
//! (`Drevo::export_graphml` / `Drevo::export_graphml_to_path`
//! — the filesystem variant is gated off WASM).
//!
//! Provides a human-readable, schema-versioned dump format that captures the
//! entire graph (every node and every edge) and can be reloaded into any
//! `Drevo` handle, regardless of which backend (memory or redb)
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
//! `Drevo::export_json` and `Drevo::import_json` are
//! available on every target — they operate on `String` only and do not
//! touch the filesystem. `Drevo::export_json_to_path` /
//! `Drevo::import_json_from_path` are gated behind
//! `cfg(not(target_arch = "wasm32"))` because `std::fs` is not available in
//! the browser.
//!
//! ## GraphML export / import (tasks `00056` / `00057`)
//!
//! `Drevo::export_graphml` emits the graph as a GraphML 1.0
//! document — the ubiquitous XML interchange format consumed by yEd, Gephi,
//! NetworkX, Cytoscape, igraph, and a long tail of network-analysis tooling.
//! `Drevo::import_graphml` is its inverse: it parses a GraphML
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
//!   `Drevo::import_json`.
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

use crate::error::{DrevoError, Result};
use crate::model::{new_uuid_v7, now_ms, Edge, NewEdge, Node, Properties};

// The `drevo-json-v1` wire-format types — `FORMAT_V1`, `Dump`, `ImportReport`,
// and `DumpError` — were extracted to the `drevo-core` crate (Phase 7 slice 4)
// so the native engine can produce and consume them directly. They are
// re-exported here so existing `crate::dump::Dump` / `drevo::dump::Dump` paths
// keep resolving unchanged; the KV-specific JSON/GraphML rendering + parsing
// and the filesystem/HTTP entry points below stay in this crate.
pub use drevo_core::dump::{Dump, DumpError, ImportReport, FORMAT_V1};

/// Lift a dump-format failure into the main crate's [`DrevoError`].
///
/// Kept in this crate (alongside `DrevoError`) so the KV file-I/O methods below
/// can `?`-lift a [`DumpError`] straight to [`DrevoError::Io`], preserving the
/// human-readable message — `{err}` includes the original failure mode. (The
/// core crate has its own `From<DumpError> for CoreError` for the native path.)
impl From<DumpError> for DrevoError {
    fn from(err: DumpError) -> Self {
        DrevoError::Io(std::io::Error::other(err.to_string()))
    }
}

/// Parse a GraphML document into node/edge records — the engine-independent
/// half of `Drevo::import_graphml`, shared with the
/// durable-native import (`crate::native_service`). `db_max_node` /
/// `db_max_edge` are the target store's current maximum ids, used to
/// allocate ids for records whose GraphML ids do not follow the `n<id>` /
/// `e<id>` convention.
///
/// # Errors
///
/// [`crate::error::DrevoError::Io`] via [`DumpError::MalformedGraphml`] on
/// malformed XML or structure, exactly as the KV import reports them.
pub(crate) fn graphml_records(
    xml: &str,
    db_max_node: u64,
    db_max_edge: u64,
) -> Result<(Vec<Node>, Vec<Edge>)> {
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
                    DrevoError::from(DumpError::MalformedGraphml("<edge> without source".into()))
                })?;
                let target = attr(&child.attrs, "target").ok_or_else(|| {
                    DrevoError::from(DumpError::MalformedGraphml("<edge> without target".into()))
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
pub(crate) fn render_graphml(nodes: &[Node], edges: &[Edge]) -> Result<String> {
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

    #[test]
    fn import_report_default_is_all_zero() {
        let r = ImportReport::default();
        assert_eq!(r.nodes_imported, 0);
        assert_eq!(r.edges_imported, 0);
        assert_eq!(r.nodes_skipped, 0);
        assert_eq!(r.edges_skipped, 0);
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
}
