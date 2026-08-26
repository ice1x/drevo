//! A per-node cache of the executor's [`crate::cypher::executor::NodeValue`]
//! projection, fed by a [`crate::native::NativeGraph`] change-feed (RFC `docs/rfc-native-core.md`, #307).
//!
//! # Why
//!
//! With the zero-copy `Arc<Node>` seam in place, the remaining cost of a
//! native scan is rebuilding the Cypher-facing [`crate::cypher::executor::NodeValue`] (labels vector +
//! `BTreeMap` property map, every string cloned) for **every node on every
//! query**. The projection of an unchanged node never changes, so it can be
//! built once and shared — this cache is the memo, maintained off the write
//! path by tailing the change-feed like the label/property indexes do.
//!
//! # Why it can never serve a stale value
//!
//! Correctness does **not** depend on the caller keeping the cache in sync.
//! Each entry stores the exact source [`std::sync::Arc`]`<Node>` it was built from, and the
//! executor validates a hit with [`std::sync::Arc::ptr_eq`] against the **live** record it
//! is enumerating: same allocation ⇒ same content ⇒ the cached projection is
//! exact. Any write replaces the stored record with a fresh allocation (the
//! native engine clones-out and re-inserts on update — and the cache's own
//! strong reference pins the old allocation, so its address cannot be reused
//! while the entry exists). A mismatch simply falls back to building the
//! projection from the live node — a stale or unsynced cache costs speed,
//! never answers.

use std::collections::HashMap;
use std::sync::Arc;

use crate::cypher::executor::{node_to_value, NodeValue};
use crate::engine::GraphEngine;
use crate::native::{NativeGraph, WalOp};

/// One cached projection: the exact source record it was built from (the
/// [`Arc::ptr_eq`] validity token) plus the built value.
struct Entry {
    source: Arc<crate::model::Node>,
    value: Arc<NodeValue>,
}

/// A change-feed-maintained memo of the executor's [`NodeValue`] projection.
/// See the module docs for the validity model.
#[derive(Default)]
pub struct NativeValueCache {
    entries: HashMap<u64, Entry>,
    /// The change-feed cursor this cache has consumed up to.
    cursor: u64,
}

impl NativeValueCache {
    /// Create an empty cache positioned before any change.
    pub fn new() -> Self {
        Self::default()
    }

    /// The change-feed cursor this cache has consumed up to.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// The number of nodes with a cached projection.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds no projections.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The cached projection for the **exact** stored record `live`, or `None`
    /// when the cache has no entry for this node or the entry was built from a
    /// different (older) allocation. `Some` is returned only when
    /// [`Arc::ptr_eq`] proves the entry matches the live record, so a hit is
    /// always exact regardless of how stale the cache is.
    pub fn value_for(&self, live: &Arc<crate::model::Node>) -> Option<Arc<NodeValue>> {
        let entry = self.entries.get(&live.id)?;
        Arc::ptr_eq(&entry.source, live).then(|| Arc::clone(&entry.value))
    }

    /// Bring the cache up to date with `graph` by consuming its change-feed
    /// since the last [`cursor`](Self::cursor); a feed trimmed past the cursor
    /// triggers a transparent rebuild from a fresh snapshot.
    pub fn sync(&mut self, graph: &NativeGraph) {
        let batch = graph.changes_since(self.cursor);
        if batch.lagged {
            self.rebuild_from(graph);
            self.cursor = graph.change_head().max(batch.cursor);
            return;
        }
        for op in batch.ops {
            match op {
                // The feed carries an owned copy of the node; re-fetch the
                // *stored* Arc so the entry's source is the allocation the
                // engine actually serves (the ptr_eq validity token).
                WalOp::UpsertNode(node) => self.index_id(graph, node.id),
                WalOp::DeleteNode(id) => {
                    self.entries.remove(&id);
                }
                WalOp::UpsertEdge(_) | WalOp::DeleteEdge(_) => {}
            }
        }
        self.cursor = batch.cursor;
    }

    fn rebuild_from(&mut self, graph: &NativeGraph) {
        self.entries.clear();
        if let Ok(nodes) = GraphEngine::all_nodes(graph) {
            for node in nodes {
                let value = node_to_value(&node);
                self.entries.insert(
                    node.id,
                    Entry {
                        source: node,
                        value,
                    },
                );
            }
        }
    }

    fn index_id(&mut self, graph: &NativeGraph, id: u64) {
        match GraphEngine::get_node(graph, id) {
            Ok(Some(node)) => {
                let value = node_to_value(&node);
                self.entries.insert(
                    id,
                    Entry {
                        source: node,
                        value,
                    },
                );
            }
            // Deleted (or unreadable) between the op and now — drop it.
            _ => {
                self.entries.remove(&id);
            }
        }
    }
}
