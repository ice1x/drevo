//! An in-memory **secondary-label** index that tails a
//! [`NativeGraph`](crate::native::NativeGraph)'s change-feed
//! (RFC `docs/rfc-native-core.md`, #307, Phase 6.6).
//!
//! drevo storage gives each node a single primary `kind`; Cypher allows any
//! number of labels, and the extras added via `SET n:Label` live in a reserved
//! `_labels` property (a JSON array of strings — parsed by the executor's
//! `secondary_labels`, the single source of truth this index reuses). The native
//! engine's `nodes_by_kind` already
//! indexes the primary kind, but nothing indexed the secondary labels — so a
//! `MATCH (n:Label)` on the native engine had to fall back to a full scan of
//! every node to catch `_labels` matches.
//!
//! This index closes that gap. Like the trigram FTS (`NativeFtsIndex`, in the
//! main crate), it is a secondary index kept off the core graph seam and current
//! by **tailing the change-feed**
//! (see [`NativeGraph::changes_since`](crate::native::NativeGraph::changes_since))
//! rather than coupling to the write path. Combined with the primary-kind index,
//! it lets the executor gather label candidates as
//! `nodes_by_kind(label) ∪ node_ids(label)` — never a full scan.
//!
//! # Usage
//!
//! Snapshot-then-tail: build the index, then
//! [`sync`](crate::native_label_index::NativeLabelIndex::sync) after each batch
//! of writes. `sync` applies every change since the last cursor; if the feed was
//! trimmed past the cursor it transparently rebuilds from a fresh snapshot.
//!
//! ```
//! use drevo_core::native::NativeGraph;
//! use drevo_core::native_label_index::NativeLabelIndex;
//! use drevo_core::engine::GraphEngine; // brings `create_node` into scope
//! use drevo_core::model::{NewNode, Properties};
//! use std::collections::HashMap;
//!
//! # fn main() -> drevo_core::error::Result<()> {
//! let g = NativeGraph::new();
//! // A node whose primary kind is `person` and which also carries the
//! // secondary label `employee` (as `SET n:Employee` would store it).
//! let props = Properties(HashMap::from([(
//!     "_labels".to_string(),
//!     serde_json::json!(["employee"]),
//! )]));
//! let n = g.create_node(NewNode { kind: "person".into(), title: "ada".into(),
//!     body: String::new(), body_html: String::new(), properties: props })?;
//!
//! let mut labels = NativeLabelIndex::new();
//! labels.sync(&g);
//! assert_eq!(labels.node_ids("employee"), vec![n.id]);
//! assert!(labels.node_ids("ghost").is_empty());
//! # Ok(())
//! # }
//! ```

use std::collections::{BTreeSet, HashMap};

use crate::engine::GraphEngine;
use crate::labels::secondary_labels;
use crate::model::Node;
use crate::native::{NativeGraph, WalOp};

/// A secondary-label index maintained by tailing a [`NativeGraph`]'s
/// change-feed. See the module docs.
#[derive(Default)]
pub struct NativeLabelIndex {
    /// secondary label → ids of nodes carrying it. `BTreeSet` keeps ids
    /// ascending so a scan is deterministic and merges cleanly with the
    /// ascending primary-kind index.
    postings: HashMap<String, BTreeSet<u64>>,
    /// node id → its secondary labels (the forward index, so a node can be
    /// removed or re-indexed without scanning every posting list).
    docs: HashMap<u64, Vec<String>>,
    /// The change-feed cursor this index has consumed up to.
    cursor: u64,
}

impl NativeLabelIndex {
    /// Create an empty index positioned before any change.
    pub fn new() -> Self {
        Self::default()
    }

    /// The change-feed cursor this index has consumed up to.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// The number of nodes that carry at least one secondary label.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// Whether no node carries a secondary label.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Node ids carrying `label` as a **secondary** label, in ascending order.
    /// Empty when no node carries it. (Primary-kind matches are not here — the
    /// engine's `nodes_by_kind` covers those; the executor unions the two.)
    pub fn node_ids(&self, label: &str) -> Vec<u64> {
        self.postings
            .get(label)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Bring the index up to date with `graph` by consuming its change-feed
    /// since the last [`cursor`](Self::cursor).
    ///
    /// If the feed was trimmed past this index's cursor (a `lagged` batch), the
    /// index is rebuilt from a fresh snapshot of every node — the standard
    /// re-snapshot recovery for a consumer that fell behind the retention
    /// window.
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
                // Edges carry no labels in this index.
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

    /// Insert or replace a node's secondary-label postings (create and update
    /// both route here). A node with no secondary labels is simply not tracked.
    fn index_node(&mut self, node: &Node) {
        self.remove_node(node.id);
        let labels = secondary_labels(node);
        if labels.is_empty() {
            return;
        }
        for l in &labels {
            self.postings.entry(l.clone()).or_default().insert(node.id);
        }
        self.docs.insert(node.id, labels);
    }

    /// Remove a node's postings, if present, dropping any bucket that empties.
    fn remove_node(&mut self, id: u64) {
        let Some(labels) = self.docs.remove(&id) else {
            return;
        };
        for l in &labels {
            if let Some(set) = self.postings.get_mut(l) {
                set.remove(&id);
                if set.is_empty() {
                    self.postings.remove(l);
                }
            }
        }
    }
}
