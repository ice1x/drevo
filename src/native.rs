//! `NativeGraph` — the native graph core (RFC `docs/rfc-native-core.md`,
//! tracking #307, Phase 2/3).
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
//! compare.
//!
//! # Snapshot isolation (ACID "I", Phase 3)
//!
//! The whole store lives behind `RwLock<Arc<Inner>>`. A
//! [`GraphSnapshot`](crate::native::GraphSnapshot) is a cheap `Arc::clone` of
//! the current state — an **O(1)**, frozen, consistent view. Writers use
//! copy-on-write via [`Arc::make_mut`](std::sync::Arc::make_mut): they mutate the state
//! in place while no snapshot is outstanding, and clone it once on the first
//! write after a snapshot is taken. So a reader that holds a snapshot always
//! sees one consistent version of the graph regardless of concurrent writes —
//! the property a multi-hop retrieval needs (a traversal never observes half of
//! another writer's in-flight mutation). Per-transaction write buffering and a
//! `READ COMMITTED` knob build on this in later slices.
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
//! Secondary subsystems (FTS, vectors, property/recency indexes) are
//! intentionally **not** part of this engine — the RFC keeps them off the core
//! graph seam, fed separately via a change-feed.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

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

/// Interior state of a [`NativeGraph`]. Held behind `Arc` so a snapshot is a
/// cheap clone and writers copy-on-write via [`Arc::make_mut`]. `Clone` is what
/// makes that copy-on-write possible.
#[derive(Default, Clone)]
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

    // ----- read operations (shared by the engine and by GraphSnapshot) -------

    fn get_node(&self, id: u64) -> Option<Node> {
        self.nodes.get(&id).cloned()
    }

    fn get_edge(&self, id: u64) -> Option<Edge> {
        self.edges.get(&id).cloned()
    }

    fn neighbor_ids(&self, node_id: u64, direction: Direction, kind: Option<&str>) -> Vec<u64> {
        // A kind that was never seen means no matching edges.
        let want_kind = match kind {
            Some(k) => match self.kind_id_of(k) {
                Some(id) => Some(id),
                None => return Vec::new(),
            },
            None => None,
        };
        let mut seen = std::collections::HashSet::new();
        seen.insert(node_id);
        let mut ids = Vec::new();
        for e in self.incident_entries(node_id, direction) {
            if want_kind.is_some_and(|k| k != e.kind_id) {
                continue;
            }
            if seen.insert(e.neighbor_id) {
                ids.push(e.neighbor_id);
            }
        }
        ids
    }

    fn neighbors(&self, node_id: u64, direction: Direction, kind: Option<&str>) -> Vec<Node> {
        self.neighbor_ids(node_id, direction, kind)
            .into_iter()
            .filter_map(|id| self.nodes.get(&id).cloned())
            .collect()
    }

    fn edges_of(&self, node_id: u64, direction: Direction) -> Vec<Edge> {
        self.incident_entries(node_id, direction)
            .into_iter()
            .filter_map(|e| self.edges.get(&e.edge_id).cloned())
            .collect()
    }

    fn all_nodes(&self) -> Vec<Node> {
        self.nodes.values().cloned().collect()
    }

    fn all_edges(&self) -> Vec<Edge> {
        self.edges.values().cloned().collect()
    }

    fn nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Vec<Node> {
        let mut matching: Vec<Node> = self
            .nodes
            .values()
            .filter(|n| n.kind == kind)
            .cloned()
            .collect();
        // Drevo returns kind scans in ascending id order; match that so
        // pagination (offset/limit) is deterministic and comparable.
        matching.sort_by_key(|n| n.id);
        matching.into_iter().skip(offset).take(limit).collect()
    }

    // ----- write operations (mutate through Arc::make_mut) -------------------

    fn create_node(&mut self, new_node: NewNode) -> Result<Node> {
        if self.titles.contains_key(&new_node.title) {
            return Err(DrevoError::DuplicateTitle(new_node.title));
        }
        self.next_node_id += 1;
        let id = self.next_node_id;
        let node = new_node.into_node(id);
        self.titles.insert(node.title.clone(), id);
        self.nodes.insert(id, node.clone());
        Ok(node)
    }

    fn update_node(&mut self, id: u64, patch: NodePatch) -> Result<Node> {
        let mut node = self
            .nodes
            .get(&id)
            .cloned()
            .ok_or(DrevoError::NodeNotFound(id))?;
        if let Some(ref new_title) = patch.title {
            // A rename collides only when a *different* node owns the title.
            match self.titles.get(new_title) {
                Some(&owner) if owner != id => {
                    return Err(DrevoError::DuplicateTitle(new_title.clone()));
                }
                _ => {}
            }
            self.titles.remove(&node.title);
            self.titles.insert(new_title.clone(), id);
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
        self.nodes.insert(id, node.clone());
        Ok(node)
    }

    fn delete_node(&mut self, id: u64) -> Result<()> {
        let node = self
            .nodes
            .get(&id)
            .cloned()
            .ok_or(DrevoError::NodeNotFound(id))?;
        let edge_ids: Vec<u64> = self
            .incident_entries(id, Direction::Both)
            .into_iter()
            .map(|e| e.edge_id)
            .collect();
        for eid in edge_ids {
            self.remove_edge(eid);
        }
        self.titles.remove(&node.title);
        self.nodes.remove(&id);
        Ok(())
    }

    fn create_edge(&mut self, new_edge: NewEdge) -> Result<Edge> {
        if !new_edge.weight.is_finite() {
            return Err(DrevoError::InvalidWeight(new_edge.weight));
        }
        if !self.nodes.contains_key(&new_edge.from_id) {
            return Err(DrevoError::NodeNotFound(new_edge.from_id));
        }
        if !self.nodes.contains_key(&new_edge.to_id) {
            return Err(DrevoError::NodeNotFound(new_edge.to_id));
        }
        self.next_edge_id += 1;
        let id = self.next_edge_id;
        let edge = new_edge.into_edge(id);
        let kind_id = self.intern_kind(&edge.kind);
        self.out_adj
            .entry(edge.from_id)
            .or_default()
            .push(AdjEntry {
                edge_id: id,
                neighbor_id: edge.to_id,
                kind_id,
            });
        self.in_adj.entry(edge.to_id).or_default().push(AdjEntry {
            edge_id: id,
            neighbor_id: edge.from_id,
            kind_id,
        });
        self.edges.insert(id, edge.clone());
        Ok(edge)
    }

    fn update_edge(&mut self, id: u64, patch: EdgePatch) -> Result<Edge> {
        // Weight finiteness is validated before existence, matching Drevo.
        if let Some(w) = patch.weight {
            if !w.is_finite() {
                return Err(DrevoError::InvalidWeight(w));
            }
        }
        let kind_changed = patch.kind.is_some();
        let mut edge = self
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
        if kind_changed {
            let new_kind_id = self.intern_kind(&edge.kind);
            if let Some(list) = self.out_adj.get_mut(&edge.from_id) {
                for e in list.iter_mut().filter(|e| e.edge_id == id) {
                    e.kind_id = new_kind_id;
                }
            }
            if let Some(list) = self.in_adj.get_mut(&edge.to_id) {
                for e in list.iter_mut().filter(|e| e.edge_id == id) {
                    e.kind_id = new_kind_id;
                }
            }
        }
        self.edges.insert(id, edge.clone());
        Ok(edge)
    }

    fn delete_edge(&mut self, id: u64) -> Result<()> {
        if !self.edges.contains_key(&id) {
            return Err(DrevoError::EdgeNotFound(id));
        }
        self.remove_edge(id);
        Ok(())
    }

    /// Remove an edge and its adjacency entries. A missing id is a no-op;
    /// callers that must error (`delete_edge`) check existence first.
    fn remove_edge(&mut self, id: u64) {
        let Some(edge) = self.edges.remove(&id) else {
            return;
        };
        if let Some(v) = self.out_adj.get_mut(&edge.from_id) {
            v.retain(|e| e.edge_id != id);
        }
        if let Some(v) = self.in_adj.get_mut(&edge.to_id) {
            v.retain(|e| e.edge_id != id);
        }
    }
}

/// An in-memory, native [`GraphEngine`] (RFC Phase 2/3). See the module docs.
#[derive(Default)]
pub struct NativeGraph {
    inner: RwLock<Arc<Inner>>,
}

impl NativeGraph {
    /// Create an empty engine. Ids start at 1, matching `Drevo`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a consistent, read-only [`GraphSnapshot`] of the current state.
    ///
    /// O(1): it is an `Arc::clone` of the live state. The returned view is
    /// **frozen** — subsequent writes to this engine copy-on-write and leave the
    /// snapshot untouched — giving snapshot isolation for reads (RFC ACID "I").
    pub fn snapshot(&self) -> GraphSnapshot {
        GraphSnapshot {
            inner: Arc::clone(&read(&self.inner)),
        }
    }
}

/// Recover a poisoned lock rather than propagating the panic — matches the
/// library-wide policy (no `unwrap`/`expect` in non-test code; a poisoned
/// graph lock still holds valid data).
fn read(inner: &RwLock<Arc<Inner>>) -> std::sync::RwLockReadGuard<'_, Arc<Inner>> {
    inner.read().unwrap_or_else(|e| e.into_inner())
}

fn write(inner: &RwLock<Arc<Inner>>) -> std::sync::RwLockWriteGuard<'_, Arc<Inner>> {
    inner.write().unwrap_or_else(|e| e.into_inner())
}

impl GraphEngine for NativeGraph {
    fn create_node(&self, new_node: NewNode) -> Result<Node> {
        Arc::make_mut(&mut write(&self.inner)).create_node(new_node)
    }

    fn get_node(&self, id: u64) -> Result<Option<Node>> {
        Ok(read(&self.inner).get_node(id))
    }

    fn update_node(&self, id: u64, patch: NodePatch) -> Result<Node> {
        Arc::make_mut(&mut write(&self.inner)).update_node(id, patch)
    }

    fn delete_node(&self, id: u64) -> Result<()> {
        Arc::make_mut(&mut write(&self.inner)).delete_node(id)
    }

    fn create_edge(&self, new_edge: NewEdge) -> Result<Edge> {
        Arc::make_mut(&mut write(&self.inner)).create_edge(new_edge)
    }

    fn get_edge(&self, id: u64) -> Result<Option<Edge>> {
        Ok(read(&self.inner).get_edge(id))
    }

    fn update_edge(&self, id: u64, patch: EdgePatch) -> Result<Edge> {
        Arc::make_mut(&mut write(&self.inner)).update_edge(id, patch)
    }

    fn delete_edge(&self, id: u64) -> Result<()> {
        Arc::make_mut(&mut write(&self.inner)).delete_edge(id)
    }

    fn neighbor_ids(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<u64>> {
        Ok(read(&self.inner).neighbor_ids(node_id, direction, kind))
    }

    fn neighbors(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<Node>> {
        Ok(read(&self.inner).neighbors(node_id, direction, kind))
    }

    fn edges_of(&self, node_id: u64, direction: Direction) -> Result<Vec<Edge>> {
        Ok(read(&self.inner).edges_of(node_id, direction))
    }

    fn all_nodes(&self) -> Result<Vec<Node>> {
        Ok(read(&self.inner).all_nodes())
    }

    fn all_edges(&self) -> Result<Vec<Edge>> {
        Ok(read(&self.inner).all_edges())
    }

    fn nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Node>> {
        Ok(read(&self.inner).nodes_by_kind(kind, limit, offset))
    }
}

/// A frozen, read-only view of a [`NativeGraph`] at the instant
/// [`NativeGraph::snapshot`] was called (RFC ACID "I", Phase 3). Reads are
/// served from the captured [`Arc`] and are unaffected by later writes to the
/// engine, so a whole traversal sees one consistent version of the graph.
#[derive(Clone)]
pub struct GraphSnapshot {
    inner: Arc<Inner>,
}

impl GraphSnapshot {
    /// Fetch a node by id, or `None` if absent in this snapshot.
    pub fn get_node(&self, id: u64) -> Option<Node> {
        self.inner.get_node(id)
    }

    /// Fetch an edge by id, or `None` if absent in this snapshot.
    pub fn get_edge(&self, id: u64) -> Option<Edge> {
        self.inner.get_edge(id)
    }

    /// Distinct neighbour ids in `direction`, optionally kind-filtered.
    pub fn neighbor_ids(&self, node_id: u64, direction: Direction, kind: Option<&str>) -> Vec<u64> {
        self.inner.neighbor_ids(node_id, direction, kind)
    }

    /// Adjacent nodes in `direction`, optionally kind-filtered.
    pub fn neighbors(&self, node_id: u64, direction: Direction, kind: Option<&str>) -> Vec<Node> {
        self.inner.neighbors(node_id, direction, kind)
    }

    /// Full edge records incident to `node_id` in `direction`.
    pub fn edges_of(&self, node_id: u64, direction: Direction) -> Vec<Edge> {
        self.inner.edges_of(node_id, direction)
    }

    /// Every node in this snapshot.
    pub fn all_nodes(&self) -> Vec<Node> {
        self.inner.all_nodes()
    }

    /// Every edge in this snapshot.
    pub fn all_edges(&self) -> Vec<Edge> {
        self.inner.all_edges()
    }

    /// Nodes of `kind`, paginated by `limit`/`offset`.
    pub fn nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Vec<Node> {
        self.inner.nodes_by_kind(kind, limit, offset)
    }
}
