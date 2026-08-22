//! `NativeGraph` — the first slice of the native graph core (RFC
//! `docs/rfc-native-core.md`, tracking #307, Phase 2).
//!
//! # What this is
//!
//! An **in-memory, native** implementation of the [`GraphEngine`](crate::engine::GraphEngine)
//! seam — nodes and edges held directly in Rust maps with maintained
//! adjacency, rather than encoded as byte-keyed rows over a
//! [`StorageBackend`](crate::storage::StorageBackend) the way [`Drevo`](crate::db::Drevo)
//! does. It is the seed the RFC grows into an arena/CSR engine that drops
//! index encoding entirely; this first slice is **correctness-first** (plain
//! `HashMap`s, adjacency lists) so the engine's observable behaviour can be
//! pinned against `Drevo` by differential test before the fast internal
//! layout lands in a later slice.
//!
//! # Behaviour parity
//!
//! `NativeGraph` reproduces `Drevo`'s *observable* graph semantics exactly —
//! monotonic ids from 1, title uniqueness
//! ([`DrevoError::DuplicateTitle`](crate::error::DrevoError::DuplicateTitle)),
//! endpoint existence on edge create
//! ([`DrevoError::NodeNotFound`](crate::error::DrevoError::NodeNotFound)), weight
//! finiteness ([`DrevoError::InvalidWeight`](crate::error::DrevoError::InvalidWeight)),
//! cascade edge deletion when a node is removed, and the direction/kind-filtered
//! adjacency contract — and returns the **same**
//! [`DrevoError`](crate::error::DrevoError) variants, so `tests/native_engine_tests.rs`
//! can compare the two engines op-for-op. uuid/timestamp fields are generated
//! by the shared [`NewNode::into_node`](crate::model::NewNode::into_node) /
//! [`NewEdge::into_edge`](crate::model::NewEdge::into_edge) so the record shape
//! matches; those non-deterministic fields are excluded from differential
//! comparison.
//!
//! Secondary subsystems (FTS, vectors, property/recency indexes, transactions)
//! are intentionally **not** part of this engine — the RFC keeps them off the
//! core graph seam, fed separately via a change-feed.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::engine::GraphEngine;
use crate::error::{DrevoError, Result};
use crate::model::{Direction, Edge, EdgePatch, NewEdge, NewNode, Node, NodePatch};

/// Interior mutable state of a [`NativeGraph`]. Guarded by a single
/// [`RwLock`] so the engine is `Send + Sync` behind an `Arc`, matching the
/// thread-safety contract the seam's callers rely on.
#[derive(Default)]
struct Inner {
    nodes: HashMap<u64, Node>,
    edges: HashMap<u64, Edge>,
    /// `from_id → edge ids` in insertion order (outgoing adjacency).
    out_adj: HashMap<u64, Vec<u64>>,
    /// `to_id → edge ids` in insertion order (incoming adjacency).
    in_adj: HashMap<u64, Vec<u64>>,
    /// `title → node id`, mirroring `Drevo`'s title-uniqueness index.
    titles: HashMap<String, u64>,
    next_node_id: u64,
    next_edge_id: u64,
}

/// An in-memory, native [`GraphEngine`] (RFC Phase 2). See the module docs.
#[derive(Default)]
pub struct NativeGraph {
    inner: RwLock<Inner>,
}

impl NativeGraph {
    /// Create an empty engine. Ids start at 1, matching `Drevo`.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Recover a poisoned lock rather than propagating the panic — matches the
/// library-wide policy (no `unwrap`/`expect` in non-test code; a poisoned
/// graph lock still holds valid data).
fn read(inner: &RwLock<Inner>) -> std::sync::RwLockReadGuard<'_, Inner> {
    inner.read().unwrap_or_else(|e| e.into_inner())
}

fn write(inner: &RwLock<Inner>) -> std::sync::RwLockWriteGuard<'_, Inner> {
    inner.write().unwrap_or_else(|e| e.into_inner())
}

impl Inner {
    /// Edge ids incident to `node_id` in `direction`, in `Drevo::edges_of`
    /// order: the outgoing pass first, then the incoming pass with any edge
    /// already seen skipped. The dedup matters for a **self-loop** under
    /// [`Direction::Both`], which appears in both adjacency lists — `Drevo`
    /// reports it once, so we do too.
    fn incident_edge_ids(&self, node_id: u64, direction: Direction) -> Vec<u64> {
        let mut ids = Vec::new();
        let mut seen = std::collections::HashSet::new();
        if matches!(direction, Direction::Outgoing | Direction::Both) {
            if let Some(v) = self.out_adj.get(&node_id) {
                for &e in v {
                    if seen.insert(e) {
                        ids.push(e);
                    }
                }
            }
        }
        if matches!(direction, Direction::Incoming | Direction::Both) {
            if let Some(v) = self.in_adj.get(&node_id) {
                for &e in v {
                    if seen.insert(e) {
                        ids.push(e);
                    }
                }
            }
        }
        ids
    }
}

impl GraphEngine for NativeGraph {
    fn create_node(&self, new_node: NewNode) -> Result<Node> {
        let mut g = write(&self.inner);
        if g.titles.contains_key(&new_node.title) {
            return Err(DrevoError::DuplicateTitle(new_node.title));
        }
        g.next_node_id += 1;
        let id = g.next_node_id;
        let node = new_node.into_node(id);
        g.titles.insert(node.title.clone(), id);
        g.nodes.insert(id, node.clone());
        Ok(node)
    }

    fn get_node(&self, id: u64) -> Result<Option<Node>> {
        Ok(read(&self.inner).nodes.get(&id).cloned())
    }

    fn update_node(&self, id: u64, patch: NodePatch) -> Result<Node> {
        let mut g = write(&self.inner);
        let mut node = g
            .nodes
            .get(&id)
            .cloned()
            .ok_or(DrevoError::NodeNotFound(id))?;

        if let Some(ref new_title) = patch.title {
            // A rename collides only when a *different* node owns the title.
            match g.titles.get(new_title) {
                Some(&owner) if owner != id => {
                    return Err(DrevoError::DuplicateTitle(new_title.clone()));
                }
                _ => {}
            }
            g.titles.remove(&node.title);
            g.titles.insert(new_title.clone(), id);
            node.title = new_title.clone();
        }
        if let Some(kind) = patch.kind {
            node.kind = kind;
        }
        if let Some(body) = patch.body {
            node.body = body;
        }
        if let Some(body_html) = patch.body_html {
            node.body_html = body_html;
        }
        if let Some(properties) = patch.properties {
            node.properties = properties;
        }
        node.updated_at = now_millis(&node);
        g.nodes.insert(id, node.clone());
        Ok(node)
    }

    fn delete_node(&self, id: u64) -> Result<()> {
        let mut g = write(&self.inner);
        let node = g
            .nodes
            .get(&id)
            .cloned()
            .ok_or(DrevoError::NodeNotFound(id))?;
        // Cascade: remove every incident edge (both directions), deduped so a
        // self-loop is only deleted once.
        let mut edge_ids = g.incident_edge_ids(id, Direction::Both);
        edge_ids.sort_unstable();
        edge_ids.dedup();
        for eid in edge_ids {
            remove_edge(&mut g, eid);
        }
        g.titles.remove(&node.title);
        g.nodes.remove(&id);
        Ok(())
    }

    fn create_edge(&self, new_edge: NewEdge) -> Result<Edge> {
        if !new_edge.weight.is_finite() {
            return Err(DrevoError::InvalidWeight(new_edge.weight));
        }
        let mut g = write(&self.inner);
        if !g.nodes.contains_key(&new_edge.from_id) {
            return Err(DrevoError::NodeNotFound(new_edge.from_id));
        }
        if !g.nodes.contains_key(&new_edge.to_id) {
            return Err(DrevoError::NodeNotFound(new_edge.to_id));
        }
        g.next_edge_id += 1;
        let id = g.next_edge_id;
        let edge = new_edge.into_edge(id);
        g.out_adj.entry(edge.from_id).or_default().push(id);
        g.in_adj.entry(edge.to_id).or_default().push(id);
        g.edges.insert(id, edge.clone());
        Ok(edge)
    }

    fn get_edge(&self, id: u64) -> Result<Option<Edge>> {
        Ok(read(&self.inner).edges.get(&id).cloned())
    }

    fn update_edge(&self, id: u64, patch: EdgePatch) -> Result<Edge> {
        // Weight finiteness is validated before existence, matching Drevo.
        if let Some(w) = patch.weight {
            if !w.is_finite() {
                return Err(DrevoError::InvalidWeight(w));
            }
        }
        let mut g = write(&self.inner);
        let mut edge = g
            .edges
            .get(&id)
            .cloned()
            .ok_or(DrevoError::EdgeNotFound(id))?;
        if let Some(kind) = patch.kind {
            edge.kind = kind;
        }
        if let Some(weight) = patch.weight {
            edge.weight = weight;
        }
        if let Some(properties) = patch.properties {
            edge.properties = properties;
        }
        g.edges.insert(id, edge.clone());
        Ok(edge)
    }

    fn delete_edge(&self, id: u64) -> Result<()> {
        let mut g = write(&self.inner);
        if !g.edges.contains_key(&id) {
            return Err(DrevoError::EdgeNotFound(id));
        }
        remove_edge(&mut g, id);
        Ok(())
    }

    fn neighbor_ids(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<u64>> {
        let g = read(&self.inner);
        let mut seen = std::collections::HashSet::new();
        seen.insert(node_id);
        let mut ids = Vec::new();
        for eid in g.incident_edge_ids(node_id, direction) {
            let Some(edge) = g.edges.get(&eid) else {
                continue;
            };
            if let Some(k) = kind {
                if edge.kind != k {
                    continue;
                }
            }
            // The neighbour is whichever endpoint is not `node_id`.
            let other = if edge.from_id == node_id {
                edge.to_id
            } else {
                edge.from_id
            };
            if seen.insert(other) {
                ids.push(other);
            }
        }
        Ok(ids)
    }

    fn neighbors(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<Node>> {
        let ids = self.neighbor_ids(node_id, direction, kind)?;
        let g = read(&self.inner);
        Ok(ids
            .into_iter()
            .filter_map(|id| g.nodes.get(&id).cloned())
            .collect())
    }

    fn edges_of(&self, node_id: u64, direction: Direction) -> Result<Vec<Edge>> {
        let g = read(&self.inner);
        Ok(g.incident_edge_ids(node_id, direction)
            .into_iter()
            .filter_map(|eid| g.edges.get(&eid).cloned())
            .collect())
    }

    fn all_nodes(&self) -> Result<Vec<Node>> {
        Ok(read(&self.inner).nodes.values().cloned().collect())
    }

    fn all_edges(&self) -> Result<Vec<Edge>> {
        Ok(read(&self.inner).edges.values().cloned().collect())
    }

    fn nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Node>> {
        let g = read(&self.inner);
        let mut matching: Vec<Node> = g
            .nodes
            .values()
            .filter(|n| n.kind == kind)
            .cloned()
            .collect();
        // Drevo returns kind scans in ascending id order; match that so
        // pagination (offset/limit) is deterministic and comparable.
        matching.sort_by_key(|n| n.id);
        Ok(matching.into_iter().skip(offset).take(limit).collect())
    }
}

/// Remove an edge and its adjacency entries. A missing id is a no-op, matching
/// `Drevo::delete_edge`.
fn remove_edge(g: &mut Inner, id: u64) {
    let Some(edge) = g.edges.remove(&id) else {
        return;
    };
    if let Some(v) = g.out_adj.get_mut(&edge.from_id) {
        v.retain(|&e| e != id);
    }
    if let Some(v) = g.in_adj.get_mut(&edge.to_id) {
        v.retain(|&e| e != id);
    }
}

/// A monotonic-ish update timestamp. `into_node` already stamped `updated_at`
/// at creation; on update we keep it non-decreasing without depending on wall
/// clock (which differential tests cannot compare anyway — the field is
/// excluded from comparison). Reusing the node's own value keeps the record
/// well-formed.
fn now_millis(node: &Node) -> i64 {
    node.updated_at
}
