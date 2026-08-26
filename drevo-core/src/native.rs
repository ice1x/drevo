//! `NativeGraph` — the native graph core (RFC `docs/rfc-native-core.md`,
//! tracking #307, Phase 2/3).
//!
//! # What this is
//!
//! An **in-memory, native** implementation of the [`GraphEngine`](crate::engine::GraphEngine)
//! seam — nodes and edges held directly in Rust maps, rather than encoded as
//! byte-keyed rows over a storage backend the way the main crate's KV-backed
//! `Drevo` store does.
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
//! ([`CoreError::DuplicateTitle`](crate::error::CoreError::DuplicateTitle)),
//! endpoint existence on edge create
//! ([`CoreError::NodeNotFound`](crate::error::CoreError::NodeNotFound)), weight
//! finiteness ([`CoreError::InvalidWeight`](crate::error::CoreError::InvalidWeight)),
//! [`EdgeNotFound`](crate::error::CoreError::EdgeNotFound) on missing edge
//! update/delete, cascade edge deletion when a node is removed, and the
//! direction/kind-filtered adjacency contract — and returns the **same**
//! [`CoreError`](crate::error::CoreError) variants, so
//! `tests/native_engine_tests.rs` can compare the two engines op-for-op
//! (including a randomized differential workload). uuid/timestamp fields come
//! from the shared [`NewNode::into_node`](crate::model::NewNode::into_node) /
//! [`NewEdge::into_edge`](crate::model::NewEdge::into_edge) and are excluded
//! from comparison as non-deterministic.
//!
//! Secondary subsystems (FTS, vectors, property/recency indexes) are
//! intentionally **not** part of this engine — the RFC keeps them off the core
//! graph seam, fed separately via a change-feed.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::dump::{Dump, DumpError, ImportReport, FORMAT_V1};
use crate::engine::GraphEngine;
use crate::error::{CoreError, Result};
use crate::model::{now_ms, Direction, Edge, EdgePatch, NewEdge, NewNode, Node, NodePatch};

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
    // Nodes and edges are stored behind `Arc` so a read can hand back a cheap
    // pointer-bump handle (`get_node_arc`/`neighbors_arc`) instead of deep-
    // cloning the record, and so copy-on-write of `Inner` for a snapshot clones
    // `Arc`s rather than every node body/property map.
    nodes: HashMap<u64, Arc<Node>>,
    edges: HashMap<u64, Arc<Edge>>,
    /// `from_id → entries` (neighbour = the edge's `to_id`), insertion order.
    out_adj: HashMap<u64, Vec<AdjEntry>>,
    /// `to_id → entries` (neighbour = the edge's `from_id`), insertion order.
    in_adj: HashMap<u64, Vec<AdjEntry>>,
    /// `title → node id`, mirroring `Drevo`'s title-uniqueness index.
    titles: HashMap<String, u64>,
    /// `node kind → ids of that kind`, mirroring `Drevo`'s primary-kind index
    /// so `nodes_by_kind` is `O(matches · log)` instead of a full `O(n)` scan.
    /// The `BTreeSet` keeps ids ascending, which is the order `Drevo` returns
    /// (and what `offset`/`limit` pagination is defined against).
    kind_index: HashMap<String, BTreeSet<u64>>,
    /// Edge-kind string → interned `u32` id (adjacency stores the id).
    kind_ids: HashMap<String, u32>,
    /// Declared schema constraints, validated at transaction commit.
    constraints: Vec<Constraint>,
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

    /// Add `id` to the `kind` bucket of the node-kind index.
    fn index_node_kind(&mut self, id: u64, kind: &str) {
        self.kind_index
            .entry(kind.to_string())
            .or_default()
            .insert(id);
    }

    /// Remove `id` from the `kind` bucket, dropping the bucket when it empties
    /// so an absent kind is truly absent (no lingering empty set).
    fn unindex_node_kind(&mut self, id: u64, kind: &str) {
        if let Some(set) = self.kind_index.get_mut(kind) {
            set.remove(&id);
            if set.is_empty() {
                self.kind_index.remove(kind);
            }
        }
    }

    /// Invoke `f` for each adjacency entry incident to `node_id` in `direction`,
    /// in `Drevo::edges_of` order (the outgoing pass, then the incoming pass) —
    /// **without allocating**, so the hot fan-out paths spend no time on a
    /// scratch `Vec`/`HashSet`.
    ///
    /// A **self-loop** under [`Direction::Both`] sits in both the out- and
    /// in-lists; `Drevo` reports it once, so the incoming pass skips it. That is
    /// exact: a self-loop is the *only* way one edge lands in both a node's out-
    /// and in-lists (a non-loop edge `x→y` is in `out[x]` and `in[y]` only), and
    /// a self-loop's entry is exactly the one whose `neighbor_id == node_id`.
    fn for_each_incident<F: FnMut(&AdjEntry)>(&self, node_id: u64, direction: Direction, mut f: F) {
        if matches!(direction, Direction::Outgoing | Direction::Both) {
            if let Some(v) = self.out_adj.get(&node_id) {
                for e in v {
                    f(e);
                }
            }
        }
        if matches!(direction, Direction::Incoming | Direction::Both) {
            if let Some(v) = self.in_adj.get(&node_id) {
                for e in v {
                    // Under `Both`, the outgoing pass already emitted any
                    // self-loop; skip its mirror here to keep `Drevo`'s count.
                    if matches!(direction, Direction::Both) && e.neighbor_id == node_id {
                        continue;
                    }
                    f(e);
                }
            }
        }
    }

    /// The adjacency entries incident to `node_id`, collected into a `Vec`. Used
    /// by the **mutating** paths (`delete_node`), which need an owned list
    /// because they borrow `self` mutably while iterating; read paths use the
    /// allocation-free [`for_each_incident`](Self::for_each_incident).
    fn incident_entries(&self, node_id: u64, direction: Direction) -> Vec<AdjEntry> {
        let mut entries = Vec::new();
        self.for_each_incident(node_id, direction, |e| entries.push(*e));
        entries
    }

    // ----- read operations (shared by the engine and by GraphSnapshot) -------

    fn get_node(&self, id: u64) -> Option<Node> {
        self.nodes.get(&id).map(|a| (**a).clone())
    }

    fn get_edge(&self, id: u64) -> Option<Edge> {
        self.edges.get(&id).map(|a| (**a).clone())
    }

    /// Zero-copy node fetch: clones the `Arc` (a refcount bump), not the record.
    fn get_node_arc(&self, id: u64) -> Option<Arc<Node>> {
        self.nodes.get(&id).cloned()
    }

    /// Zero-copy edge fetch: clones the `Arc`, not the record.
    fn get_edge_arc(&self, id: u64) -> Option<Arc<Edge>> {
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
        self.for_each_incident(node_id, direction, |e| {
            if want_kind.is_some_and(|k| k != e.kind_id) {
                return;
            }
            if seen.insert(e.neighbor_id) {
                ids.push(e.neighbor_id);
            }
        });
        ids
    }

    fn neighbors(&self, node_id: u64, direction: Direction, kind: Option<&str>) -> Vec<Node> {
        self.neighbor_ids(node_id, direction, kind)
            .into_iter()
            .filter_map(|id| self.nodes.get(&id).map(|a| (**a).clone()))
            .collect()
    }

    /// Zero-copy neighbour fetch: each neighbour comes back as an `Arc<Node>`
    /// handle (refcount bump), so a fan-out that reads many neighbours never
    /// deep-clones their bodies/properties.
    fn neighbors_arc(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Vec<Arc<Node>> {
        self.neighbor_ids(node_id, direction, kind)
            .into_iter()
            .filter_map(|id| self.nodes.get(&id).cloned())
            .collect()
    }

    fn edges_of(&self, node_id: u64, direction: Direction) -> Vec<Edge> {
        let mut out = Vec::new();
        self.for_each_incident(node_id, direction, |e| {
            if let Some(edge) = self.edges.get(&e.edge_id) {
                out.push((**edge).clone());
            }
        });
        out
    }

    fn all_nodes(&self) -> Vec<Node> {
        // Id-ascending, not HashMap iteration order: full scans must
        // enumerate deterministically (and identically to the KV engine's
        // `collect_all_nodes`) so an unordered Cypher `MATCH` produces the
        // same row order on both engines.
        let mut nodes: Vec<Node> = self.nodes.values().map(|a| (**a).clone()).collect();
        nodes.sort_unstable_by_key(|n| n.id);
        nodes
    }

    /// Zero-copy full scan: id-ascending `Arc<Node>` handles (refcount bumps
    /// only), so enumerating the whole graph never deep-clones node bodies.
    fn all_nodes_arc(&self) -> Vec<Arc<Node>> {
        let mut nodes: Vec<Arc<Node>> = self.nodes.values().cloned().collect();
        nodes.sort_unstable_by_key(|n| n.id);
        nodes
    }

    fn all_edges(&self) -> Vec<Edge> {
        // Id-ascending, for the same determinism contract as `all_nodes`.
        let mut edges: Vec<Edge> = self.edges.values().map(|a| (**a).clone()).collect();
        edges.sort_unstable_by_key(|e| e.id);
        edges
    }

    fn nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Vec<Node> {
        // Index-driven: walk only this kind's ids (already ascending in the
        // `BTreeSet`, matching Drevo's order) and materialise the page. An
        // unknown kind has no bucket → empty, no scan.
        let Some(ids) = self.kind_index.get(kind) else {
            return Vec::new();
        };
        ids.iter()
            .skip(offset)
            .take(limit)
            .filter_map(|id| self.nodes.get(id).map(|a| (**a).clone()))
            .collect()
    }

    /// Zero-copy label scan (see [`Inner::all_nodes_arc`]): the page's nodes
    /// come back as `Arc<Node>` handles instead of deep clones.
    fn nodes_by_kind_arc(&self, kind: &str, limit: usize, offset: usize) -> Vec<Arc<Node>> {
        let Some(ids) = self.kind_index.get(kind) else {
            return Vec::new();
        };
        ids.iter()
            .skip(offset)
            .take(limit)
            .filter_map(|id| self.nodes.get(id).cloned())
            .collect()
    }

    // ----- write operations (mutate through Arc::make_mut) -------------------

    fn create_node(&mut self, new_node: NewNode) -> Result<Node> {
        if self.titles.contains_key(&new_node.title) {
            return Err(CoreError::DuplicateTitle(new_node.title));
        }
        self.next_node_id += 1;
        let id = self.next_node_id;
        let node = new_node.into_node(id);
        self.titles.insert(node.title.clone(), id);
        self.index_node_kind(id, &node.kind);
        self.nodes.insert(id, Arc::new(node.clone()));
        Ok(node)
    }

    fn update_node(&mut self, id: u64, patch: NodePatch) -> Result<Node> {
        let mut node = self
            .nodes
            .get(&id)
            .map(|a| (**a).clone())
            .ok_or(CoreError::NodeNotFound(id))?;
        let old_kind = node.kind.clone();
        if let Some(ref new_title) = patch.title {
            // A rename collides only when a *different* node owns the title.
            match self.titles.get(new_title) {
                Some(&owner) if owner != id => {
                    return Err(CoreError::DuplicateTitle(new_title.clone()));
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
        if old_kind != node.kind {
            self.unindex_node_kind(id, &old_kind);
            self.index_node_kind(id, &node.kind);
        }
        self.nodes.insert(id, Arc::new(node.clone()));
        Ok(node)
    }

    fn delete_node(&mut self, id: u64) -> Result<()> {
        let node = self
            .nodes
            .get(&id)
            .map(|a| (**a).clone())
            .ok_or(CoreError::NodeNotFound(id))?;
        let edge_ids: Vec<u64> = self
            .incident_entries(id, Direction::Both)
            .into_iter()
            .map(|e| e.edge_id)
            .collect();
        for eid in edge_ids {
            self.remove_edge(eid);
        }
        self.titles.remove(&node.title);
        self.unindex_node_kind(id, &node.kind);
        self.nodes.remove(&id);
        Ok(())
    }

    fn create_edge(&mut self, new_edge: NewEdge) -> Result<Edge> {
        if !new_edge.weight.is_finite() {
            return Err(CoreError::InvalidWeight(new_edge.weight));
        }
        if !self.nodes.contains_key(&new_edge.from_id) {
            return Err(CoreError::NodeNotFound(new_edge.from_id));
        }
        if !self.nodes.contains_key(&new_edge.to_id) {
            return Err(CoreError::NodeNotFound(new_edge.to_id));
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
        self.edges.insert(id, Arc::new(edge.clone()));
        Ok(edge)
    }

    fn update_edge(&mut self, id: u64, patch: EdgePatch) -> Result<Edge> {
        // Weight finiteness is validated before existence, matching Drevo.
        if let Some(w) = patch.weight {
            if !w.is_finite() {
                return Err(CoreError::InvalidWeight(w));
            }
        }
        let kind_changed = patch.kind.is_some();
        let mut edge = self
            .edges
            .get(&id)
            .map(|a| (**a).clone())
            .ok_or(CoreError::EdgeNotFound(id))?;
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
        self.edges.insert(id, Arc::new(edge.clone()));
        Ok(edge)
    }

    fn delete_edge(&mut self, id: u64) -> Result<()> {
        if !self.edges.contains_key(&id) {
            return Err(CoreError::EdgeNotFound(id));
        }
        self.remove_edge(id);
        Ok(())
    }

    /// The state as a compact op log: every node then every edge as an
    /// `Upsert`, ascending id. Replaying it reproduces the graph, so it is the
    /// snapshot form used by `dump_wal` and WAL compaction.
    fn to_wal_ops(&self) -> Vec<WalOp> {
        let mut ops = Vec::with_capacity(self.nodes.len() + self.edges.len());
        let mut nodes: Vec<&Arc<Node>> = self.nodes.values().collect();
        nodes.sort_by_key(|n| n.id);
        for n in nodes {
            ops.push(WalOp::UpsertNode((**n).clone()));
        }
        let mut edges: Vec<&Arc<Edge>> = self.edges.values().collect();
        edges.sort_by_key(|e| e.id);
        for e in edges {
            ops.push(WalOp::UpsertEdge((**e).clone()));
        }
        ops
    }

    /// Apply one [`WalOp`] during replay. Upserts re-insert a stored record
    /// verbatim (ids/uuids/timestamps preserved) and advance the id counters so
    /// a post-recovery create never reuses an id; deletes mirror the live paths.
    /// Derived structures (adjacency, title and kind indexes) are rebuilt from
    /// the record.
    fn apply_wal_op(&mut self, op: WalOp) {
        match op {
            WalOp::UpsertNode(node) => {
                // A replacing upsert may change the record's title and/or kind;
                // clone the previous record so the derived indexes can be moved
                // off the old values before the new record lands.
                let old = self.nodes.get(&node.id).map(|a| (**a).clone());
                if let Some(ref old) = old {
                    if old.title != node.title {
                        self.titles.remove(&old.title);
                    }
                    self.unindex_node_kind(node.id, &old.kind);
                }
                self.titles.insert(node.title.clone(), node.id);
                self.index_node_kind(node.id, &node.kind);
                self.next_node_id = self.next_node_id.max(node.id);
                self.nodes.insert(node.id, Arc::new(node));
            }
            WalOp::DeleteNode(id) => {
                if let Some(node) = self.nodes.get(&id).map(|a| (**a).clone()) {
                    let edge_ids: Vec<u64> = self
                        .incident_entries(id, Direction::Both)
                        .into_iter()
                        .map(|e| e.edge_id)
                        .collect();
                    for eid in edge_ids {
                        self.remove_edge(eid);
                    }
                    self.titles.remove(&node.title);
                    self.unindex_node_kind(id, &node.kind);
                    self.nodes.remove(&id);
                }
            }
            WalOp::UpsertEdge(edge) => {
                // On update, drop the old adjacency entries before re-adding.
                if self.edges.contains_key(&edge.id) {
                    self.remove_edge(edge.id);
                }
                let kind_id = self.intern_kind(&edge.kind);
                self.out_adj
                    .entry(edge.from_id)
                    .or_default()
                    .push(AdjEntry {
                        edge_id: edge.id,
                        neighbor_id: edge.to_id,
                        kind_id,
                    });
                self.in_adj.entry(edge.to_id).or_default().push(AdjEntry {
                    edge_id: edge.id,
                    neighbor_id: edge.from_id,
                    kind_id,
                });
                self.next_edge_id = self.next_edge_id.max(edge.id);
                self.edges.insert(edge.id, Arc::new(edge));
            }
            WalOp::DeleteEdge(id) => self.remove_edge(id),
        }
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

    /// Check every declared constraint against the current node set, returning
    /// the first [`ConstraintViolation`] found. A `UNIQUE(kind, property)`
    /// constraint ignores nodes of that kind that lack the property (matching
    /// Neo4j), and flags the first pair that shares a value.
    fn validate_constraints(&self) -> std::result::Result<(), ConstraintViolation> {
        for c in &self.constraints {
            match c {
                Constraint::UniqueNodeProperty { kind, property } => {
                    let mut seen: std::collections::HashSet<String> =
                        std::collections::HashSet::new();
                    for n in self.nodes.values().filter(|n| &n.kind == kind) {
                        if let Some(v) = n.properties.0.get(property) {
                            let value = v.to_string();
                            if !seen.insert(value.clone()) {
                                return Err(ConstraintViolation {
                                    kind: kind.clone(),
                                    message: format!(
                                        "duplicate value {value} for unique property `{property}`"
                                    ),
                                });
                            }
                        }
                    }
                }
                Constraint::PropertyExists { kind, property } => {
                    for n in self.nodes.values().filter(|n| &n.kind == kind) {
                        if !n.properties.0.contains_key(property) {
                            return Err(ConstraintViolation {
                                kind: kind.clone(),
                                message: format!(
                                    "node {} is missing required property `{property}`",
                                    n.id
                                ),
                            });
                        }
                    }
                }
                Constraint::NodeKey { kind, properties } => {
                    let mut seen: std::collections::HashSet<String> =
                        std::collections::HashSet::new();
                    for n in self.nodes.values().filter(|n| &n.kind == kind) {
                        let mut parts = Vec::with_capacity(properties.len());
                        for p in properties {
                            match n.properties.0.get(p) {
                                Some(v) => parts.push(v.to_string()),
                                None => {
                                    return Err(ConstraintViolation {
                                        kind: kind.clone(),
                                        message: format!(
                                            "node {} is missing node-key property `{p}`",
                                            n.id
                                        ),
                                    })
                                }
                            }
                        }
                        // Unit separator joins the tuple into one comparable key.
                        let key = parts.join("\u{1f}");
                        if !seen.insert(key) {
                            return Err(ConstraintViolation {
                                kind: kind.clone(),
                                message: format!(
                                    "duplicate node key {parts:?} on properties {properties:?}"
                                ),
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// A schema constraint a [`NativeGraph`] enforces at transaction commit
/// (RFC ACID "C", Phase 3) — the Neo4j-parity set: UNIQUE, property EXISTS, and
/// NODE KEY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Constraint {
    /// Among nodes of `kind`, the value of `property` must be unique. Nodes of
    /// that kind that do not carry the property are not constrained.
    UniqueNodeProperty {
        /// The node kind (label) the constraint applies to.
        kind: String,
        /// The property whose value must be unique within that kind.
        property: String,
    },
    /// Every node of `kind` must carry `property` (Neo4j property-existence
    /// constraint).
    PropertyExists {
        /// The node kind the constraint applies to.
        kind: String,
        /// The property every such node must have.
        property: String,
    },
    /// The tuple of `properties` is unique among nodes of `kind`, **and** every
    /// such node must carry all of them — Neo4j's NODE KEY (existence +
    /// uniqueness of the combination).
    NodeKey {
        /// The node kind the constraint applies to.
        kind: String,
        /// The properties whose combination forms the key.
        properties: Vec<String>,
    },
}

/// The details of a [`Constraint`] that was violated: the node kind and a
/// human-readable description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintViolation {
    /// The node kind whose constraint was violated.
    pub kind: String,
    /// A human-readable description of the violation.
    pub message: String,
}

impl std::fmt::Display for ConstraintViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "constraint on `{}` violated: {}",
            self.kind, self.message
        )
    }
}

impl std::error::Error for ConstraintViolation {}

/// Why a [`NativeTx::commit`] failed. Both cases leave the graph unchanged and
/// the transaction's writes discarded. A local error type, so the crate-wide
/// [`CoreError`] is not widened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitError {
    /// The graph changed since the transaction began (optimistic conflict);
    /// retry against the new state.
    Conflict,
    /// The transaction's writes would violate a declared [`Constraint`].
    Constraint(ConstraintViolation),
    /// Writing the transaction to the write-ahead log failed (durable engine
    /// only). The transaction is not applied; the message is the I/O error.
    Io(String),
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommitError::Conflict => write!(
                f,
                "transaction conflict: the graph changed since the transaction began; retry"
            ),
            CommitError::Constraint(v) => write!(f, "{v}"),
            CommitError::Io(e) => write!(f, "write-ahead log write failed: {e}"),
        }
    }
}

impl std::error::Error for CommitError {}

/// One entry in the write-ahead log (RFC ACID "D", Phase 3): the durable,
/// replayable record of a single graph mutation. Upserts carry the **applied**
/// record (id/uuid/timestamps already assigned) so replay is deterministic and
/// never regenerates them; deletes carry the id. Appended in commit order and
/// replayed in order by [`NativeGraph::replay`] to reconstruct the in-memory
/// graph after a restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WalOp {
    /// Insert or replace a node (create and update both log this).
    UpsertNode(Node),
    /// Delete a node (and, on replay, cascade its incident edges).
    DeleteNode(u64),
    /// Insert or replace an edge (create and update both log this).
    UpsertEdge(Edge),
    /// Delete an edge.
    DeleteEdge(u64),
}

/// An in-memory, native [`GraphEngine`] (RFC Phase 2/3). See the module docs.
#[derive(Default)]
pub struct NativeGraph {
    inner: RwLock<Arc<Inner>>,
    /// The write-ahead log sink, when the engine was opened durable via
    /// [`open_durable`](Self::open_durable). `None` for an in-memory-only
    /// engine ([`new`](Self::new)). Each direct write appends and fsyncs before
    /// returning, so an acknowledged write survives a crash.
    #[cfg(not(target_arch = "wasm32"))]
    wal: Option<std::sync::Mutex<WalSink>>,
    /// The ordered change-feed: every committed write, in commit order, as a
    /// [`WalOp`] (RFC `docs/rfc-native-core.md`, #307, Phase 6). Secondary
    /// indexes off the graph seam (FTS, vector) keep themselves current by
    /// **tailing** this feed — snapshot the graph once, then apply each change
    /// since a cursor — instead of coupling to the write path. See
    /// [`changes_since`](Self::changes_since).
    feed: std::sync::Mutex<ChangeFeed>,
}

/// The in-memory tail of committed [`WalOp`]s backing [`NativeGraph`]'s
/// change-feed. `start_seq` is the sequence number *before* `ops[0]`, so the
/// change at index `i` carries sequence `start_seq + i + 1` and the current
/// head is `start_seq + ops.len()`. (Trimming consumed history — advancing
/// `start_seq` and draining `ops` — is a later slice; for now the tail is
/// retained in full.)
#[derive(Default)]
struct ChangeFeed {
    start_seq: u64,
    ops: Vec<WalOp>,
}

/// A batch of change-feed entries plus the cursor to resume from.
///
/// Returned by [`NativeGraph::changes_since`]. `cursor` is the sequence number
/// of the last change in `ops` (or the caller's cursor when `ops` is empty), so
/// the next poll is `changes_since(batch.cursor)`.
#[derive(Debug, Clone)]
pub struct ChangeBatch {
    /// The resume cursor — pass to the next [`changes_since`](NativeGraph::changes_since).
    pub cursor: u64,
    /// The changes after the requested cursor, in commit order.
    pub ops: Vec<WalOp>,
    /// `true` when the requested cursor was older than the retained history, so
    /// some changes were skipped and the subscriber must re-snapshot the graph
    /// before applying `ops`. Always `false` until history trimming lands.
    pub lagged: bool,
}

/// The write-ahead-log file plus its path (needed so [`compact_wal`] can
/// atomically rewrite the log in place).
///
/// [`compact_wal`]: NativeGraph::compact_wal
#[cfg(not(target_arch = "wasm32"))]
struct WalSink {
    path: std::path::PathBuf,
    file: std::fs::File,
}

impl NativeGraph {
    /// Create an empty engine. Ids start at 1, matching `Drevo`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Zero-copy node fetch: returns an `Arc<Node>` handle (a refcount bump)
    /// rather than deep-cloning the record the way
    /// [`GraphEngine::get_node`] must to
    /// honour its owned-return contract. Prefer this on read-hot paths where the
    /// caller only needs to *read* the node — a node with a large body/property
    /// map costs the same here as a tiny one.
    pub fn get_node_arc(&self, id: u64) -> Option<Arc<Node>> {
        read(&self.inner).get_node_arc(id)
    }

    /// Zero-copy edge fetch — the [`get_node_arc`](Self::get_node_arc) analogue
    /// for edges.
    pub fn get_edge_arc(&self, id: u64) -> Option<Arc<Edge>> {
        read(&self.inner).get_edge_arc(id)
    }

    /// Distinct neighbours as zero-copy `Arc<Node>` handles — the fan-out
    /// counterpart to [`get_node_arc`](Self::get_node_arc), so expanding a
    /// high-degree node never deep-clones every neighbour's record.
    pub fn neighbors_arc(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Vec<Arc<Node>> {
        read(&self.inner).neighbors_arc(node_id, direction, kind)
    }

    /// The current change-feed head — the sequence number of the most recent
    /// committed change. A caught-up subscriber holds this as its cursor;
    /// `changes_since(change_head())` returns an empty batch.
    pub fn change_head(&self) -> u64 {
        let feed = self.feed.lock().unwrap_or_else(|e| e.into_inner());
        feed.start_seq + feed.ops.len() as u64
    }

    /// Read every committed change after `cursor`, in commit order (RFC
    /// `docs/rfc-native-core.md`, #307, Phase 6 change-feed). The returned
    /// [`ChangeBatch`] carries the resume cursor and, if the cursor had fallen
    /// behind the retained history, a `lagged` flag telling the subscriber to
    /// re-snapshot first.
    ///
    /// Subscribers follow snapshot-then-tail: seed the index from a
    /// [`snapshot`](Self::snapshot), remember [`change_head`](Self::change_head)
    /// at that instant, then poll `changes_since(cursor)` and advance the cursor
    /// by each batch — so an FTS or vector index stays current without touching
    /// the write path.
    pub fn changes_since(&self, cursor: u64) -> ChangeBatch {
        let feed = self.feed.lock().unwrap_or_else(|e| e.into_inner());
        let head = feed.start_seq + feed.ops.len() as u64;
        if cursor < feed.start_seq {
            // The cursor predates the retained window — history was trimmed, so
            // the subscriber must re-snapshot before applying what remains.
            return ChangeBatch {
                cursor: head,
                ops: feed.ops.clone(),
                lagged: true,
            };
        }
        let from = (cursor - feed.start_seq).min(feed.ops.len() as u64) as usize;
        ChangeBatch {
            cursor: head,
            ops: feed.ops[from..].to_vec(),
            lagged: false,
        }
    }

    /// Drop change-feed history at or before `cursor`, bounding the feed's
    /// memory (RFC `docs/rfc-native-core.md`, #307, Phase 6).
    ///
    /// The caller passes the **minimum cursor across its live subscribers**, so
    /// no subscriber that is still catching up loses a change it has not seen.
    /// After trimming, a subscriber whose cursor is below the new floor gets
    /// `lagged = true` from [`changes_since`](Self::changes_since) and must
    /// re-snapshot. `cursor` is clamped to the current head (the feed never
    /// trims a change that has not been produced). Returns the new retained
    /// floor — the sequence number before the oldest retained change.
    pub fn trim_before(&self, cursor: u64) -> u64 {
        let mut feed = self.feed.lock().unwrap_or_else(|e| e.into_inner());
        let head = feed.start_seq + feed.ops.len() as u64;
        let target = cursor.min(head);
        if target <= feed.start_seq {
            return feed.start_seq;
        }
        let drop = (target - feed.start_seq) as usize;
        feed.ops.drain(..drop);
        feed.start_seq = target;
        feed.start_seq
    }

    /// The oldest sequence number still retained on the change-feed — the floor
    /// below which [`changes_since`](Self::changes_since) reports `lagged`. `0`
    /// until [`trim_before`](Self::trim_before) advances it.
    pub fn change_floor(&self) -> u64 {
        self.feed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .start_seq
    }

    /// Record committed changes into the WAL (when the engine is durable) and
    /// the change-feed, in commit order. The single record point for every
    /// mutation path, so the change-feed sees exactly what the WAL persists.
    ///
    /// The WAL append happens **first**: if it fails the feed is left untouched,
    /// so the feed never advertises a change the WAL did not persist.
    fn record(&self, ops: &[WalOp]) -> Result<()> {
        #[cfg(not(target_arch = "wasm32"))]
        self.wal_append_batch(ops)?;
        let mut feed = self.feed.lock().unwrap_or_else(|e| e.into_inner());
        feed.ops.extend_from_slice(ops);
        Ok(())
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

    /// Begin a [`NativeTx`] — a transaction that reads a consistent snapshot of
    /// the graph as of now, buffers its writes privately, and applies them all
    /// atomically on [`commit`](NativeTx::commit) (RFC ACID "I"/"A", Phase 3).
    ///
    /// Isolation is snapshot: the transaction never sees another writer's
    /// changes made after it began. Concurrency control is optimistic — the
    /// commit succeeds only if the graph has not changed since the transaction
    /// began, otherwise it returns [`CommitError::Conflict`] and the caller
    /// retries. Dropping the transaction without committing rolls it back.
    pub fn begin(&self) -> NativeTx<'_> {
        let base = Arc::clone(&read(&self.inner));
        NativeTx {
            engine: self,
            working: Arc::clone(&base),
            base,
            ops: Vec::new(),
        }
    }

    /// Declare a schema [`Constraint`] the engine enforces at transaction
    /// commit (RFC ACID "C").
    ///
    /// The constraint is validated against the current data first: if existing
    /// nodes already violate it, it is **not** stored and the
    /// [`ConstraintViolation`] is returned (matching Neo4j, which refuses to
    /// create a constraint the data does not already satisfy).
    ///
    /// # Errors
    /// [`ConstraintViolation`] if the current graph already violates the
    /// constraint.
    pub fn add_constraint(
        &self,
        constraint: Constraint,
    ) -> std::result::Result<(), ConstraintViolation> {
        let mut guard = write(&self.inner);
        let g = Arc::make_mut(&mut guard);
        g.constraints.push(constraint);
        if let Err(e) = g.validate_constraints() {
            g.constraints.pop();
            return Err(e);
        }
        Ok(())
    }

    /// Dump the current state as a write-ahead-log op sequence: every node then
    /// every edge as an `Upsert`, in ascending id order (RFC ACID "D",
    /// Phase 3). This is the "snapshot" form of the log — replaying it into a
    /// fresh engine via [`replay`](Self::replay) reproduces the graph exactly,
    /// which is what lets a periodic snapshot compact/truncate the incremental
    /// WAL. Constraints are schema, not data, and are not part of the dump.
    pub fn dump_wal(&self) -> Vec<WalOp> {
        read(&self.inner).to_wal_ops()
    }

    /// Rebuild an engine by replaying a [`WalOp`] sequence in order (RFC ACID
    /// "D", Phase 3) — the recovery path: load a snapshot then replay the WAL
    /// tail, or replay a full WAL from empty. Ids/uuids/timestamps come from the
    /// logged records; the id counters advance past every replayed id so a
    /// post-recovery create never collides with a recovered node/edge.
    pub fn replay(ops: impl IntoIterator<Item = WalOp>) -> Self {
        let mut inner = Inner::default();
        for op in ops {
            inner.apply_wal_op(op);
        }
        NativeGraph {
            inner: RwLock::new(Arc::new(inner)),
            #[cfg(not(target_arch = "wasm32"))]
            wal: None,
            feed: std::sync::Mutex::new(ChangeFeed::default()),
        }
    }

    /// Open a **durable** engine backed by a write-ahead log at `path` (RFC
    /// ACID "D", Phase 3). If the file exists its ops are replayed to
    /// reconstruct the graph; then the file is opened for append and every
    /// subsequent direct write is logged and fsynced before it returns, so a
    /// write that has returned survives a crash. Reopening the same path
    /// recovers the graph.
    ///
    /// Both direct writes and transactions ([`begin`](Self::begin)) are durable:
    /// a committed transaction is logged as one fsynced batch, so it recovers
    /// all-or-nothing.
    ///
    /// # Errors
    /// [`CoreError::Io`] / [`CoreError::Json`] on a filesystem or log-parse
    /// failure.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open_durable(path: impl AsRef<std::path::Path>) -> Result<Self> {
        use std::io::{BufRead, BufReader};
        let path = path.as_ref();
        let mut inner = Inner::default();
        if path.exists() {
            let f = std::fs::File::open(path)?;
            for line in BufReader::new(f).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let op: WalOp = serde_json::from_str(&line)?;
                inner.apply_wal_op(op);
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(NativeGraph {
            inner: RwLock::new(Arc::new(inner)),
            wal: Some(std::sync::Mutex::new(WalSink {
                path: path.to_path_buf(),
                file,
            })),
            feed: std::sync::Mutex::new(ChangeFeed::default()),
        })
    }

    /// Compact the write-ahead log: rewrite it as the current state's snapshot
    /// form (every node/edge as one `Upsert`), discarding the superseded
    /// incremental history that has accumulated from overwrites and deletes
    /// (RFC ACID "D", Phase 3). Bounds the log's growth without changing the
    /// recovered graph.
    ///
    /// Atomic and crash-safe: the snapshot is written to a temp file and fsynced,
    /// then `rename`d over the live log (an atomic replace on the same
    /// filesystem), then the append handle is reopened on the new file. A crash
    /// at any point leaves either the old or the new complete log — never a torn
    /// one. Writes are quiesced for the duration (the inner write lock is held).
    /// A no-op for an in-memory-only engine.
    ///
    /// # Errors
    /// [`CoreError::Io`] / [`CoreError::Json`] on a filesystem or encode
    /// failure.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn compact_wal(&self) -> Result<()> {
        use std::io::Write;
        let Some(wal) = &self.wal else {
            return Ok(());
        };
        // Hold the write lock so no mutation appends to the log mid-compaction.
        let inner = write(&self.inner);
        let ops = inner.to_wal_ops();
        let mut sink = wal.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = sink.path.with_extension("wal.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            for op in &ops {
                let mut line = serde_json::to_string(op)?;
                line.push('\n');
                f.write_all(line.as_bytes())?;
            }
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &sink.path)?;
        sink.file = std::fs::OpenOptions::new().append(true).open(&sink.path)?;
        Ok(())
    }

    /// Append a batch of ops and fsync **once**, when this engine is durable —
    /// the transaction path, so a whole commit costs a single fsync. A no-op for
    /// an in-memory-only engine or an empty batch.
    #[cfg(not(target_arch = "wasm32"))]
    fn wal_append_batch(&self, ops: &[WalOp]) -> Result<()> {
        use std::io::Write;
        if ops.is_empty() {
            return Ok(());
        }
        if let Some(wal) = &self.wal {
            let mut sink = wal.lock().unwrap_or_else(|e| e.into_inner());
            for op in ops {
                let mut line = serde_json::to_string(op)?;
                line.push('\n');
                sink.file.write_all(line.as_bytes())?;
            }
            sink.file.sync_all()?;
        }
        Ok(())
    }
}

/// A transaction over a [`NativeGraph`]: reads a consistent snapshot taken at
/// [`begin`](NativeGraph::begin), buffers writes on a private working copy, and
/// applies them atomically at [`commit`](Self::commit) (or discards them on
/// [`rollback`](Self::rollback) / drop). Reads reflect the transaction's own
/// buffered writes (read-your-writes). See [`NativeGraph::begin`].
pub struct NativeTx<'a> {
    engine: &'a NativeGraph,
    /// The snapshot the transaction began from — used to detect, at commit, a
    /// concurrent change (the live `Arc` pointer will differ).
    base: Arc<Inner>,
    /// The private working copy; starts shared with `base` and copy-on-writes
    /// on the first mutation.
    working: Arc<Inner>,
    /// The write-ahead-log ops this transaction produced, in causal order,
    /// flushed to the engine's WAL as one atomic batch at commit (durable
    /// engine only).
    ops: Vec<WalOp>,
}

impl NativeTx<'_> {
    /// Commit the buffered writes atomically. Succeeds only if (a) the graph
    /// has not changed since [`begin`](NativeGraph::begin) and (b) the resulting
    /// state satisfies every declared [`Constraint`]; otherwise the writes are
    /// discarded and the reason is returned for the caller to handle.
    ///
    /// # Errors
    /// - [`CommitError::Conflict`] if another writer committed since this
    ///   transaction began.
    /// - [`CommitError::Constraint`] if the transaction's writes would violate a
    ///   declared constraint.
    pub fn commit(self) -> std::result::Result<(), CommitError> {
        let mut live = write(&self.engine.inner);
        if !Arc::ptr_eq(&live, &self.base) {
            return Err(CommitError::Conflict);
        }
        self.working
            .validate_constraints()
            .map_err(CommitError::Constraint)?;
        // Durability + change-feed: record the whole transaction as one fsynced
        // batch *before* the swap, so the commit is all-or-nothing — an I/O
        // failure leaves the graph, the log, and the feed untouched. On a
        // non-durable engine the WAL step is a no-op and only the feed advances.
        self.engine
            .record(&self.ops)
            .map_err(|e| CommitError::Io(e.to_string()))?;
        *live = self.working;
        Ok(())
    }

    /// Discard the transaction's buffered writes. Equivalent to dropping it.
    pub fn rollback(self) {}

    // ----- reads (reflect the transaction's own buffered writes) -------------

    /// Fetch a node by id within the transaction.
    pub fn get_node(&self, id: u64) -> Option<Node> {
        self.working.get_node(id)
    }

    /// Fetch an edge by id within the transaction.
    pub fn get_edge(&self, id: u64) -> Option<Edge> {
        self.working.get_edge(id)
    }

    /// Distinct neighbour ids within the transaction.
    pub fn neighbor_ids(&self, node_id: u64, direction: Direction, kind: Option<&str>) -> Vec<u64> {
        self.working.neighbor_ids(node_id, direction, kind)
    }

    /// Every node visible within the transaction.
    pub fn all_nodes(&self) -> Vec<Node> {
        self.working.all_nodes()
    }

    /// Every edge visible within the transaction.
    pub fn all_edges(&self) -> Vec<Edge> {
        self.working.all_edges()
    }

    // ----- writes (buffered on the private working copy) ---------------------

    /// Create a node within the transaction. Mirrors
    /// [`GraphEngine::create_node`].
    ///
    /// # Errors
    /// [`CoreError::DuplicateTitle`]
    /// if the title is taken in the transaction's view.
    pub fn create_node(&mut self, new_node: NewNode) -> Result<Node> {
        let node = Arc::make_mut(&mut self.working).create_node(new_node)?;
        self.ops.push(WalOp::UpsertNode(node.clone()));
        Ok(node)
    }

    /// Update a node within the transaction.
    ///
    /// # Errors
    /// Propagates any [`CoreError`] from the update.
    pub fn update_node(&mut self, id: u64, patch: NodePatch) -> Result<Node> {
        let node = Arc::make_mut(&mut self.working).update_node(id, patch)?;
        self.ops.push(WalOp::UpsertNode(node.clone()));
        Ok(node)
    }

    /// Delete a node (and its incident edges) within the transaction.
    ///
    /// # Errors
    /// [`CoreError::NodeNotFound`] if
    /// absent in the transaction's view.
    pub fn delete_node(&mut self, id: u64) -> Result<()> {
        Arc::make_mut(&mut self.working).delete_node(id)?;
        self.ops.push(WalOp::DeleteNode(id));
        Ok(())
    }

    /// Create an edge within the transaction.
    ///
    /// # Errors
    /// Propagates any [`CoreError`] from the create.
    pub fn create_edge(&mut self, new_edge: NewEdge) -> Result<Edge> {
        let edge = Arc::make_mut(&mut self.working).create_edge(new_edge)?;
        self.ops.push(WalOp::UpsertEdge(edge.clone()));
        Ok(edge)
    }

    /// Update an edge within the transaction.
    ///
    /// # Errors
    /// Propagates any [`CoreError`] from the update.
    pub fn update_edge(&mut self, id: u64, patch: EdgePatch) -> Result<Edge> {
        let edge = Arc::make_mut(&mut self.working).update_edge(id, patch)?;
        self.ops.push(WalOp::UpsertEdge(edge.clone()));
        Ok(edge)
    }

    /// Delete an edge within the transaction.
    ///
    /// # Errors
    /// [`CoreError::EdgeNotFound`] if
    /// absent in the transaction's view.
    pub fn delete_edge(&mut self, id: u64) -> Result<()> {
        Arc::make_mut(&mut self.working).delete_edge(id)?;
        self.ops.push(WalOp::DeleteEdge(id));
        Ok(())
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
        let node = Arc::make_mut(&mut write(&self.inner)).create_node(new_node)?;
        self.record(&[WalOp::UpsertNode(node.clone())])?;
        Ok(node)
    }

    fn get_node(&self, id: u64) -> Result<Option<Arc<Node>>> {
        // Zero-copy: an `Arc` handle to the stored record (refcount bump).
        Ok(read(&self.inner).get_node_arc(id))
    }

    fn update_node(&self, id: u64, patch: NodePatch) -> Result<Node> {
        let node = Arc::make_mut(&mut write(&self.inner)).update_node(id, patch)?;
        self.record(&[WalOp::UpsertNode(node.clone())])?;
        Ok(node)
    }

    fn delete_node(&self, id: u64) -> Result<()> {
        Arc::make_mut(&mut write(&self.inner)).delete_node(id)?;
        self.record(&[WalOp::DeleteNode(id)])?;
        Ok(())
    }

    fn create_edge(&self, new_edge: NewEdge) -> Result<Edge> {
        let edge = Arc::make_mut(&mut write(&self.inner)).create_edge(new_edge)?;
        self.record(&[WalOp::UpsertEdge(edge.clone())])?;
        Ok(edge)
    }

    fn get_edge(&self, id: u64) -> Result<Option<Edge>> {
        Ok(read(&self.inner).get_edge(id))
    }

    fn update_edge(&self, id: u64, patch: EdgePatch) -> Result<Edge> {
        let edge = Arc::make_mut(&mut write(&self.inner)).update_edge(id, patch)?;
        self.record(&[WalOp::UpsertEdge(edge.clone())])?;
        Ok(edge)
    }

    fn delete_edge(&self, id: u64) -> Result<()> {
        Arc::make_mut(&mut write(&self.inner)).delete_edge(id)?;
        self.record(&[WalOp::DeleteEdge(id)])?;
        Ok(())
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
    ) -> Result<Vec<Arc<Node>>> {
        Ok(read(&self.inner).neighbors_arc(node_id, direction, kind))
    }

    fn edges_of(&self, node_id: u64, direction: Direction) -> Result<Vec<Edge>> {
        Ok(read(&self.inner).edges_of(node_id, direction))
    }

    fn all_nodes(&self) -> Result<Vec<Arc<Node>>> {
        Ok(read(&self.inner).all_nodes_arc())
    }

    fn all_edges(&self) -> Result<Vec<Edge>> {
        Ok(read(&self.inner).all_edges())
    }

    fn nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Arc<Node>>> {
        Ok(read(&self.inner).nodes_by_kind_arc(kind, limit, offset))
    }

    fn export_dump(&self) -> Result<Dump> {
        let g = read(&self.inner);
        let nodes = g.all_nodes();
        let edges = g.all_edges();
        // Derive the counters from the live max id (`max + 1`), matching
        // `Drevo::build_dump` byte-for-byte so a dump is engine-independent.
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
        // Buffer the ops that actually land so they can be journaled in one
        // fsync after the in-memory apply succeeds (durability parity with the
        // live write paths).
        let mut applied: Vec<WalOp> = Vec::with_capacity(dump.nodes.len() + dump.edges.len());
        {
            let mut guard = write(&self.inner);
            let g = Arc::make_mut(&mut guard);

            // --- Nodes: skip byte-equal, reject id-collision, else upsert ---
            for node in &dump.nodes {
                match g.get_node(node.id) {
                    Some(existing) if existing == *node => {
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
                g.apply_wal_op(WalOp::UpsertNode(node.clone()));
                applied.push(WalOp::UpsertNode(node.clone()));
                report.nodes_imported += 1;
            }

            // --- Edges: endpoints must resolve (nodes are already applied) ---
            for edge in &dump.edges {
                match g.get_edge(edge.id) {
                    Some(existing) if existing == *edge => {
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
                if g.get_node(edge.from_id).is_none() {
                    return Err(CoreError::NodeNotFound(edge.from_id));
                }
                if g.get_node(edge.to_id).is_none() {
                    return Err(CoreError::NodeNotFound(edge.to_id));
                }
                g.apply_wal_op(WalOp::UpsertEdge(edge.clone()));
                applied.push(WalOp::UpsertEdge(edge.clone()));
                report.edges_imported += 1;
            }

            // Clamp id counters above every imported id so a post-migration
            // create never reuses one (`apply_wal_op` already raised them to
            // the max imported id; honour a higher producer counter too).
            g.next_node_id = g.next_node_id.max(dump.next_node_id.saturating_sub(1));
            g.next_edge_id = g.next_edge_id.max(dump.next_edge_id.saturating_sub(1));
        }

        if !applied.is_empty() {
            self.record(&applied)?;
        }

        Ok(report)
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

    /// Zero-copy node fetch within the snapshot — an `Arc<Node>` handle instead
    /// of a deep clone (see [`NativeGraph::get_node_arc`]).
    pub fn get_node_arc(&self, id: u64) -> Option<Arc<Node>> {
        self.inner.get_node_arc(id)
    }

    /// Zero-copy edge fetch within the snapshot (see
    /// [`NativeGraph::get_edge_arc`]).
    pub fn get_edge_arc(&self, id: u64) -> Option<Arc<Edge>> {
        self.inner.get_edge_arc(id)
    }

    /// Distinct neighbour ids in `direction`, optionally kind-filtered.
    pub fn neighbor_ids(&self, node_id: u64, direction: Direction, kind: Option<&str>) -> Vec<u64> {
        self.inner.neighbor_ids(node_id, direction, kind)
    }

    /// Distinct neighbours as zero-copy `Arc<Node>` handles (see
    /// [`NativeGraph::neighbors_arc`]).
    pub fn neighbors_arc(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Vec<Arc<Node>> {
        self.inner.neighbors_arc(node_id, direction, kind)
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

#[cfg(test)]
mod kind_index_tests {
    //! The private node-kind index must always equal the ground truth derived
    //! from the live node set — the definitive guard that every node-map
    //! mutation site (create / update / delete / WAL replay) keeps it in sync.
    use super::*;
    use std::collections::BTreeSet;

    fn nn(kind: &str, title: &str) -> NewNode {
        NewNode {
            kind: kind.into(),
            title: title.into(),
            body: String::new(),
            body_html: String::new(),
            properties: Default::default(),
        }
    }

    /// The kind index recomputed from scratch off `nodes` — what any correct
    /// incremental maintenance must converge to.
    fn ground_truth(inner: &Inner) -> HashMap<String, BTreeSet<u64>> {
        let mut truth: HashMap<String, BTreeSet<u64>> = HashMap::new();
        for node in inner.nodes.values() {
            truth.entry(node.kind.clone()).or_default().insert(node.id);
        }
        truth
    }

    #[test]
    fn index_matches_ground_truth_after_mixed_workload() {
        let mut inner = Inner::default();
        let a = inner.create_node(nn("person", "a")).unwrap();
        let b = inner.create_node(nn("person", "b")).unwrap();
        let _c = inner.create_node(nn("city", "c")).unwrap();
        assert_eq!(inner.kind_index, ground_truth(&inner));

        // Re-kind b (person -> city): must move buckets.
        inner
            .update_node(
                b.id,
                NodePatch {
                    kind: Some("city".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(inner.kind_index, ground_truth(&inner));

        // Delete a: person bucket empties and is dropped (no empty bucket left).
        inner.delete_node(a.id).unwrap();
        assert_eq!(inner.kind_index, ground_truth(&inner));
        assert!(!inner.kind_index.contains_key("person"));

        // A replacing WAL upsert that changes an existing node's kind must
        // unindex the old kind and index the new one.
        let mut c_rekind = inner.get_node(_c.id).unwrap();
        c_rekind.kind = "town".into();
        inner.apply_wal_op(WalOp::UpsertNode(c_rekind));
        assert_eq!(inner.kind_index, ground_truth(&inner));
        // c left `city` for `town`; b (re-kinded to city earlier) still holds it.
        assert_eq!(inner.kind_index["town"], BTreeSet::from([_c.id]));
        assert_eq!(inner.kind_index["city"], BTreeSet::from([b.id]));

        // A WAL delete removes its id from the index.
        inner.apply_wal_op(WalOp::DeleteNode(_c.id));
        assert_eq!(inner.kind_index, ground_truth(&inner));
    }
}
