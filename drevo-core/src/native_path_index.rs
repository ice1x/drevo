//! Opt-in indexes on **nested property paths**, kept current from a
//! [`NativeGraph`]'s change feed.
//!
//! The always-on [`NativePropertyIndex`] covers top-level properties only: a
//! map-valued property such as `meta: {author: "ann"}` is stored, but nothing
//! inside it is indexed, so `WHERE n.meta.author = 'ann'` scans every node. A
//! [`NativePathIndex`] indexes the values a set of declared
//! [`PathIndexSpec`]s point at:
//!
//! - one nested field — `meta.author`;
//! - every path under a map — `meta.*`;
//! - the whole property map — `*`;
//!
//! each optionally restricted to nodes carrying a label. Leaves are indexed
//! with the same rules as the top-level index: strings, booleans and integers
//! for equality, integers and floats for ranges. Lists are leaves; nothing
//! below a list is a path.
//!
//! Lookups return a **superset** of the matching node ids, or `None` when no
//! declared spec covers the path for the given labels — the caller then scans
//! and its exact filter decides. A spec for label `L` covers a lookup only when
//! the pattern requires `L`, because nodes without `L` were never indexed.
//!
//! ```
//! use drevo_core::engine::GraphEngine;
//! use drevo_core::model::{NewNode, Properties};
//! use drevo_core::native::NativeGraph;
//! use drevo_core::native_path_index::{NativePathIndex, PathIndexSpec, PropertyPath};
//! use serde_json::json;
//!
//! # fn main() -> drevo_core::error::Result<()> {
//! let graph = NativeGraph::new();
//! let doc = graph.create_node(NewNode {
//!     kind: "Doc".into(),
//!     title: "spec".into(),
//!     body: String::new(),
//!     body_html: String::new(),
//!     properties: Properties([("meta".to_string(), json!({"author": "ann"}))].into()),
//! })?;
//!
//! let author = vec!["meta".to_string(), "author".to_string()];
//! let mut idx = NativePathIndex::new(vec![PathIndexSpec {
//!     label: Some("Doc".into()),
//!     path: PropertyPath::new(author.clone(), false).expect("valid path"),
//! }]);
//! idx.sync(&graph);
//! let doc_label = ["Doc".to_string()];
//! assert_eq!(idx.node_ids(&doc_label, &author, &json!("ann")), Some(vec![doc.id]));
//! // Without the label the index cannot vouch for every node: scan instead.
//! assert_eq!(idx.node_ids(&[], &author, &json!("ann")), None);
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::engine::GraphEngine;
use crate::labels::{secondary_labels, SECONDARY_LABELS_KEY};
use crate::model::Node;
use crate::native::{NativeGraph, WalOp};
use crate::native_property_index::{NativePropertyIndex, RangeOp};

/// Separator between path segments in the index's internal keys. A control
/// character, so property names containing `.` cannot collide.
const KEY_SEPARATOR: char = '\u{1f}';

/// A property path: the segments from a node's property map down to a value,
/// optionally ending in a wildcard that stands for every path below it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PropertyPath {
    segments: Vec<String>,
    wildcard: bool,
}

/// Why a [`PropertyPath`] is invalid.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// An exact path needs at least one segment.
    #[error("a property path needs at least one segment")]
    Empty,
    /// A segment was the empty string.
    #[error("a property path segment cannot be empty")]
    EmptySegment,
}

impl PropertyPath {
    /// A path through `segments`; with `wildcard`, every path below them
    /// (`meta.*`), and with no segments, every path in the property map (`*`).
    ///
    /// # Errors
    /// [`PathError::Empty`] for an exact path with no segments,
    /// [`PathError::EmptySegment`] for an empty segment.
    pub fn new(segments: Vec<String>, wildcard: bool) -> Result<Self, PathError> {
        if segments.is_empty() && !wildcard {
            return Err(PathError::Empty);
        }
        if segments.iter().any(String::is_empty) {
            return Err(PathError::EmptySegment);
        }
        Ok(Self { segments, wildcard })
    }

    /// The segments before any wildcard.
    #[must_use]
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// Whether the path ends in a wildcard.
    #[must_use]
    pub fn is_wildcard(&self) -> bool {
        self.wildcard
    }

    /// Whether the path reaches below the top level. A single exact segment is
    /// a top-level property, which [`NativePropertyIndex`] always indexes.
    #[must_use]
    pub fn is_nested(&self) -> bool {
        self.wildcard || self.segments.len() > 1
    }

    /// Whether this path, as a declared index, covers the concrete nested
    /// `path` (top-level paths belong to [`NativePropertyIndex`]).
    fn covers(&self, path: &[String]) -> bool {
        if path.len() < 2 {
            false
        } else if self.wildcard {
            path.len() > self.segments.len() && path.starts_with(&self.segments)
        } else {
            path == self.segments.as_slice()
        }
    }
}

impl fmt::Display for PropertyPath {
    /// `meta.author`, `meta.*`, `*`; a segment that is not a plain identifier
    /// is backtick-quoted, as in Cypher (`` `a.b`.c ``).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for segment in &self.segments {
            if !first {
                f.write_str(".")?;
            }
            first = false;
            let plain = segment
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
                && segment.chars().all(|c| c.is_alphanumeric() || c == '_');
            if plain {
                f.write_str(segment)?;
            } else {
                write!(f, "`{}`", segment.replace('`', "``"))?;
            }
        }
        if self.wildcard {
            if !first {
                f.write_str(".")?;
            }
            f.write_str("*")?;
        }
        Ok(())
    }
}

/// One declared path index: a [`PropertyPath`], for nodes with `label` or, when
/// `label` is `None`, for every node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PathIndexSpec {
    /// Only nodes carrying this label (primary kind or secondary label) are
    /// indexed. `None` indexes every node.
    pub label: Option<String>,
    /// The indexed path.
    pub path: PropertyPath,
}

impl PathIndexSpec {
    /// Whether a node with `labels` falls under this spec.
    fn applies_to(&self, labels: &[String]) -> bool {
        self.label
            .as_ref()
            .is_none_or(|l| labels.iter().any(|have| have == l))
    }
}

/// Values at declared nested property paths, indexed for equality and range
/// lookups. See the [module docs](self).
#[derive(Default)]
pub struct NativePathIndex {
    specs: Vec<PathIndexSpec>,
    /// Postings keyed by the encoded leaf path (see [`encode_path`]).
    store: NativePropertyIndex,
    /// The change-feed cursor this index has consumed up to.
    cursor: u64,
}

impl fmt::Debug for NativePathIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativePathIndex")
            .field("specs", &self.specs)
            .field("nodes", &self.store.len())
            .field("cursor", &self.cursor)
            .finish()
    }
}

impl NativePathIndex {
    /// An index over `specs`, positioned before any change; call
    /// [`sync`](Self::sync) to populate it.
    #[must_use]
    pub fn new(specs: Vec<PathIndexSpec>) -> Self {
        Self {
            specs,
            store: NativePropertyIndex::new(),
            cursor: 0,
        }
    }

    /// The declared specs.
    #[must_use]
    pub fn specs(&self) -> &[PathIndexSpec] {
        &self.specs
    }

    /// The change-feed cursor this index has consumed up to.
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Replace the declared specs and rebuild from `graph`.
    pub fn set_specs(&mut self, specs: Vec<PathIndexSpec>, graph: &NativeGraph) {
        self.specs = specs;
        self.rebuild_from(graph);
        self.cursor = graph.change_head();
    }

    /// The internal key and the posting store serving `path` for a pattern
    /// that requires `labels`, or `None` when no spec covers it. Use with the
    /// [`NativePropertyIndex`] lookups (`node_ids`, `range_ids`).
    #[must_use]
    pub fn covering_key(
        &self,
        labels: &[String],
        path: &[String],
    ) -> Option<(&NativePropertyIndex, String)> {
        let covered = self.specs.iter().any(|spec| {
            spec.label.as_ref().is_none_or(|l| labels.contains(l)) && spec.path.covers(path)
        });
        covered.then(|| (&self.store, encode_path(path)))
    }

    /// Node ids whose value at `path` equals `value`, ascending; `None` when
    /// no spec covers `path` for a pattern requiring `labels`.
    #[must_use]
    pub fn node_ids(
        &self,
        labels: &[String],
        path: &[String],
        value: &JsonValue,
    ) -> Option<Vec<u64>> {
        let (store, key) = self.covering_key(labels, path)?;
        Some(store.node_ids(&key, value))
    }

    /// Node ids whose numeric value at `path` satisfies `value OP bound`, as a
    /// superset; `None` when no spec covers `path` or the bound is not served
    /// (see [`NativePropertyIndex::range_ids`]).
    #[must_use]
    pub fn range_ids(
        &self,
        labels: &[String],
        path: &[String],
        op: RangeOp,
        bound: &JsonValue,
    ) -> Option<BTreeSet<u64>> {
        let (store, key) = self.covering_key(labels, path)?;
        store.range_ids(&key, op, bound)
    }

    /// Bring the index up to date with `graph` by consuming its change feed
    /// since the last [`cursor`](Self::cursor). A feed trimmed past the cursor
    /// triggers a rebuild.
    pub fn sync(&mut self, graph: &NativeGraph) {
        if self.specs.is_empty() {
            self.cursor = graph.change_head();
            return;
        }
        let batch = graph.changes_since(self.cursor);
        if batch.lagged {
            self.rebuild_from(graph);
            self.cursor = graph.change_head().max(batch.cursor);
            return;
        }
        for op in batch.ops {
            match op {
                WalOp::UpsertNode(node) => self.index_node(&node),
                WalOp::DeleteNode(id) => self.store.remove_node(id),
                WalOp::UpsertEdge(_)
                | WalOp::DeleteEdge(_)
                | WalOp::SetEmbedding(..)
                | WalOp::DeleteEmbedding(_)
                | WalOp::Prepare { .. }
                | WalOp::CommitPrepared { .. }
                | WalOp::RollbackPrepared { .. } => {}
            }
        }
        self.cursor = batch.cursor;
    }

    fn rebuild_from(&mut self, graph: &NativeGraph) {
        self.store.clear();
        if self.specs.is_empty() {
            return;
        }
        if let Ok(nodes) = graph.all_nodes() {
            for node in &nodes {
                self.index_node(node);
            }
        }
    }

    /// Replace `node`'s postings with the leaves every applicable spec covers.
    fn index_node(&mut self, node: &Node) {
        self.store.remove_node(node.id);
        let mut labels = secondary_labels(node);
        labels.push(node.kind.clone());
        let mut leaves: Vec<(Vec<String>, &JsonValue)> = Vec::new();
        for spec in self.specs.iter().filter(|s| s.applies_to(&labels)) {
            collect_leaves(node, &spec.path, &mut leaves);
        }
        // Two specs can reach the same leaf (`meta.*` and `meta.author`).
        leaves.sort_by(|a, b| a.0.cmp(&b.0));
        leaves.dedup_by(|a, b| a.0 == b.0);
        self.store.insert_entries(
            node.id,
            leaves.into_iter().map(|(path, v)| (encode_path(&path), v)),
        );
    }
}

/// Push every leaf of `node` that `path` covers onto `out`.
fn collect_leaves<'n>(
    node: &'n Node,
    path: &PropertyPath,
    out: &mut Vec<(Vec<String>, &'n JsonValue)>,
) {
    let props = &node.properties.0;
    let Some((first, rest)) = path.segments.split_first() else {
        // `*`: everything nested in every top-level map property.
        for (key, value) in props {
            if key != SECONDARY_LABELS_KEY {
                if let JsonValue::Object(_) = value {
                    walk(value, &mut vec![key.clone()], out);
                }
            }
        }
        return;
    };
    let Some(mut value) = props.get(first) else {
        return;
    };
    for segment in rest {
        match value {
            JsonValue::Object(map) => match map.get(segment) {
                Some(next) => value = next,
                None => return,
            },
            _ => return,
        }
    }
    if path.wildcard {
        if let JsonValue::Object(_) = value {
            walk(value, &mut path.segments.clone(), out);
        }
    } else if path.segments.len() > 1 {
        out.push((path.segments.clone(), value));
    }
}

/// Push `value` (reached by `prefix`) and, recursively, everything under it.
/// A map is pushed too: it is never an equality posting, but like any
/// non-numeric value it blocks range scans on its path, so a range over a
/// path holding maps falls back to the exact filter (and its type errors)
/// exactly as a top-level property does.
fn walk<'n>(
    value: &'n JsonValue,
    prefix: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, &'n JsonValue)>,
) {
    if prefix.len() > 1 {
        out.push((prefix.clone(), value));
    }
    if let JsonValue::Object(map) = value {
        for (key, child) in map {
            prefix.push(key.clone());
            walk(child, prefix, out);
            prefix.pop();
        }
    }
}

/// The internal posting key for a leaf path.
fn encode_path(path: &[String]) -> String {
    let mut key = String::new();
    for (i, segment) in path.iter().enumerate() {
        if i > 0 {
            key.push(KEY_SEPARATOR);
        }
        key.push_str(segment);
    }
    key
}
