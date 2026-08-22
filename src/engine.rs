//! The `GraphEngine` seam — the graph-level abstraction the query layers
//! (Cypher executor, traversal, planner) will depend on instead of reaching
//! into a concrete store's KV-encoded internals.
//!
//! # Why this exists
//!
//! drevo today encodes the whole graph as byte-keyed rows over a
//! [`crate::storage::StorageBackend`] (`node:`, `out:{from}:{kind}:{edge_id}`,
//! …). That KV vocabulary is the wrong seam for a *native* graph engine, whose
//! whole point is to drop key encoding in favour of index-free adjacency (see
//! the RFC at `docs/rfc-native-core.md`, tracking issue #307).
//!
//! `GraphEngine` raises the boundary one level, expressing storage in **graph
//! terms** — nodes, edges, adjacency — so a future in-memory-first native
//! `drevo-core` can be a drop-in alternative to today's KV-backed
//! [`Drevo`](crate::db::Drevo).
//!
//! # This is the first, additive thread
//!
//! The trait is introduced **without changing any behaviour**:
//! [`Drevo`](crate::db::Drevo) implements it by delegating to its existing
//! inherent methods. Nothing in the
//! codebase depends on the trait yet; it exists so subsequent slices can port
//! call sites onto it (strangler-fig) and so the eventual native engine has an
//! executable contract — exercised by `tests/graph_engine_tests.rs`.

use crate::db::Drevo;
use crate::dump::{Dump, ImportReport};
use crate::error::Result;
use crate::model::{Direction, Edge, EdgePatch, NewEdge, NewNode, Node, NodePatch};

/// Graph-level storage and traversal, expressed in graph terms rather than the
/// byte-key KV vocabulary of [`crate::storage::StorageBackend`].
///
/// The method set mirrors the core of [`Drevo`]'s inherent API — node/edge CRUD
/// plus adjacency expansion — so that today's KV-backed store and a future
/// native engine present the *same* contract to the query layers above them.
/// Secondary concerns (FTS, vectors, transactions, statistics) are intentionally
/// out of scope for this first slice and will be layered on as later phases wire
/// call sites onto the seam.
///
/// The trait is **object-safe** (`&dyn GraphEngine` works) so call sites can be
/// generic over the engine without monomorphising the whole executor.
pub trait GraphEngine {
    /// Create a node, allocating its id/uuid/timestamps. Mirrors
    /// [`Drevo::create_node`].
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store
    /// (e.g. a title-uniqueness violation).
    fn create_node(&self, new_node: NewNode) -> Result<Node>;

    /// Fetch a node by id, or `Ok(None)` if absent. Mirrors [`Drevo::get_node`].
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store.
    fn get_node(&self, id: u64) -> Result<Option<Node>>;

    /// Apply a partial update to a node. Mirrors [`Drevo::update_node`].
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store
    /// (e.g. the node not existing, or a title-uniqueness violation).
    fn update_node(&self, id: u64, patch: NodePatch) -> Result<Node>;

    /// Delete a node (and its incident edges/indexes). Mirrors
    /// [`Drevo::delete_node`].
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store.
    fn delete_node(&self, id: u64) -> Result<()>;

    /// Create an edge between two existing nodes. Mirrors [`Drevo::create_edge`].
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store.
    fn create_edge(&self, new_edge: NewEdge) -> Result<Edge>;

    /// Fetch an edge by id, or `Ok(None)` if absent. Mirrors [`Drevo::get_edge`].
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store.
    fn get_edge(&self, id: u64) -> Result<Option<Edge>>;

    /// Apply a partial update to an edge (kind / weight / properties). Mirrors
    /// [`Drevo::update_edge`].
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store
    /// (e.g. the edge not existing, or a non-finite weight).
    fn update_edge(&self, id: u64, patch: EdgePatch) -> Result<Edge>;

    /// Delete an edge (and its adjacency/index entries). Mirrors
    /// [`Drevo::delete_edge`].
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store.
    fn delete_edge(&self, id: u64) -> Result<()>;

    /// Return the **distinct** node ids adjacent to `node_id` in `direction`,
    /// optionally restricted to edges of `kind`. Mirrors [`Drevo::neighbor_ids`]
    /// — the id-only fan-out that reads straight from the adjacency index.
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store.
    fn neighbor_ids(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<u64>>;

    /// Return the adjacent **nodes** (loaded once each) in `direction`,
    /// optionally restricted to edges of `kind`. Mirrors [`Drevo::neighbors`].
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store.
    fn neighbors(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<Node>>;

    /// Return the full [`Edge`] records incident to `node_id` in `direction`
    /// (the weighted/full-edge expansion, which loads each edge record —
    /// unlike [`neighbor_ids`](Self::neighbor_ids)). Mirrors [`Drevo::edges_of`].
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store.
    fn edges_of(&self, node_id: u64, direction: Direction) -> Result<Vec<Edge>>;

    /// Return **every** node in the store (a full scan — the label-less
    /// `MATCH (n)`). Mirrors `Drevo::collect_all_nodes`.
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store.
    fn all_nodes(&self) -> Result<Vec<Node>>;

    /// Return **every** edge in the store (a full scan — the anonymous
    /// `MATCH ()-[r]->()`). Mirrors `Drevo::collect_all_edges`.
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store.
    fn all_edges(&self) -> Result<Vec<Edge>>;

    /// Return nodes of a given `kind` (label scan), paginated by `limit` /
    /// `offset`. Mirrors [`Drevo::list_nodes_by_kind`].
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store.
    fn nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Node>>;

    /// Export the entire graph as a `drevo-json-v1` [`Dump`] — every node and
    /// edge plus the id-allocation counters — the interchange format used for
    /// backup, JSON/GraphML round-trips, and cross-engine migration.
    ///
    /// This is the read half of the migration seam: `dst.apply_dump(src.
    /// export_dump()?)` moves a live graph between any two engines
    /// (see [`crate::migrate::migrate`]).
    ///
    /// # Errors
    /// Propagates any [`crate::error::DrevoError`] from the underlying store.
    fn export_dump(&self) -> Result<Dump>;

    /// Bulk-load a [`Dump`] into this engine **verbatim**, preserving every
    /// node/edge id (so edges stay connected) and clamping the id counters
    /// above every imported id. Nodes/edges already present byte-for-byte are
    /// skipped (idempotent re-import); an id that collides with *different*
    /// content, or an edge whose endpoint is missing, is an error.
    ///
    /// This is the write half of the migration seam. The counts in the
    /// returned [`ImportReport`] separate freshly-inserted from skipped rows.
    ///
    /// # Errors
    /// * [`crate::error::DrevoError::NodeNotFound`] — an edge references an
    ///   endpoint that neither exists nor is supplied by the dump.
    /// * Other [`crate::error::DrevoError`] variants — an id collision against
    ///   different content, a title/uuid clash, or a backend failure.
    fn apply_dump(&self, dump: Dump) -> Result<ImportReport>;
}

/// The current KV-backed store is the first `GraphEngine` implementation. Every
/// method is a straight delegation to the inherent method of the same name, so
/// this introduces the seam with **no** behaviour change.
impl GraphEngine for Drevo {
    fn create_node(&self, new_node: NewNode) -> Result<Node> {
        Drevo::create_node(self, new_node)
    }

    fn get_node(&self, id: u64) -> Result<Option<Node>> {
        Drevo::get_node(self, id)
    }

    fn update_node(&self, id: u64, patch: NodePatch) -> Result<Node> {
        Drevo::update_node(self, id, patch)
    }

    fn delete_node(&self, id: u64) -> Result<()> {
        Drevo::delete_node(self, id)
    }

    fn create_edge(&self, new_edge: NewEdge) -> Result<Edge> {
        Drevo::create_edge(self, new_edge)
    }

    fn get_edge(&self, id: u64) -> Result<Option<Edge>> {
        Drevo::get_edge(self, id)
    }

    fn update_edge(&self, id: u64, patch: EdgePatch) -> Result<Edge> {
        Drevo::update_edge(self, id, patch)
    }

    fn delete_edge(&self, id: u64) -> Result<()> {
        Drevo::delete_edge(self, id)
    }

    fn neighbor_ids(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<u64>> {
        Drevo::neighbor_ids(self, node_id, direction, kind)
    }

    fn neighbors(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<Node>> {
        Drevo::neighbors(self, node_id, direction, kind)
    }

    fn edges_of(&self, node_id: u64, direction: Direction) -> Result<Vec<Edge>> {
        Drevo::edges_of(self, node_id, direction)
    }

    fn all_nodes(&self) -> Result<Vec<Node>> {
        Drevo::collect_all_nodes(self)
    }

    fn all_edges(&self) -> Result<Vec<Edge>> {
        Drevo::collect_all_edges(self)
    }

    fn nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Node>> {
        Drevo::list_nodes_by_kind(self, kind, limit, offset)
    }

    fn export_dump(&self) -> Result<Dump> {
        self.build_dump()
    }

    fn apply_dump(&self, dump: Dump) -> Result<ImportReport> {
        self.apply_dump_records(dump)
    }
}
