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

use serde::{Deserialize, Serialize};

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

    /// Apply one [`WalOp`] during replay. Upserts re-insert a stored record
    /// verbatim (ids/uuids/timestamps preserved) and advance the id counters so
    /// a post-recovery create never reuses an id; deletes mirror the live paths.
    /// Derived structures (adjacency, title and kind indexes) are rebuilt from
    /// the record.
    fn apply_wal_op(&mut self, op: WalOp) {
        match op {
            WalOp::UpsertNode(node) => {
                if let Some(old) = self.nodes.get(&node.id) {
                    if old.title != node.title {
                        self.titles.remove(&old.title);
                    }
                }
                self.titles.insert(node.title.clone(), node.id);
                self.next_node_id = self.next_node_id.max(node.id);
                self.nodes.insert(node.id, node);
            }
            WalOp::DeleteNode(id) => {
                if let Some(node) = self.nodes.get(&id).cloned() {
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
                self.edges.insert(edge.id, edge);
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
            let Constraint::UniqueNodeProperty { kind, property } = c;
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for n in self.nodes.values() {
                if &n.kind != kind {
                    continue;
                }
                if let Some(v) = n.properties.0.get(property) {
                    let value = v.to_string();
                    if !seen.insert(value.clone()) {
                        return Err(ConstraintViolation {
                            kind: kind.clone(),
                            property: property.clone(),
                            value,
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

/// A schema constraint a [`NativeGraph`] enforces at transaction commit
/// (RFC ACID "C", Phase 3).
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
}

/// The details of a [`Constraint`] that was violated: which kind/property, and
/// the (JSON-encoded) value that appeared more than once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintViolation {
    /// The node kind whose constraint was violated.
    pub kind: String,
    /// The constrained property.
    pub property: String,
    /// The duplicated value, JSON-encoded.
    pub value: String,
}

impl std::fmt::Display for ConstraintViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unique constraint on {}.{} violated by duplicate value {}",
            self.kind, self.property, self.value
        )
    }
}

impl std::error::Error for ConstraintViolation {}

/// Why a [`NativeTx::commit`] failed. Both cases leave the graph unchanged and
/// the transaction's writes discarded. A local error type, so the crate-wide
/// [`DrevoError`] is not widened.
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
    wal: Option<std::sync::Mutex<std::fs::File>>,
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
        let g = read(&self.inner);
        let mut ops = Vec::with_capacity(g.nodes.len() + g.edges.len());
        let mut nodes: Vec<&Node> = g.nodes.values().collect();
        nodes.sort_by_key(|n| n.id);
        for n in nodes {
            ops.push(WalOp::UpsertNode(n.clone()));
        }
        let mut edges: Vec<&Edge> = g.edges.values().collect();
        edges.sort_by_key(|e| e.id);
        for e in edges {
            ops.push(WalOp::UpsertEdge(e.clone()));
        }
        ops
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
    /// [`DrevoError::Io`] / [`DrevoError::Json`] on a filesystem or log-parse
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
            wal: Some(std::sync::Mutex::new(file)),
        })
    }

    /// Append one op to the WAL and fsync, when this engine is durable. A no-op
    /// for an in-memory-only engine.
    #[cfg(not(target_arch = "wasm32"))]
    fn wal_append(&self, op: &WalOp) -> Result<()> {
        self.wal_append_batch(std::slice::from_ref(op))
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
            let mut f = wal.lock().unwrap_or_else(|e| e.into_inner());
            for op in ops {
                let mut line = serde_json::to_string(op)?;
                line.push('\n');
                f.write_all(line.as_bytes())?;
            }
            f.sync_all()?;
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
        // Durability: log the whole transaction as one fsynced batch *before*
        // the swap, so the commit is all-or-nothing — an I/O failure leaves the
        // graph and the log untouched. A no-op on a non-durable engine.
        #[cfg(not(target_arch = "wasm32"))]
        self.engine
            .wal_append_batch(&self.ops)
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
    /// [`DrevoError::DuplicateTitle`]
    /// if the title is taken in the transaction's view.
    pub fn create_node(&mut self, new_node: NewNode) -> Result<Node> {
        let node = Arc::make_mut(&mut self.working).create_node(new_node)?;
        self.ops.push(WalOp::UpsertNode(node.clone()));
        Ok(node)
    }

    /// Update a node within the transaction.
    ///
    /// # Errors
    /// Propagates any [`DrevoError`] from the update.
    pub fn update_node(&mut self, id: u64, patch: NodePatch) -> Result<Node> {
        let node = Arc::make_mut(&mut self.working).update_node(id, patch)?;
        self.ops.push(WalOp::UpsertNode(node.clone()));
        Ok(node)
    }

    /// Delete a node (and its incident edges) within the transaction.
    ///
    /// # Errors
    /// [`DrevoError::NodeNotFound`] if
    /// absent in the transaction's view.
    pub fn delete_node(&mut self, id: u64) -> Result<()> {
        Arc::make_mut(&mut self.working).delete_node(id)?;
        self.ops.push(WalOp::DeleteNode(id));
        Ok(())
    }

    /// Create an edge within the transaction.
    ///
    /// # Errors
    /// Propagates any [`DrevoError`] from the create.
    pub fn create_edge(&mut self, new_edge: NewEdge) -> Result<Edge> {
        let edge = Arc::make_mut(&mut self.working).create_edge(new_edge)?;
        self.ops.push(WalOp::UpsertEdge(edge.clone()));
        Ok(edge)
    }

    /// Update an edge within the transaction.
    ///
    /// # Errors
    /// Propagates any [`DrevoError`] from the update.
    pub fn update_edge(&mut self, id: u64, patch: EdgePatch) -> Result<Edge> {
        let edge = Arc::make_mut(&mut self.working).update_edge(id, patch)?;
        self.ops.push(WalOp::UpsertEdge(edge.clone()));
        Ok(edge)
    }

    /// Delete an edge within the transaction.
    ///
    /// # Errors
    /// [`DrevoError::EdgeNotFound`] if
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
        #[cfg(not(target_arch = "wasm32"))]
        self.wal_append(&WalOp::UpsertNode(node.clone()))?;
        Ok(node)
    }

    fn get_node(&self, id: u64) -> Result<Option<Node>> {
        Ok(read(&self.inner).get_node(id))
    }

    fn update_node(&self, id: u64, patch: NodePatch) -> Result<Node> {
        let node = Arc::make_mut(&mut write(&self.inner)).update_node(id, patch)?;
        #[cfg(not(target_arch = "wasm32"))]
        self.wal_append(&WalOp::UpsertNode(node.clone()))?;
        Ok(node)
    }

    fn delete_node(&self, id: u64) -> Result<()> {
        Arc::make_mut(&mut write(&self.inner)).delete_node(id)?;
        #[cfg(not(target_arch = "wasm32"))]
        self.wal_append(&WalOp::DeleteNode(id))?;
        Ok(())
    }

    fn create_edge(&self, new_edge: NewEdge) -> Result<Edge> {
        let edge = Arc::make_mut(&mut write(&self.inner)).create_edge(new_edge)?;
        #[cfg(not(target_arch = "wasm32"))]
        self.wal_append(&WalOp::UpsertEdge(edge.clone()))?;
        Ok(edge)
    }

    fn get_edge(&self, id: u64) -> Result<Option<Edge>> {
        Ok(read(&self.inner).get_edge(id))
    }

    fn update_edge(&self, id: u64, patch: EdgePatch) -> Result<Edge> {
        let edge = Arc::make_mut(&mut write(&self.inner)).update_edge(id, patch)?;
        #[cfg(not(target_arch = "wasm32"))]
        self.wal_append(&WalOp::UpsertEdge(edge.clone()))?;
        Ok(edge)
    }

    fn delete_edge(&self, id: u64) -> Result<()> {
        Arc::make_mut(&mut write(&self.inner)).delete_edge(id)?;
        #[cfg(not(target_arch = "wasm32"))]
        self.wal_append(&WalOp::DeleteEdge(id))?;
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
