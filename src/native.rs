//! `NativeGraph` — the native graph core (RFC `docs/rfc-native-core.md`,
//! tracking #307, Phase 2).
//!
//! # What this is
//!
//! An **in-memory, native** implementation of the [`GraphEngine`](crate::engine::GraphEngine)
//! seam — nodes and edges held directly in Rust maps, rather than encoded as
//! byte-keyed rows over a [`StorageBackend`](crate::storage::StorageBackend) the
//! way [`Drevo`](crate::db::Drevo) does.
//!
//! Adjacency is **denormalized**: each entry carries the neighbour node id and
//! the edge's kind (interned to a `u32`) inline alongside the edge id, so a
//! fan-out reads neighbour ids and filters by kind straight from a node's
//! adjacency vector — no second lookup into the edge map, no per-edge string
//! compare. This mirrors, in native memory, what Drevo's #243 denormalized
//! adjacency does over the KV index, and is the seed the RFC grows into an
//! arena/CSR layout (kind-*sorted* slices for binary-searched type filtering
//! are a later slice; this one keeps insertion order).
//!
//! # Behaviour parity
//!
//! `NativeGraph` reproduces `Drevo`'s *observable* graph semantics exactly —
//! monotonic ids from 1, title uniqueness
//! ([`DrevoError::DuplicateTitle`](crate::error::DrevoError::DuplicateTitle)),
//! endpoint existence on edge create
//! ([`DrevoError::NodeNotFound`](crate::error::DrevoError::NodeNotFound)), weight
//! finiteness ([`DrevoError::InvalidWeight`](crate::error::DrevoError::InvalidWeight)),
//! [`EdgeNotFound`](crate::error::DrevoError::EdgeNotFound) on missing edge
//! update/delete, cascade edge deletion when a node is removed, and the
//! direction/kind-filtered adjacency contract — and returns the **same**
//! [`DrevoError`](crate::error::DrevoError) variants, so
//! `tests/native_engine_tests.rs` can compare the two engines op-for-op
//! (including a randomized differential workload). uuid/timestamp fields come
//! from the shared [`NewNode::into_node`](crate::model::NewNode::into_node) /
//! [`NewEdge::into_edge`](crate::model::NewEdge::into_edge) and are excluded
//! from comparison as non-deterministic.
//!
//! Secondary subsystems (FTS, vectors, property/recency indexes, transactions)
//! are intentionally **not** part of this engine — the RFC keeps them off the
//! core graph seam, fed separately via a change-feed.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::engine::GraphEngine;
use crate::error::{DrevoError, Result};
use crate::model::{Direction, Edge, EdgePatch, NewEdge, NewNode, Node, NodePatch};

/// A denormalized adjacency entry: the incident edge, the node at its other
/// end, and that edge's kind interned to a `u32`. Storing the neighbour id and
/// kind inline is what lets a fan-out avoid a second map lookup per edge.
#[derive(Clone, Copy)]
struct AdjEntry {
    edge_id: u64,
    neighbor_id: u64,
    kind_id: u32,
}

/// Interior mutable state of a [`NativeGraph`]. Guarded by a single
/// [`RwLock`] so the engine is `Send + Sync` behind an `Arc`, matching the
/// thread-safety contract the seam's callers rely on.
#[derive(Default)]
struct Inner {
    nodes: HashMap<u64, Node>,
    edges: HashMap<u64, Edge>,
    /// `from_id → entries` (neighbour = the edge's `to_id`), insertion order.
    out_adj: HashMap<u64, Vec<AdjEntry>>,
    /// `to_id → entries` (neighbour = the edge's `from_id`), insertion order.
    in_adj: HashMap<u64, Vec<AdjEntry>>,
    /// `title → node id`, mirroring `Drevo`'s title-uniqueness index.
    titles: HashMap<String, u64>,
    /// Edge-kind string → interned `u32` id (adjacency stores the id).
    kind_ids: HashMap<String, u32>,
    next_node_id: u64,
    next_edge_id: u64,
}

impl Inner {
    /// Intern an edge kind, assigning a fresh id on first sight.
    fn intern_kind(&mut self, kind: &str) -> u32 {
        if let Some(&id) = self.kind_ids.get(kind) {
            return id;
        }
        let id = self.kind_ids.len() as u32;
        self.kind_ids.insert(kind.to_string(), id);
        id
    }

    /// The interned id for `kind`, or `None` if no edge of that kind has ever
    /// existed (so a kind-filtered scan can short-circuit to empty).
    fn kind_id_of(&self, kind: &str) -> Option<u32> {
        self.kind_ids.get(kind).copied()
    }

    /// The adjacency entries incident to `node_id` in `direction`, in
    /// `Drevo::edges_of` order: the outgoing pass first, then the incoming pass
    /// with any edge already seen skipped. The dedup matters for a **self-loop**
    /// under [`Direction::Both`], which appears in both adjacency lists —
    /// `Drevo` reports it once, so we do too.
    fn incident_entries(&self, node_id: u64, direction: Direction) -> Vec<AdjEntry> {
        let mut entries = Vec::new();
        let mut seen = std::collections::HashSet::new();
        if matches!(direction, Direction::Outgoing | Direction::Both) {
            if let Some(v) = self.out_adj.get(&node_id) {
                for e in v {
                    if seen.insert(e.edge_id) {
                        entries.push(*e);
                    }
                }
            }
        }
        if matches!(direction, Direction::Incoming | Direction::Both) {
            if let Some(v) = self.in_adj.get(&node_id) {
                for e in v {
                    if seen.insert(e.edge_id) {
                        entries.push(*e);
                    }
                }
            }
        }
        entries
    }
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
        let edge_ids: Vec<u64> = g
            .incident_entries(id, Direction::Both)
            .into_iter()
            .map(|e| e.edge_id)
            .collect();
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
        let kind_id = g.intern_kind(&edge.kind);
        g.out_adj.entry(edge.from_id).or_default().push(AdjEntry {
            edge_id: id,
            neighbor_id: edge.to_id,
            kind_id,
        });
        g.in_adj.entry(edge.to_id).or_default().push(AdjEntry {
            edge_id: id,
            neighbor_id: edge.from_id,
            kind_id,
        });
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
        let kind_changed = patch.kind.is_some();
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
        // A kind change must be reflected in the denormalized adjacency so
        // kind-filtered scans stay correct.
        if kind_changed {
            let new_kind_id = g.intern_kind(&edge.kind);
            // The edge appears once in out_adj[from] and once in in_adj[to];
            // update both.
            if let Some(list) = g.out_adj.get_mut(&edge.from_id) {
                for e in list.iter_mut().filter(|e| e.edge_id == id) {
                    e.kind_id = new_kind_id;
                }
            }
            if let Some(list) = g.in_adj.get_mut(&edge.to_id) {
                for e in list.iter_mut().filter(|e| e.edge_id == id) {
                    e.kind_id = new_kind_id;
                }
            }
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
        // A kind that was never seen means no matching edges.
        let want_kind = match kind {
            Some(k) => match g.kind_id_of(k) {
                Some(id) => Some(id),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        let mut seen = std::collections::HashSet::new();
        seen.insert(node_id);
        let mut ids = Vec::new();
        for e in g.incident_entries(node_id, direction) {
            if want_kind.is_some_and(|k| k != e.kind_id) {
                continue;
            }
            if seen.insert(e.neighbor_id) {
                ids.push(e.neighbor_id);
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
        Ok(g.incident_entries(node_id, direction)
            .into_iter()
            .filter_map(|e| g.edges.get(&e.edge_id).cloned())
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

/// Remove an edge and its adjacency entries. A missing id is a no-op; callers
/// that must error on a missing edge (the public `delete_edge`) check first.
fn remove_edge(g: &mut Inner, id: u64) {
    let Some(edge) = g.edges.remove(&id) else {
        return;
    };
    if let Some(v) = g.out_adj.get_mut(&edge.from_id) {
        v.retain(|e| e.edge_id != id);
    }
    if let Some(v) = g.in_adj.get_mut(&edge.to_id) {
        v.retain(|e| e.edge_id != id);
    }
}
