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
}
