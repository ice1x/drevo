//! An in-memory **property-value** index that tails a
//! [`NativeGraph`](crate::native::NativeGraph)'s change-feed
//! (RFC `docs/rfc-native-core.md`, #307, Phase 6.7).
//!
//! Maps every indexable `(property key, property value)` pair a node carries to
//! the ids that hold it, so a Cypher equality pattern such as
//! `MATCH (n {status: "open"})` resolves through an `O(matches)` lookup instead
//! of an `O(N)` full-node scan. It is the native counterpart of the KV store's
//! [`property_index`](crate::property_index) (`Drevo::nodes_by_property`), kept —
//! like the trigram FTS and the secondary-label index — off the core graph seam
//! and current by **tailing the change-feed** (see
//! [`NativeGraph::changes_since`](crate::native::NativeGraph::changes_since)).
//!
//! # Which values are indexed
//!
//! Only **strings, booleans, and integers** are indexed, and only those are
//! looked up. This is deliberate: the executor turns a pattern value back into
//! JSON to probe the index, and that round-trip is byte-exact only for these
//! types. Restricting both the index and the probe to them guarantees the index
//! is a true **superset** of the matches (never a false negative) — floats,
//! lists, maps, and null fall back to the full scan, and the executor's exact
//! per-candidate check runs regardless. The reserved `_labels` property is
//! skipped (the [`NativeLabelIndex`](crate::native_label_index::NativeLabelIndex)
//! owns it).
//!
//! # Usage
//!
//! Snapshot-then-tail: build the index, then
//! [`sync`](crate::native_property_index::NativePropertyIndex::sync) after each
//! batch of writes; a feed trimmed past the cursor triggers a transparent
//! rebuild.
//!
//! ```
//! use drevo::native::NativeGraph;
//! use drevo::native_property_index::NativePropertyIndex;
//! use drevo::engine::GraphEngine; // brings `create_node` into scope
//! use drevo::model::{NewNode, Properties};
//! use std::collections::HashMap;
//!
//! # fn main() -> drevo::error::Result<()> {
//! let g = NativeGraph::new();
//! let props = Properties(HashMap::from([(
//!     "status".to_string(),
//!     serde_json::json!("open"),
//! )]));
//! let n = g.create_node(NewNode { kind: "task".into(), title: "t".into(),
//!     body: String::new(), body_html: String::new(), properties: props })?;
//!
//! let mut idx = NativePropertyIndex::new();
//! idx.sync(&g);
//! assert_eq!(idx.node_ids("status", &serde_json::json!("open")), vec![n.id]);
//! assert!(idx.node_ids("status", &serde_json::json!("closed")).is_empty());
//! # Ok(())
//! # }
//! ```

use std::collections::{BTreeSet, HashMap};

use serde_json::Value as JsonValue;

use crate::engine::GraphEngine;
use crate::model::Node;
use crate::native::{NativeGraph, WalOp};
use crate::property_index::encode_value;

/// Reserved property key holding a node's secondary Cypher labels — owned by the
/// [`NativeLabelIndex`](crate::native_label_index::NativeLabelIndex), so this
/// index skips it.
const SECONDARY_LABELS_KEY: &str = "_labels";

/// Whether `value` is one of the types this index stores and can be probed for
/// with a byte-exact round-trip (string, bool, or an `i64` integer).
///
/// The executor uses this to decide which inline pattern properties may narrow
/// candidates through the index: a non-indexable value (float, list, map, null)
/// must be skipped rather than looked up, since its empty posting would wrongly
/// drop real matches instead of falling through to the exact per-candidate
/// check.
pub fn is_indexable(value: &JsonValue) -> bool {
    match value {
        JsonValue::String(_) | JsonValue::Bool(_) => true,
        JsonValue::Number(n) => n.is_i64(),
        _ => false,
    }
}

/// A property-value index maintained by tailing a [`NativeGraph`]'s change-feed.
/// See the module docs.
#[derive(Default)]
pub struct NativePropertyIndex {
    /// property key → (canonical value bytes → ids carrying it, ascending).
    postings: HashMap<String, HashMap<Vec<u8>, BTreeSet<u64>>>,
    /// node id → the `(key, value-bytes)` pairs it was indexed under (the
    /// forward index, so a node can be removed or re-indexed cheaply).
    docs: HashMap<u64, Vec<(String, Vec<u8>)>>,
    /// The change-feed cursor this index has consumed up to.
    cursor: u64,
}

impl NativePropertyIndex {
    /// Create an empty index positioned before any change.
    pub fn new() -> Self {
        Self::default()
    }

    /// The change-feed cursor this index has consumed up to.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// The number of nodes that carry at least one indexable property.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// Whether no node carries an indexable property.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Node ids whose `key` property equals `value`, ascending. Empty when none
    /// match or when `value` is not an indexable type (the caller then falls
    /// back to a full scan, so this can only ever be a superset gap the exact
    /// per-candidate check would close anyway).
    pub fn node_ids(&self, key: &str, value: &JsonValue) -> Vec<u64> {
        if !is_indexable(value) {
            return Vec::new();
        }
        let Ok(bytes) = encode_value(value) else {
            return Vec::new();
        };
        self.postings
            .get(key)
            .and_then(|by_val| by_val.get(&bytes))
            .map(|ids| ids.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Bring the index up to date with `graph` by consuming its change-feed
    /// since the last [`cursor`](Self::cursor). A feed trimmed past the cursor
    /// (a `lagged` batch) triggers a rebuild from a fresh snapshot.
    pub fn sync(&mut self, graph: &NativeGraph) {
        let batch = graph.changes_since(self.cursor);
        if batch.lagged {
            self.rebuild_from(graph);
            self.cursor = graph.change_head().max(batch.cursor);
            return;
        }
        for op in batch.ops {
            match op {
                WalOp::UpsertNode(node) => self.index_node(&node),
                WalOp::DeleteNode(id) => self.remove_node(id),
                // Edge properties are not indexed here.
                WalOp::UpsertEdge(_) | WalOp::DeleteEdge(_) => {}
            }
        }
        self.cursor = batch.cursor;
    }

    // ----- maintenance -------------------------------------------------------

    /// Discard everything and re-index every node in `graph`.
    fn rebuild_from(&mut self, graph: &NativeGraph) {
        self.postings.clear();
        self.docs.clear();
        if let Ok(nodes) = graph.all_nodes() {
            for node in &nodes {
                self.index_node(node);
            }
        }
    }

    /// Insert or replace a node's indexable `(key, value)` postings. A node with
    /// no indexable property is not tracked.
    fn index_node(&mut self, node: &Node) {
        self.remove_node(node.id);
        let mut pairs: Vec<(String, Vec<u8>)> = Vec::new();
        for (key, value) in node.properties.0.iter() {
            if key == SECONDARY_LABELS_KEY || !is_indexable(value) {
                continue;
            }
            let Ok(bytes) = encode_value(value) else {
                continue;
            };
            self.postings
                .entry(key.clone())
                .or_default()
                .entry(bytes.clone())
                .or_default()
                .insert(node.id);
            pairs.push((key.clone(), bytes));
        }
        if !pairs.is_empty() {
            self.docs.insert(node.id, pairs);
        }
    }

    /// Remove a node's postings, if present, dropping any bucket that empties.
    fn remove_node(&mut self, id: u64) {
        let Some(pairs) = self.docs.remove(&id) else {
            return;
        };
        for (key, bytes) in &pairs {
            if let Some(by_val) = self.postings.get_mut(key) {
                if let Some(ids) = by_val.get_mut(bytes) {
                    ids.remove(&id);
                    if ids.is_empty() {
                        by_val.remove(bytes);
                    }
                }
                if by_val.is_empty() {
                    self.postings.remove(key);
                }
            }
        }
    }
}
