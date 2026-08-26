//! The `GraphEngine` seam — the graph-level abstraction the query layers
//! (Cypher executor, traversal, planner) depend on instead of reaching into a
//! concrete store's KV-encoded internals.
//!
//! `GraphEngine` expresses storage in **graph terms** — nodes, edges, adjacency
//! — so the in-memory-first native engine ([`crate::native::NativeGraph`]) and
//! the main crate's KV-backed store present the *same* contract to the query
//! layers above them. The trait is **object-safe** (`&dyn GraphEngine` works) so
//! call sites can be generic over the engine without monomorphising the whole
//! executor.
//!
//! The method set mirrors the core of a store's inherent API — node/edge CRUD
//! plus adjacency expansion, and the [`crate::engine::GraphEngine::export_dump`]
//! / [`crate::engine::GraphEngine::apply_dump`] migration seam. Secondary concerns
//! (FTS, vectors, transactions, statistics) are intentionally out of scope and
//! layered on above the seam.
//!
//! Every method returns [`crate::error::Result`] (i.e. [`crate::error::CoreError`]).
//! The main crate's KV store implements this trait by mapping its richer
//! `DrevoError` into `CoreError`; the native engine returns `CoreError` natively.

use crate::dump::{Dump, ImportReport};
use crate::error::Result;
use crate::model::{Direction, Edge, EdgePatch, NewEdge, NewNode, Node, NodePatch};

/// Graph-level storage and traversal, expressed in graph terms rather than a
/// byte-key KV vocabulary.
///
/// Implemented by both the KV-backed store (in the main `drevo` crate) and the
/// native [`crate::native::NativeGraph`], so today's store and the native engine
/// present the same contract to the query layers above them.
pub trait GraphEngine {
    /// Create a node, allocating its id/uuid/timestamps.
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store
    /// (e.g. a title-uniqueness violation).
    fn create_node(&self, new_node: NewNode) -> Result<Node>;

    /// Fetch a node by id, or `Ok(None)` if absent.
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store.
    fn get_node(&self, id: u64) -> Result<Option<Node>>;

    /// Apply a partial update to a node.
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store
    /// (e.g. the node not existing, or a title-uniqueness violation).
    fn update_node(&self, id: u64, patch: NodePatch) -> Result<Node>;

    /// Delete a node (and its incident edges/indexes).
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store.
    fn delete_node(&self, id: u64) -> Result<()>;

    /// Create an edge between two existing nodes.
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store.
    fn create_edge(&self, new_edge: NewEdge) -> Result<Edge>;

    /// Fetch an edge by id, or `Ok(None)` if absent.
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store.
    fn get_edge(&self, id: u64) -> Result<Option<Edge>>;

    /// Apply a partial update to an edge (kind / weight / properties).
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store
    /// (e.g. the edge not existing, or a non-finite weight).
    fn update_edge(&self, id: u64, patch: EdgePatch) -> Result<Edge>;

    /// Delete an edge (and its adjacency/index entries).
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store.
    fn delete_edge(&self, id: u64) -> Result<()>;

    /// Return the **distinct** node ids adjacent to `node_id` in `direction`,
    /// optionally restricted to edges of `kind` — the id-only fan-out that reads
    /// straight from the adjacency index.
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store.
    fn neighbor_ids(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<u64>>;

    /// Return the adjacent **nodes** (loaded once each) in `direction`,
    /// optionally restricted to edges of `kind`.
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store.
    fn neighbors(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<Node>>;

    /// Return the full [`Edge`] records incident to `node_id` in `direction`
    /// (the weighted/full-edge expansion, which loads each edge record — unlike
    /// [`neighbor_ids`](Self::neighbor_ids)).
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store.
    fn edges_of(&self, node_id: u64, direction: Direction) -> Result<Vec<Edge>>;

    /// Return **every** node in the store (a full scan — the label-less
    /// `MATCH (n)`).
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store.
    fn all_nodes(&self) -> Result<Vec<Node>>;

    /// Return **every** edge in the store (a full scan — the anonymous
    /// `MATCH ()-[r]->()`).
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store.
    fn all_edges(&self) -> Result<Vec<Edge>>;

    /// Return nodes of a given `kind` (label scan), paginated by `limit` /
    /// `offset`.
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store.
    fn nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Node>>;

    /// Export the entire graph as a `drevo-json-v1` [`Dump`] — every node and
    /// edge plus the id-allocation counters — the interchange format used for
    /// backup, JSON/GraphML round-trips, and cross-engine migration.
    ///
    /// This is the read half of the migration seam:
    /// `dst.apply_dump(src.export_dump()?)` moves a live graph between any two
    /// engines.
    ///
    /// # Errors
    /// Propagates any [`crate::error::CoreError`] from the underlying store.
    fn export_dump(&self) -> Result<Dump>;

    /// Bulk-load a [`Dump`] into this engine **verbatim**, preserving every
    /// node/edge id (so edges stay connected) and clamping the id counters above
    /// every imported id. Nodes/edges already present byte-for-byte are skipped
    /// (idempotent re-import); an id that collides with *different* content, or
    /// an edge whose endpoint is missing, is an error.
    ///
    /// This is the write half of the migration seam. The counts in the returned
    /// [`ImportReport`] separate freshly-inserted from skipped rows.
    ///
    /// # Errors
    /// * [`crate::error::CoreError::NodeNotFound`] — an edge references an
    ///   endpoint that neither exists nor is supplied by the dump.
    /// * Other [`crate::error::CoreError`] variants — an id collision against
    ///   different content, a title/uuid clash, or a backend failure.
    fn apply_dump(&self, dump: Dump) -> Result<ImportReport>;
}
