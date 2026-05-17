//! Core database struct and lifecycle methods.
//!
//! [`Drevo`] is the main entry point for all database operations.
//! It wraps a [`StorageBackend`] and manages auto-increment counters,
//! indexes, and the graph data model.

#[cfg(feature = "redb-backend")]
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{DrevoError, Result};
use crate::fts::index as fts_index;
use crate::fts::tokenizer::extract_trigrams;
use crate::model::{
    Direction, Edge, EdgePatch, NewEdge, NewNode, Node, NodePatch, ScoredNode, SubGraph,
};
#[cfg(feature = "redb-backend")]
use crate::storage::RedbBackend;
use crate::storage::{MemoryBackend, StorageBackend};

/// Meta key for the next node ID counter.
const META_NEXT_NODE_ID: &[u8] = b"meta:next_node_id";

/// Meta key for the next edge ID counter.
const META_NEXT_EDGE_ID: &[u8] = b"meta:next_edge_id";

/// Key prefix for node data: `node:{id}` -> bincode(Node).
const PREFIX_NODE: &[u8] = b"node:";

/// Key prefix for UUID-to-id index: `node_uuid:{uuid}` -> u64 (le bytes).
const PREFIX_NODE_UUID: &[u8] = b"node_uuid:";

/// Key prefix for title-to-id index: `node_title:{title}` -> u64 (le bytes).
const PREFIX_NODE_TITLE: &[u8] = b"node_title:";

/// Key prefix for edge data: `edge:{id}` -> bincode(Edge).
const PREFIX_EDGE: &[u8] = b"edge:";

/// Key prefix for edge UUID index: `edge_uuid:{uuid}` -> u64 (le bytes).
const PREFIX_EDGE_UUID: &[u8] = b"edge_uuid:";

/// Key prefix for outgoing adjacency: `out:{from_id}:{edge_id}` -> empty.
const PREFIX_OUT: &[u8] = b"out:";

/// Key prefix for incoming adjacency: `in:{to_id}:{edge_id}` -> empty.
const PREFIX_IN: &[u8] = b"in:";

/// Key prefix for node kind index: `node_kind:{kind}:{node_id}` -> empty.
const PREFIX_NODE_KIND: &[u8] = b"node_kind:";

/// Key prefix for edge kind index: `edge_kind:{kind}:{edge_id}` -> empty.
const PREFIX_EDGE_KIND: &[u8] = b"edge_kind:";

/// Key prefix for updated_at index: `updated:{inverted_ts_be}:{node_id_le}` -> empty.
/// Inverted timestamp (`i64::MAX - updated_at`) stored as big-endian so that
/// scanning in natural byte order yields newest nodes first.
const PREFIX_UPDATED: &[u8] = b"updated:";

/// Bincode configuration used for all serialization.
const BINCODE_CONFIG: bincode::config::Configuration = bincode::config::standard();

/// The main drevo handle.
///
/// Created via [`Drevo::open`] (disk-backed) or
/// [`Drevo::open_in_memory`] (ephemeral). All graph operations
/// are methods on this struct.
pub struct Drevo {
    /// The underlying key-value storage backend.
    backend: Box<dyn StorageBackend>,
    /// Auto-increment counter for node IDs.
    next_node_id: AtomicU64,
    /// Auto-increment counter for edge IDs.
    next_edge_id: AtomicU64,
}

impl Drevo {
    /// Open a disk-backed database at the given path.
    ///
    /// Creates the database file if it does not exist.
    /// Loads auto-increment counters from the stored metadata.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::Storage`] if the backend cannot be opened.
    ///
    /// # Availability
    ///
    /// This method requires the `redb-backend` feature and is not available
    /// on `wasm32` targets. Use [`open_in_memory`](Self::open_in_memory) instead.
    #[cfg(feature = "redb-backend")]
    pub fn open(path: &Path) -> Result<Self> {
        let backend = RedbBackend::open(path).map_err(DrevoError::Storage)?;
        let backend = Box::new(backend);
        let (next_node_id, next_edge_id) = Self::load_counters(&*backend)?;
        Ok(Self {
            backend,
            next_node_id: AtomicU64::new(next_node_id),
            next_edge_id: AtomicU64::new(next_edge_id),
        })
    }

    /// Open an ephemeral in-memory database.
    ///
    /// Data is lost when the database is dropped. Useful for tests
    /// and temporary workloads.
    pub fn open_in_memory() -> Result<Self> {
        let backend = Box::new(MemoryBackend::new());
        Ok(Self {
            backend,
            next_node_id: AtomicU64::new(1),
            next_edge_id: AtomicU64::new(1),
        })
    }

    /// Flush all pending writes and close the database.
    ///
    /// Persists auto-increment counters to storage before flushing.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::Storage`] if flush fails.
    pub fn close(self) -> Result<()> {
        self.persist_counters()?;
        self.backend.flush().map_err(DrevoError::Storage)?;
        Ok(())
    }

    /// Trigger compaction of the underlying storage.
    ///
    /// For redb this is a no-op (redb manages its own compaction).
    /// For the memory backend this flushes to disk if a path is configured.
    pub fn compact(&self) -> Result<()> {
        self.backend.flush().map_err(DrevoError::Storage)?;
        Ok(())
    }

    /// Cheap readiness probe used by the HTTP `/ready` endpoint.
    ///
    /// Exercises the storage backend with a tiny `get` against the
    /// meta-counter key so that probe traffic does not stay in the
    /// abstraction layer — if the underlying redb file is corrupted,
    /// missing, or its mutex is poisoned, the failure surfaces here
    /// instead of waiting for a real CRUD call. The probe deliberately
    /// avoids any write so it is safe to call from a read-only replica
    /// once Phase 13 lands.
    pub fn health_check(&self) -> Result<()> {
        self.backend
            .get(META_NEXT_NODE_ID)
            .map_err(DrevoError::Storage)?;
        Ok(())
    }

    /// Allocate the next node ID (thread-safe).
    ///
    /// Returns a unique, monotonically increasing ID starting from 1.
    /// Used internally by `create_node`.
    pub fn alloc_node_id(&self) -> u64 {
        self.next_node_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Allocate the next edge ID (thread-safe).
    ///
    /// Returns a unique, monotonically increasing ID starting from 1.
    /// Used internally by `create_edge`.
    pub fn alloc_edge_id(&self) -> u64 {
        self.next_edge_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Return a reference to the underlying storage backend.
    #[allow(dead_code)] // Reserved for future use (e.g. traversal, search)
    pub(crate) fn backend(&self) -> &dyn StorageBackend {
        &*self.backend
    }

    // ---------------------------------------------------------------
    // Node CRUD
    // ---------------------------------------------------------------

    /// Create a new node in the database.
    ///
    /// Allocates a unique ID, generates a UUID v7 and timestamps,
    /// stores the node, and updates the title and UUID indexes.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::DuplicateTitle`] if a node with the
    /// same title already exists.
    pub fn create_node(&self, new_node: NewNode) -> Result<Node> {
        // Check title uniqueness
        let title_key = node_title_key(&new_node.title);
        if self
            .backend
            .get(&title_key)
            .map_err(DrevoError::Storage)?
            .is_some()
        {
            return Err(DrevoError::DuplicateTitle(new_node.title));
        }

        let id = self.alloc_node_id();
        let node = new_node.into_node(id);

        // Store node data
        let data = serialize_node(&node)?;
        self.backend
            .put(&node_key(id), &data)
            .map_err(DrevoError::Storage)?;

        // UUID index
        self.backend
            .put(&node_uuid_key(&node.uuid), &id.to_le_bytes())
            .map_err(DrevoError::Storage)?;

        // Title index
        self.backend
            .put(&title_key, &id.to_le_bytes())
            .map_err(DrevoError::Storage)?;

        // Kind index
        self.backend
            .put(&node_kind_key(&node.kind, id), &[])
            .map_err(DrevoError::Storage)?;

        // FTS index
        fts_index::index_node(&*self.backend, id, &node.title, &node.body)?;

        // Updated-at index (newest-first ordering)
        self.backend
            .put(&updated_key(node.updated_at, id), &[])
            .map_err(DrevoError::Storage)?;

        Ok(node)
    }

    /// Retrieve a node by its auto-increment ID.
    ///
    /// Returns `None` if the node does not exist.
    pub fn get_node(&self, id: u64) -> Result<Option<Node>> {
        match self
            .backend
            .get(&node_key(id))
            .map_err(DrevoError::Storage)?
        {
            Some(bytes) => Ok(Some(deserialize_node(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Retrieve a node by its UUID v7.
    ///
    /// Returns `None` if no node has the given UUID.
    pub fn get_node_by_uuid(&self, uuid: &[u8; 16]) -> Result<Option<Node>> {
        match self
            .backend
            .get(&node_uuid_key(uuid))
            .map_err(DrevoError::Storage)?
        {
            Some(id_bytes) => {
                let id = u64_from_bytes(&id_bytes);
                self.get_node(id)
            }
            None => Ok(None),
        }
    }

    /// Retrieve a node by its title (exact match).
    ///
    /// Returns `None` if no node has the given title.
    pub fn get_node_by_title(&self, title: &str) -> Result<Option<Node>> {
        match self
            .backend
            .get(&node_title_key(title))
            .map_err(DrevoError::Storage)?
        {
            Some(id_bytes) => {
                let id = u64_from_bytes(&id_bytes);
                self.get_node(id)
            }
            None => Ok(None),
        }
    }

    /// Update an existing node with a partial patch.
    ///
    /// Only `Some` fields in the patch are applied. The `updated_at`
    /// timestamp is always refreshed.
    ///
    /// # Errors
    ///
    /// - [`DrevoError::NodeNotFound`] if the node does not exist.
    /// - [`DrevoError::DuplicateTitle`] if the new title collides
    ///   with another node.
    pub fn update_node(&self, id: u64, patch: NodePatch) -> Result<Node> {
        let mut node = self.get_node(id)?.ok_or(DrevoError::NodeNotFound(id))?;

        let old_title = node.title.clone();
        let old_body = node.body.clone();
        let old_kind = node.kind.clone();
        let old_updated_at = node.updated_at;

        // Check title uniqueness before applying patch
        if let Some(ref new_title) = patch.title {
            if *new_title != old_title {
                let title_key = node_title_key(new_title);
                if self
                    .backend
                    .get(&title_key)
                    .map_err(DrevoError::Storage)?
                    .is_some()
                {
                    return Err(DrevoError::DuplicateTitle(new_title.clone()));
                }
            }
        }

        node.apply_patch(patch);

        // Store updated node
        let data = serialize_node(&node)?;
        self.backend
            .put(&node_key(id), &data)
            .map_err(DrevoError::Storage)?;

        // Update title index if title changed
        if node.title != old_title {
            self.backend
                .delete(&node_title_key(&old_title))
                .map_err(DrevoError::Storage)?;
            self.backend
                .put(&node_title_key(&node.title), &id.to_le_bytes())
                .map_err(DrevoError::Storage)?;
        }

        // Update kind index if kind changed
        if node.kind != old_kind {
            self.backend
                .delete(&node_kind_key(&old_kind, id))
                .map_err(DrevoError::Storage)?;
            self.backend
                .put(&node_kind_key(&node.kind, id), &[])
                .map_err(DrevoError::Storage)?;
        }

        // Update FTS index if title or body changed
        if node.title != old_title || node.body != old_body {
            fts_index::deindex_node(&*self.backend, id, &old_title, &old_body)?;
            fts_index::index_node(&*self.backend, id, &node.title, &node.body)?;
        }

        // Update updated-at index: remove old entry, add new one
        self.backend
            .delete(&updated_key(old_updated_at, id))
            .map_err(DrevoError::Storage)?;
        self.backend
            .put(&updated_key(node.updated_at, id), &[])
            .map_err(DrevoError::Storage)?;

        Ok(node)
    }

    /// Delete a node by ID.
    ///
    /// Removes the node data and all associated index entries
    /// (UUID index, title index).
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::NodeNotFound`] if the node does not exist.
    pub fn delete_node(&self, id: u64) -> Result<()> {
        let node = self.get_node(id)?.ok_or(DrevoError::NodeNotFound(id))?;

        // Cascade-delete all edges connected to this node (both directions).
        // Using Direction::Both deduplicates self-loop edges automatically.
        let connected_edges = self.edges_of(id, Direction::Both)?;
        for edge in &connected_edges {
            self.delete_edge(edge.id)?;
        }

        // Remove node data
        self.backend
            .delete(&node_key(id))
            .map_err(DrevoError::Storage)?;

        // Remove UUID index
        self.backend
            .delete(&node_uuid_key(&node.uuid))
            .map_err(DrevoError::Storage)?;

        // Remove title index
        self.backend
            .delete(&node_title_key(&node.title))
            .map_err(DrevoError::Storage)?;

        // Remove kind index
        self.backend
            .delete(&node_kind_key(&node.kind, id))
            .map_err(DrevoError::Storage)?;

        // Remove FTS index
        fts_index::deindex_node(&*self.backend, id, &node.title, &node.body)?;

        // Remove updated-at index
        self.backend
            .delete(&updated_key(node.updated_at, id))
            .map_err(DrevoError::Storage)?;

        Ok(())
    }

    // ---------------------------------------------------------------
    // Edge CRUD
    // ---------------------------------------------------------------

    /// Create a new edge between two existing nodes.
    ///
    /// Allocates a unique ID, generates a UUID v7 and timestamp,
    /// stores the edge, updates UUID index and adjacency lists.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::NodeNotFound`] if either `from_id` or
    /// `to_id` does not refer to an existing node.
    pub fn create_edge(&self, new_edge: NewEdge) -> Result<Edge> {
        // Validate that both endpoints exist
        if self.get_node(new_edge.from_id)?.is_none() {
            return Err(DrevoError::NodeNotFound(new_edge.from_id));
        }
        if self.get_node(new_edge.to_id)?.is_none() {
            return Err(DrevoError::NodeNotFound(new_edge.to_id));
        }

        let id = self.alloc_edge_id();
        let edge = new_edge.into_edge(id);

        // Store edge data
        let data = serialize_edge(&edge)?;
        self.backend
            .put(&edge_key(id), &data)
            .map_err(DrevoError::Storage)?;

        // UUID index
        self.backend
            .put(&edge_uuid_key(&edge.uuid), &id.to_le_bytes())
            .map_err(DrevoError::Storage)?;

        // Outgoing adjacency: out:{from_id}:{edge_id}
        self.backend
            .put(&out_edge_key(edge.from_id, id), &[])
            .map_err(DrevoError::Storage)?;

        // Incoming adjacency: in:{to_id}:{edge_id}
        self.backend
            .put(&in_edge_key(edge.to_id, id), &[])
            .map_err(DrevoError::Storage)?;

        // Edge kind index
        self.backend
            .put(&edge_kind_key(&edge.kind, id), &[])
            .map_err(DrevoError::Storage)?;

        Ok(edge)
    }

    /// Retrieve an edge by its auto-increment ID.
    ///
    /// Returns `None` if the edge does not exist.
    pub fn get_edge(&self, id: u64) -> Result<Option<Edge>> {
        match self
            .backend
            .get(&edge_key(id))
            .map_err(DrevoError::Storage)?
        {
            Some(bytes) => Ok(Some(deserialize_edge(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Retrieve an edge by its UUID v7.
    ///
    /// Returns `None` if no edge has the given UUID.
    pub fn get_edge_by_uuid(&self, uuid: &[u8; 16]) -> Result<Option<Edge>> {
        match self
            .backend
            .get(&edge_uuid_key(uuid))
            .map_err(DrevoError::Storage)?
        {
            Some(id_bytes) => {
                let id = u64_from_bytes(&id_bytes);
                self.get_edge(id)
            }
            None => Ok(None),
        }
    }

    /// Update an existing edge with a partial patch.
    ///
    /// Only `Some` fields in the patch are applied (kind, weight, properties).
    /// The edge endpoints (`from_id`, `to_id`) cannot be changed.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::EdgeNotFound`] if the edge does not exist.
    pub fn update_edge(&self, id: u64, patch: EdgePatch) -> Result<Edge> {
        let mut edge = self.get_edge(id)?.ok_or(DrevoError::EdgeNotFound(id))?;

        let old_kind = edge.kind.clone();

        edge.apply_patch(patch);

        let data = serialize_edge(&edge)?;
        self.backend
            .put(&edge_key(id), &data)
            .map_err(DrevoError::Storage)?;

        // Update edge_kind index if kind changed
        if edge.kind != old_kind {
            self.backend
                .delete(&edge_kind_key(&old_kind, id))
                .map_err(DrevoError::Storage)?;
            self.backend
                .put(&edge_kind_key(&edge.kind, id), &[])
                .map_err(DrevoError::Storage)?;
        }

        Ok(edge)
    }

    /// Delete an edge by ID.
    ///
    /// Removes the edge data, UUID index, and adjacency list entries.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::EdgeNotFound`] if the edge does not exist.
    pub fn delete_edge(&self, id: u64) -> Result<()> {
        let edge = self.get_edge(id)?.ok_or(DrevoError::EdgeNotFound(id))?;

        // Remove edge data
        self.backend
            .delete(&edge_key(id))
            .map_err(DrevoError::Storage)?;

        // Remove UUID index
        self.backend
            .delete(&edge_uuid_key(&edge.uuid))
            .map_err(DrevoError::Storage)?;

        // Remove outgoing adjacency entry
        self.backend
            .delete(&out_edge_key(edge.from_id, id))
            .map_err(DrevoError::Storage)?;

        // Remove incoming adjacency entry
        self.backend
            .delete(&in_edge_key(edge.to_id, id))
            .map_err(DrevoError::Storage)?;

        // Remove edge kind index
        self.backend
            .delete(&edge_kind_key(&edge.kind, id))
            .map_err(DrevoError::Storage)?;

        Ok(())
    }

    /// Retrieve all edges connected to a node in the given direction.
    ///
    /// - `Outgoing`: edges where `from_id == node_id`
    /// - `Incoming`: edges where `to_id == node_id`
    /// - `Both`: union of outgoing and incoming (deduplicated for self-loops)
    pub fn edges_of(&self, node_id: u64, direction: Direction) -> Result<Vec<Edge>> {
        match direction {
            Direction::Outgoing => self.outgoing_edges(node_id),
            Direction::Incoming => self.incoming_edges(node_id),
            Direction::Both => {
                let mut edges = self.outgoing_edges(node_id)?;
                let incoming = self.incoming_edges(node_id)?;
                // Deduplicate self-loop edges that appear in both lists
                for edge in incoming {
                    if !edges.iter().any(|e| e.id == edge.id) {
                        edges.push(edge);
                    }
                }
                Ok(edges)
            }
        }
    }

    // ---------------------------------------------------------------
    // Index queries
    // ---------------------------------------------------------------

    /// List all nodes with the given kind, with pagination.
    ///
    /// Scans the `node_kind:{kind}:` prefix to find matching node IDs,
    /// then retrieves each node. Results are ordered by node ID (insertion order).
    ///
    /// # Arguments
    ///
    /// * `kind` — the node kind to filter by (e.g. "note", "task")
    /// * `limit` — maximum number of nodes to return
    /// * `offset` — number of matching nodes to skip
    pub fn list_nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Node>> {
        let prefix = node_kind_prefix(kind);
        let entries = self
            .backend
            .scan_prefix(&prefix)
            .map_err(DrevoError::Storage)?;

        let mut nodes = Vec::new();
        for (key, _) in entries.into_iter().skip(offset).take(limit) {
            let id = id_from_kind_key(&key, &prefix);
            if let Some(node) = self.get_node(id)? {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    /// List all edges with the given kind, with pagination.
    ///
    /// Scans the `edge_kind:{kind}:` prefix to find matching edge IDs,
    /// then retrieves each edge. Results are ordered by edge ID (insertion order).
    ///
    /// # Arguments
    ///
    /// * `kind` — the edge kind to filter by (e.g. "links_to", "tagged_with")
    /// * `limit` — maximum number of edges to return
    /// * `offset` — number of matching edges to skip
    pub fn list_edges_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Edge>> {
        let prefix = edge_kind_prefix(kind);
        let entries = self
            .backend
            .scan_prefix(&prefix)
            .map_err(DrevoError::Storage)?;

        let mut edges = Vec::new();
        for (key, _) in entries.into_iter().skip(offset).take(limit) {
            let id = id_from_kind_key(&key, &prefix);
            if let Some(edge) = self.get_edge(id)? {
                edges.push(edge);
            }
        }
        Ok(edges)
    }

    /// List the most recently updated nodes.
    ///
    /// Scans the `updated:` index which is sorted by descending `updated_at`
    /// timestamp (newest first). Returns at most `limit` nodes.
    pub fn list_recent(&self, limit: usize) -> Result<Vec<Node>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let entries = self
            .backend
            .scan_prefix(PREFIX_UPDATED)
            .map_err(DrevoError::Storage)?;

        let mut nodes = Vec::new();
        for (key, _) in entries.into_iter().take(limit) {
            let id = node_id_from_updated_key(&key);
            if let Some(node) = self.get_node(id)? {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    // ---------------------------------------------------------------
    // FTS index queries
    // ---------------------------------------------------------------

    /// Retrieve all node IDs from the posting list of a single trigram.
    ///
    /// Returns an empty list if no nodes match. Useful for inspecting
    /// the FTS index directly in tests.
    pub fn fts_node_ids_for_trigram(&self, trigram: &str) -> Result<Vec<u64>> {
        fts_index::node_ids_for_trigram(&*self.backend, trigram)
    }

    /// Intersect posting lists for multiple trigrams.
    ///
    /// Returns node IDs that appear in ALL posting lists (AND semantics).
    /// Returns empty if trigrams is empty or no nodes match all trigrams.
    pub fn fts_intersect_trigrams(&self, trigrams: &[String]) -> Result<Vec<u64>> {
        fts_index::intersect_trigrams(&*self.backend, trigrams)
    }

    /// Full-text search with TF-IDF ranking.
    ///
    /// Extracts trigrams from the query, finds candidate nodes via posting
    /// list intersection, scores each candidate using TF-IDF, and returns
    /// up to `limit` results sorted by descending score.
    ///
    /// **TF-IDF formula:**
    /// - TF (term frequency): number of query trigrams present in the
    ///   node's trigram set, divided by the total number of node trigrams.
    /// - IDF (inverse document frequency): `ln(N / df)` where `N` is the
    ///   total number of nodes and `df` is the number of nodes containing
    ///   the trigram.
    /// - Score = sum of `tf * idf` for each query trigram.
    ///
    /// Returns an empty list if the query produces no trigrams or no
    /// nodes match.
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<ScoredNode>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let query_trigrams = extract_trigrams(query, "");
        if query_trigrams.is_empty() {
            return Ok(Vec::new());
        }

        // Find candidate nodes (intersection of all query trigram posting lists)
        let candidate_ids = fts_index::intersect_trigrams(&*self.backend, &query_trigrams)?;
        if candidate_ids.is_empty() {
            return Ok(Vec::new());
        }

        // Total number of indexed nodes (approximate: count node: prefix entries)
        let all_nodes = self
            .backend
            .scan_prefix(PREFIX_NODE)
            .map_err(DrevoError::Storage)?;
        let total_nodes = all_nodes.len() as f32;

        // Precompute IDF for each query trigram
        let mut idf_values: Vec<f32> = Vec::with_capacity(query_trigrams.len());
        for trigram in &query_trigrams {
            let df = fts_index::posting_list_len(&*self.backend, trigram)? as f32;
            // Smoothed IDF: ln(1 + N / df) — avoids zero when df == N
            let idf = if df > 0.0 {
                (1.0 + total_nodes / df).ln()
            } else {
                0.0
            };
            idf_values.push(idf);
        }

        // Score each candidate
        let mut scored: Vec<ScoredNode> = Vec::with_capacity(candidate_ids.len());
        for node_id in &candidate_ids {
            let node = match self.get_node(*node_id)? {
                Some(n) => n,
                None => continue,
            };

            // Extract the node's own trigrams to compute TF
            let node_trigrams = extract_trigrams(&node.title, &node.body);
            let node_trigram_count = node_trigrams.len() as f32;
            if node_trigram_count == 0.0 {
                continue;
            }

            let mut score: f32 = 0.0;
            for (i, qt) in query_trigrams.iter().enumerate() {
                // TF: count how many times this query trigram appears in node trigrams
                // Since trigrams are deduplicated, tf is 0 or 1
                let tf = if node_trigrams.iter().any(|nt| nt == qt) {
                    1.0 / node_trigram_count
                } else {
                    0.0
                };
                score += tf * idf_values[i];
            }

            if score > 0.0 {
                scored.push(ScoredNode { node, score });
            }
        }

        // Sort by score descending, then by node id ascending for stability
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.node.id.cmp(&b.node.id))
        });

        scored.truncate(limit);
        Ok(scored)
    }

    // ---------------------------------------------------------------
    // Graph Traversal
    // ---------------------------------------------------------------

    /// Breadth-first search from a start node with depth limit.
    ///
    /// Returns all nodes reachable within `max_depth` hops. The start
    /// node is **not** included in the result. Edges can be filtered
    /// by kind.
    ///
    /// # Arguments
    ///
    /// * `start_id` — the node ID to start from
    /// * `max_depth` — maximum number of hops (0 returns empty)
    /// * `direction` — which edges to follow
    /// * `edge_kind` — if `Some`, only follow edges with this kind
    pub fn bfs(
        &self,
        start_id: u64,
        max_depth: u8,
        direction: Direction,
        edge_kind: Option<&str>,
    ) -> Result<Vec<Node>> {
        crate::traversal::bfs(
            start_id,
            max_depth,
            direction,
            edge_kind,
            &|id| self.get_node(id),
            &|id, dir| self.edges_of(id, dir),
        )
    }

    /// Depth-first search from a start node with depth limit.
    ///
    /// Returns all nodes reachable within `max_depth` hops. The start
    /// node is **not** included in the result. Edges can be filtered
    /// by kind. Nodes are returned in DFS visit order.
    ///
    /// # Arguments
    ///
    /// * `start_id` — the node ID to start from
    /// * `max_depth` — maximum number of hops (0 returns empty)
    /// * `direction` — which edges to follow
    /// * `edge_kind` — if `Some`, only follow edges with this kind
    pub fn dfs(
        &self,
        start_id: u64,
        max_depth: u8,
        direction: Direction,
        edge_kind: Option<&str>,
    ) -> Result<Vec<Node>> {
        crate::traversal::dfs(
            start_id,
            max_depth,
            direction,
            edge_kind,
            &|id| self.get_node(id),
            &|id, dir| self.edges_of(id, dir),
        )
    }

    /// Find the shortest (lowest total weight) path between two nodes
    /// using Dijkstra's algorithm. Follows **outgoing** edges only.
    ///
    /// Returns `Some(vec![from, ..., to])` with the node IDs along the
    /// shortest path, or `None` if `to` is unreachable from `from`.
    /// If `from == to`, returns `Some(vec![from])`.
    pub fn shortest_path(&self, from: u64, to: u64) -> Result<Option<Vec<u64>>> {
        crate::traversal::shortest_path(from, to, &|id| self.get_node(id), &|id, dir| {
            self.edges_of(id, dir)
        })
    }

    /// Extract a subgraph of all nodes and edges within `depth` hops
    /// of the root node. Follows edges in **both** directions.
    ///
    /// The root node is included in the result. All edges whose both
    /// endpoints are within the discovered node set are returned.
    ///
    /// Returns `Err(NodeNotFound)` if the root node does not exist.
    pub fn subgraph(&self, root: u64, depth: u8) -> Result<SubGraph> {
        crate::traversal::subgraph(root, depth, &|id| self.get_node(id), &|id, dir| {
            self.edges_of(id, dir)
        })
    }

    /// Return immediate neighbors of a node (BFS depth=1).
    ///
    /// Convenience wrapper over [`bfs`] with `max_depth=1`.
    ///
    /// # Arguments
    ///
    /// * `node_id` — the node to query
    /// * `direction` — which edges to follow
    /// * `kind` — if `Some`, only follow edges with this kind
    pub fn neighbors(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<Node>> {
        self.bfs(node_id, 1, direction, kind)
    }

    // ---------------------------------------------------------------
    // Internal helpers
    // ---------------------------------------------------------------

    /// Collect outgoing edges for a node by scanning the `out:` prefix.
    fn outgoing_edges(&self, node_id: u64) -> Result<Vec<Edge>> {
        let prefix = out_prefix(node_id);
        let entries = self
            .backend
            .scan_prefix(&prefix)
            .map_err(DrevoError::Storage)?;
        let mut edges = Vec::with_capacity(entries.len());
        for (key, _) in entries {
            let edge_id = edge_id_from_adjacency_key(&key, &prefix);
            if let Some(edge) = self.get_edge(edge_id)? {
                edges.push(edge);
            }
        }
        Ok(edges)
    }

    /// Collect incoming edges for a node by scanning the `in:` prefix.
    fn incoming_edges(&self, node_id: u64) -> Result<Vec<Edge>> {
        let prefix = in_prefix(node_id);
        let entries = self
            .backend
            .scan_prefix(&prefix)
            .map_err(DrevoError::Storage)?;
        let mut edges = Vec::with_capacity(entries.len());
        for (key, _) in entries {
            let edge_id = edge_id_from_adjacency_key(&key, &prefix);
            if let Some(edge) = self.get_edge(edge_id)? {
                edges.push(edge);
            }
        }
        Ok(edges)
    }

    /// Load auto-increment counters from storage metadata.
    ///
    /// Returns (next_node_id, next_edge_id). Defaults to 1 if not found.
    #[cfg(feature = "redb-backend")]
    fn load_counters(backend: &dyn StorageBackend) -> Result<(u64, u64)> {
        let node_id = match backend
            .get(META_NEXT_NODE_ID)
            .map_err(DrevoError::Storage)?
        {
            Some(bytes) => u64_from_bytes(&bytes),
            None => 1,
        };
        let edge_id = match backend
            .get(META_NEXT_EDGE_ID)
            .map_err(DrevoError::Storage)?
        {
            Some(bytes) => u64_from_bytes(&bytes),
            None => 1,
        };
        Ok((node_id, edge_id))
    }

    /// Persist current auto-increment counters to storage metadata.
    fn persist_counters(&self) -> Result<()> {
        let node_id = self.next_node_id.load(Ordering::Relaxed);
        let edge_id = self.next_edge_id.load(Ordering::Relaxed);
        self.backend
            .put(META_NEXT_NODE_ID, &node_id.to_le_bytes())
            .map_err(DrevoError::Storage)?;
        self.backend
            .put(META_NEXT_EDGE_ID, &edge_id.to_le_bytes())
            .map_err(DrevoError::Storage)?;
        Ok(())
    }
}

/// Decode a u64 from little-endian bytes, defaulting to 1 on invalid input.
fn u64_from_bytes(bytes: &[u8]) -> u64 {
    if bytes.len() == 8 {
        u64::from_le_bytes(bytes.try_into().unwrap())
    } else {
        1
    }
}

/// Build the storage key for a node: `node:{id}`.
fn node_key(id: u64) -> Vec<u8> {
    let mut key = PREFIX_NODE.to_vec();
    key.extend_from_slice(&id.to_le_bytes());
    key
}

/// Build the UUID index key: `node_uuid:{uuid}`.
fn node_uuid_key(uuid: &[u8; 16]) -> Vec<u8> {
    let mut key = PREFIX_NODE_UUID.to_vec();
    key.extend_from_slice(uuid);
    key
}

/// Build the title index key: `node_title:{title}`.
fn node_title_key(title: &str) -> Vec<u8> {
    let mut key = PREFIX_NODE_TITLE.to_vec();
    key.extend_from_slice(title.as_bytes());
    key
}

/// Build the storage key for an edge: `edge:{id}`.
fn edge_key(id: u64) -> Vec<u8> {
    let mut key = PREFIX_EDGE.to_vec();
    key.extend_from_slice(&id.to_le_bytes());
    key
}

/// Build the UUID index key for an edge: `edge_uuid:{uuid}`.
fn edge_uuid_key(uuid: &[u8; 16]) -> Vec<u8> {
    let mut key = PREFIX_EDGE_UUID.to_vec();
    key.extend_from_slice(uuid);
    key
}

/// Build an outgoing adjacency key: `out:{from_id}:{edge_id}`.
fn out_edge_key(from_id: u64, edge_id: u64) -> Vec<u8> {
    let mut key = PREFIX_OUT.to_vec();
    key.extend_from_slice(&from_id.to_le_bytes());
    key.push(b':');
    key.extend_from_slice(&edge_id.to_le_bytes());
    key
}

/// Build an incoming adjacency key: `in:{to_id}:{edge_id}`.
fn in_edge_key(to_id: u64, edge_id: u64) -> Vec<u8> {
    let mut key = PREFIX_IN.to_vec();
    key.extend_from_slice(&to_id.to_le_bytes());
    key.push(b':');
    key.extend_from_slice(&edge_id.to_le_bytes());
    key
}

/// Build the scan prefix for outgoing edges of a node: `out:{node_id}:`.
fn out_prefix(node_id: u64) -> Vec<u8> {
    let mut key = PREFIX_OUT.to_vec();
    key.extend_from_slice(&node_id.to_le_bytes());
    key.push(b':');
    key
}

/// Build the scan prefix for incoming edges of a node: `in:{node_id}:`.
fn in_prefix(node_id: u64) -> Vec<u8> {
    let mut key = PREFIX_IN.to_vec();
    key.extend_from_slice(&node_id.to_le_bytes());
    key.push(b':');
    key
}

/// Extract the edge ID from an adjacency key by stripping the prefix.
fn edge_id_from_adjacency_key(key: &[u8], prefix: &[u8]) -> u64 {
    let suffix = &key[prefix.len()..];
    if suffix.len() == 8 {
        u64::from_le_bytes(suffix.try_into().unwrap())
    } else {
        0
    }
}

/// Build a node kind index key: `node_kind:{kind}:{node_id}`.
fn node_kind_key(kind: &str, node_id: u64) -> Vec<u8> {
    let mut key = PREFIX_NODE_KIND.to_vec();
    key.extend_from_slice(kind.as_bytes());
    key.push(b':');
    key.extend_from_slice(&node_id.to_le_bytes());
    key
}

/// Build the scan prefix for a node kind: `node_kind:{kind}:`.
fn node_kind_prefix(kind: &str) -> Vec<u8> {
    let mut key = PREFIX_NODE_KIND.to_vec();
    key.extend_from_slice(kind.as_bytes());
    key.push(b':');
    key
}

/// Build an edge kind index key: `edge_kind:{kind}:{edge_id}`.
fn edge_kind_key(kind: &str, edge_id: u64) -> Vec<u8> {
    let mut key = PREFIX_EDGE_KIND.to_vec();
    key.extend_from_slice(kind.as_bytes());
    key.push(b':');
    key.extend_from_slice(&edge_id.to_le_bytes());
    key
}

/// Build the scan prefix for an edge kind: `edge_kind:{kind}:`.
fn edge_kind_prefix(kind: &str) -> Vec<u8> {
    let mut key = PREFIX_EDGE_KIND.to_vec();
    key.extend_from_slice(kind.as_bytes());
    key.push(b':');
    key
}

/// Build an updated_at index key: `updated:{inverted_ts_be}:{node_id_le}`.
///
/// The timestamp is inverted (`i64::MAX - ts`) and stored as big-endian
/// so that scanning the `updated:` prefix returns the most recently
/// updated nodes first.
fn updated_key(updated_at: i64, node_id: u64) -> Vec<u8> {
    let inverted = i64::MAX - updated_at;
    let mut key = PREFIX_UPDATED.to_vec();
    key.extend_from_slice(&inverted.to_be_bytes());
    key.push(b':');
    key.extend_from_slice(&node_id.to_le_bytes());
    key
}

/// Extract the node ID from an updated_at index key.
fn node_id_from_updated_key(key: &[u8]) -> u64 {
    // Format: PREFIX_UPDATED (8) + inverted_ts (8) + ':' (1) + node_id (8)
    let offset = PREFIX_UPDATED.len() + 8 + 1;
    let suffix = &key[offset..];
    if suffix.len() == 8 {
        u64::from_le_bytes(suffix.try_into().unwrap())
    } else {
        0
    }
}

/// Extract the ID (u64) from a kind index key by stripping the prefix.
fn id_from_kind_key(key: &[u8], prefix: &[u8]) -> u64 {
    let suffix = &key[prefix.len()..];
    if suffix.len() == 8 {
        u64::from_le_bytes(suffix.try_into().unwrap())
    } else {
        0
    }
}

/// Serialize an edge to bincode bytes.
fn serialize_edge(edge: &Edge) -> Result<Vec<u8>> {
    bincode::serde::encode_to_vec(edge, BINCODE_CONFIG)
        .map_err(|e| DrevoError::Serialization(e.to_string()))
}

/// Deserialize an edge from bincode bytes.
fn deserialize_edge(bytes: &[u8]) -> Result<Edge> {
    let (edge, _) = bincode::serde::decode_from_slice(bytes, BINCODE_CONFIG)
        .map_err(|e| DrevoError::Serialization(e.to_string()))?;
    Ok(edge)
}

/// Serialize a node to bincode bytes.
fn serialize_node(node: &Node) -> Result<Vec<u8>> {
    bincode::serde::encode_to_vec(node, BINCODE_CONFIG)
        .map_err(|e| DrevoError::Serialization(e.to_string()))
}

/// Deserialize a node from bincode bytes.
fn deserialize_node(bytes: &[u8]) -> Result<Node> {
    let (node, _) = bincode::serde::decode_from_slice(bytes, BINCODE_CONFIG)
        .map_err(|e| DrevoError::Serialization(e.to_string()))?;
    Ok(node)
}

impl std::fmt::Debug for Drevo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Drevo")
            .field("next_node_id", &self.next_node_id.load(Ordering::Relaxed))
            .field("next_edge_id", &self.next_edge_id.load(Ordering::Relaxed))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- open_in_memory ---

    #[test]
    fn open_in_memory_creates_db() {
        let db = Drevo::open_in_memory().unwrap();
        assert_eq!(db.next_node_id.load(Ordering::Relaxed), 1);
        assert_eq!(db.next_edge_id.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn open_in_memory_alloc_node_ids_are_sequential() {
        let db = Drevo::open_in_memory().unwrap();
        assert_eq!(db.alloc_node_id(), 1);
        assert_eq!(db.alloc_node_id(), 2);
        assert_eq!(db.alloc_node_id(), 3);
    }

    #[test]
    fn open_in_memory_alloc_edge_ids_are_sequential() {
        let db = Drevo::open_in_memory().unwrap();
        assert_eq!(db.alloc_edge_id(), 1);
        assert_eq!(db.alloc_edge_id(), 2);
        assert_eq!(db.alloc_edge_id(), 3);
    }

    #[test]
    fn open_in_memory_node_and_edge_ids_are_independent() {
        let db = Drevo::open_in_memory().unwrap();
        assert_eq!(db.alloc_node_id(), 1);
        assert_eq!(db.alloc_edge_id(), 1);
        assert_eq!(db.alloc_node_id(), 2);
        assert_eq!(db.alloc_edge_id(), 2);
    }

    #[test]
    fn open_in_memory_close_succeeds() {
        let db = Drevo::open_in_memory().unwrap();
        db.close().unwrap();
    }

    #[test]
    fn open_in_memory_compact_succeeds() {
        let db = Drevo::open_in_memory().unwrap();
        db.compact().unwrap();
    }

    // --- open (disk-backed) ---

    #[test]
    fn open_creates_new_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Drevo::open(&path).unwrap();
        assert_eq!(db.next_node_id.load(Ordering::Relaxed), 1);
        assert_eq!(db.next_edge_id.load(Ordering::Relaxed), 1);
        db.close().unwrap();
    }

    #[test]
    fn open_persists_counters_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");

        // Open, allocate some IDs, close
        {
            let db = Drevo::open(&path).unwrap();
            assert_eq!(db.alloc_node_id(), 1);
            assert_eq!(db.alloc_node_id(), 2);
            assert_eq!(db.alloc_node_id(), 3);
            assert_eq!(db.alloc_edge_id(), 1);
            assert_eq!(db.alloc_edge_id(), 2);
            db.close().unwrap();
        }

        // Reopen and verify counters continue
        {
            let db = Drevo::open(&path).unwrap();
            assert_eq!(db.next_node_id.load(Ordering::Relaxed), 4);
            assert_eq!(db.next_edge_id.load(Ordering::Relaxed), 3);
            assert_eq!(db.alloc_node_id(), 4);
            assert_eq!(db.alloc_edge_id(), 3);
            db.close().unwrap();
        }
    }

    #[test]
    fn open_without_close_loses_counter_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");

        // Open and allocate without closing properly
        {
            let db = Drevo::open(&path).unwrap();
            let _ = db.alloc_node_id();
            let _ = db.alloc_node_id();
            // Drop without close — counters not persisted
        }

        // Reopen — counters should be back at 1
        {
            let db = Drevo::open(&path).unwrap();
            assert_eq!(db.next_node_id.load(Ordering::Relaxed), 1);
            db.close().unwrap();
        }
    }

    #[test]
    fn compact_persists_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Drevo::open(&path).unwrap();
        db.compact().unwrap();
        db.close().unwrap();
    }

    // --- health_check (task 00048) ---

    #[test]
    fn health_check_succeeds_on_empty_in_memory_db() {
        let db = Drevo::open_in_memory().unwrap();
        db.health_check()
            .expect("health_check on fresh DB must succeed");
    }

    #[test]
    fn health_check_succeeds_after_crud_activity() {
        let db = Drevo::open_in_memory().unwrap();
        let _node = db
            .create_node(NewNode {
                kind: "note".into(),
                title: "hc".into(),
                body: String::new(),
                body_html: String::new(),
                properties: Default::default(),
            })
            .unwrap();
        db.health_check()
            .expect("health_check after node creation must succeed");
    }

    #[test]
    fn health_check_succeeds_on_redb_backend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hc.db");
        let db = Drevo::open(&path).unwrap();
        db.health_check()
            .expect("health_check on redb-backed DB must succeed");
        db.close().unwrap();
    }

    // --- Debug ---

    #[test]
    fn debug_format_works() {
        let db = Drevo::open_in_memory().unwrap();
        let debug = format!("{:?}", db);
        assert!(debug.contains("Drevo"));
        assert!(debug.contains("next_node_id"));
    }

    // --- u64_from_bytes ---

    #[test]
    fn u64_from_bytes_valid() {
        let val: u64 = 42;
        assert_eq!(u64_from_bytes(&val.to_le_bytes()), 42);
    }

    #[test]
    fn u64_from_bytes_invalid_length_defaults_to_1() {
        assert_eq!(u64_from_bytes(&[1, 2, 3]), 1);
        assert_eq!(u64_from_bytes(&[]), 1);
    }

    #[test]
    fn u64_from_bytes_zero() {
        let val: u64 = 0;
        assert_eq!(u64_from_bytes(&val.to_le_bytes()), 0);
    }

    #[test]
    fn u64_from_bytes_max() {
        let val = u64::MAX;
        assert_eq!(u64_from_bytes(&val.to_le_bytes()), u64::MAX);
    }

    // --- Key helpers ---

    #[test]
    fn node_key_format() {
        let key = node_key(42);
        assert!(key.starts_with(PREFIX_NODE));
        assert_eq!(&key[PREFIX_NODE.len()..], &42u64.to_le_bytes());
    }

    #[test]
    fn node_uuid_key_format() {
        let uuid = [1u8; 16];
        let key = node_uuid_key(&uuid);
        assert!(key.starts_with(PREFIX_NODE_UUID));
        assert_eq!(&key[PREFIX_NODE_UUID.len()..], &uuid);
    }

    #[test]
    fn node_title_key_format() {
        let key = node_title_key("hello");
        assert!(key.starts_with(PREFIX_NODE_TITLE));
        assert_eq!(&key[PREFIX_NODE_TITLE.len()..], b"hello");
    }

    // --- Serialization helpers ---

    #[test]
    fn serialize_deserialize_node_roundtrip() {
        use crate::model::{NewNode, Properties};
        let node = NewNode {
            kind: "note".to_string(),
            title: "Test".to_string(),
            body: "body".to_string(),
            body_html: "<p>body</p>".to_string(),
            properties: Properties::default(),
        }
        .into_node(1);

        let bytes = serialize_node(&node).unwrap();
        let decoded = deserialize_node(&bytes).unwrap();
        assert_eq!(decoded, node);
    }

    // --- Node CRUD (unit-level) ---

    #[test]
    fn create_and_get_node() {
        use crate::model::{NewNode, Properties};
        let db = Drevo::open_in_memory().unwrap();
        let node = db
            .create_node(NewNode {
                kind: "note".to_string(),
                title: "Unit".to_string(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        assert_eq!(node.id, 1);
        let fetched = db.get_node(1).unwrap().unwrap();
        assert_eq!(fetched, node);
    }

    #[test]
    fn get_node_missing_returns_none() {
        let db = Drevo::open_in_memory().unwrap();
        assert!(db.get_node(100).unwrap().is_none());
    }

    #[test]
    fn delete_node_then_get_returns_none() {
        use crate::model::{NewNode, Properties};
        let db = Drevo::open_in_memory().unwrap();
        let node = db
            .create_node(NewNode {
                kind: "note".to_string(),
                title: "Del".to_string(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        db.delete_node(node.id).unwrap();
        assert!(db.get_node(node.id).unwrap().is_none());
    }

    // --- Edge key helpers ---

    #[test]
    fn edge_key_format() {
        let key = edge_key(7);
        assert!(key.starts_with(PREFIX_EDGE));
        assert_eq!(&key[PREFIX_EDGE.len()..], &7u64.to_le_bytes());
    }

    #[test]
    fn edge_uuid_key_format() {
        let uuid = [2u8; 16];
        let key = edge_uuid_key(&uuid);
        assert!(key.starts_with(PREFIX_EDGE_UUID));
        assert_eq!(&key[PREFIX_EDGE_UUID.len()..], &uuid);
    }

    #[test]
    fn out_edge_key_format() {
        let key = out_edge_key(1, 5);
        assert!(key.starts_with(PREFIX_OUT));
        // Format: out:{from_id_8bytes}:{edge_id_8bytes}
        let rest = &key[PREFIX_OUT.len()..];
        assert_eq!(&rest[..8], &1u64.to_le_bytes());
        assert_eq!(rest[8], b':');
        assert_eq!(&rest[9..], &5u64.to_le_bytes());
    }

    #[test]
    fn in_edge_key_format() {
        let key = in_edge_key(2, 10);
        assert!(key.starts_with(PREFIX_IN));
        let rest = &key[PREFIX_IN.len()..];
        assert_eq!(&rest[..8], &2u64.to_le_bytes());
        assert_eq!(rest[8], b':');
        assert_eq!(&rest[9..], &10u64.to_le_bytes());
    }

    #[test]
    fn out_prefix_format() {
        let prefix = out_prefix(3);
        let key = out_edge_key(3, 99);
        assert!(key.starts_with(&prefix));
    }

    #[test]
    fn in_prefix_format() {
        let prefix = in_prefix(4);
        let key = in_edge_key(4, 88);
        assert!(key.starts_with(&prefix));
    }

    #[test]
    fn edge_id_from_adjacency_key_valid() {
        let prefix = out_prefix(1);
        let key = out_edge_key(1, 42);
        assert_eq!(edge_id_from_adjacency_key(&key, &prefix), 42);
    }

    #[test]
    fn edge_id_from_adjacency_key_invalid_returns_zero() {
        let prefix = b"out:";
        let key = b"out:short";
        assert_eq!(edge_id_from_adjacency_key(key, prefix), 0);
    }

    // --- Node kind index key helpers ---

    #[test]
    fn node_kind_key_format() {
        let key = node_kind_key("note", 42);
        assert!(key.starts_with(PREFIX_NODE_KIND));
        let rest = &key[PREFIX_NODE_KIND.len()..];
        assert!(rest.starts_with(b"note:"));
        assert_eq!(&rest[5..], &42u64.to_le_bytes());
    }

    #[test]
    fn node_kind_prefix_matches_key() {
        let prefix = node_kind_prefix("task");
        let key = node_kind_key("task", 99);
        assert!(key.starts_with(&prefix));
    }

    #[test]
    fn node_kind_prefix_does_not_match_different_kind() {
        let prefix = node_kind_prefix("note");
        let key = node_kind_key("note2", 1);
        // "note2" should NOT match "note:" prefix — because the prefix
        // ends with "note:" and the key has "note2:"
        assert!(!key.starts_with(&prefix));
    }

    #[test]
    fn id_from_kind_key_extracts_id() {
        let prefix = node_kind_prefix("note");
        let key = node_kind_key("note", 77);
        assert_eq!(id_from_kind_key(&key, &prefix), 77);
    }

    // --- Edge kind index key helpers ---

    #[test]
    fn edge_kind_key_format() {
        let key = edge_kind_key("links_to", 5);
        assert!(key.starts_with(PREFIX_EDGE_KIND));
        let rest = &key[PREFIX_EDGE_KIND.len()..];
        assert!(rest.starts_with(b"links_to:"));
        assert_eq!(&rest[9..], &5u64.to_le_bytes());
    }

    #[test]
    fn edge_kind_prefix_matches_key() {
        let prefix = edge_kind_prefix("tagged_with");
        let key = edge_kind_key("tagged_with", 10);
        assert!(key.starts_with(&prefix));
    }

    // --- list_nodes_by_kind (unit-level) ---

    #[test]
    fn list_nodes_by_kind_basic() {
        use crate::model::{NewNode, Properties};
        let db = Drevo::open_in_memory().unwrap();
        db.create_node(NewNode {
            kind: "note".to_string(),
            title: "A".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();
        db.create_node(NewNode {
            kind: "task".to_string(),
            title: "B".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();

        let notes = db.list_nodes_by_kind("note", 10, 0).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "A");
    }

    // --- list_edges_by_kind (unit-level) ---

    #[test]
    fn list_edges_by_kind_basic() {
        use crate::model::{NewEdge, NewNode, Properties};
        let db = Drevo::open_in_memory().unwrap();
        let n1 = db
            .create_node(NewNode {
                kind: "note".to_string(),
                title: "A".to_string(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        let n2 = db
            .create_node(NewNode {
                kind: "note".to_string(),
                title: "B".to_string(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        db.create_edge(NewEdge {
            from_id: n1.id,
            to_id: n2.id,
            kind: "links_to".to_string(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .unwrap();

        let links = db.list_edges_by_kind("links_to", 10, 0).unwrap();
        assert_eq!(links.len(), 1);

        let empty = db.list_edges_by_kind("nonexistent", 10, 0).unwrap();
        assert!(empty.is_empty());
    }

    // --- Edge serialization ---

    #[test]
    fn serialize_deserialize_edge_roundtrip() {
        use crate::model::{NewEdge, Properties};
        let edge = NewEdge {
            from_id: 1,
            to_id: 2,
            kind: "links_to".to_string(),
            weight: 1.5,
            properties: Properties::default(),
        }
        .into_edge(1);

        let bytes = serialize_edge(&edge).unwrap();
        let decoded = deserialize_edge(&bytes).unwrap();
        assert_eq!(decoded, edge);
    }

    // --- Edge CRUD (unit-level) ---

    #[test]
    fn create_and_get_edge() {
        use crate::model::{NewEdge, NewNode, Properties};
        let db = Drevo::open_in_memory().unwrap();
        let n1 = db
            .create_node(NewNode {
                kind: "note".to_string(),
                title: "A".to_string(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        let n2 = db
            .create_node(NewNode {
                kind: "note".to_string(),
                title: "B".to_string(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        let edge = db
            .create_edge(NewEdge {
                from_id: n1.id,
                to_id: n2.id,
                kind: "links_to".to_string(),
                weight: 1.0,
                properties: Properties::default(),
            })
            .unwrap();
        assert_eq!(edge.id, 1);
        let fetched = db.get_edge(1).unwrap().unwrap();
        assert_eq!(fetched, edge);
    }

    #[test]
    fn get_edge_missing_returns_none() {
        let db = Drevo::open_in_memory().unwrap();
        assert!(db.get_edge(100).unwrap().is_none());
    }

    #[test]
    fn delete_edge_then_get_returns_none() {
        use crate::model::{NewEdge, NewNode, Properties};
        let db = Drevo::open_in_memory().unwrap();
        let n1 = db
            .create_node(NewNode {
                kind: "note".to_string(),
                title: "A".to_string(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        let n2 = db
            .create_node(NewNode {
                kind: "note".to_string(),
                title: "B".to_string(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        let edge = db
            .create_edge(NewEdge {
                from_id: n1.id,
                to_id: n2.id,
                kind: "links_to".to_string(),
                weight: 1.0,
                properties: Properties::default(),
            })
            .unwrap();
        db.delete_edge(edge.id).unwrap();
        assert!(db.get_edge(edge.id).unwrap().is_none());
    }

    // --- search_fts ---

    fn test_node(kind: &str, title: &str, body: &str) -> NewNode {
        use crate::model::Properties;
        NewNode {
            kind: kind.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            body_html: String::new(),
            properties: Properties::default(),
        }
    }

    #[test]
    fn search_fts_empty_query() {
        let db = Drevo::open_in_memory().unwrap();
        db.create_node(test_node("note", "Rust", "")).unwrap();
        let results = db.search_fts("", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_fts_basic_match() {
        let db = Drevo::open_in_memory().unwrap();
        db.create_node(test_node("note", "Rust programming", ""))
            .unwrap();
        let results = db.search_fts("rust", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].node.title, "Rust programming");
        assert!(results[0].score > 0.0);
    }

    #[test]
    fn search_fts_no_match() {
        let db = Drevo::open_in_memory().unwrap();
        db.create_node(test_node("note", "Hello", "")).unwrap();
        let results = db.search_fts("zzzzz", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_fts_limit_works() {
        let db = Drevo::open_in_memory().unwrap();
        for i in 0..10 {
            db.create_node(test_node("note", &format!("Rust item {}", i), ""))
                .unwrap();
        }
        let results = db.search_fts("rust", 3).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn search_fts_results_sorted_by_score_desc() {
        let db = Drevo::open_in_memory().unwrap();
        db.create_node(test_node("note", "Rust", "")).unwrap();
        db.create_node(test_node(
            "note",
            "Rust programming language",
            "Rust is a systems programming language",
        ))
        .unwrap();
        let results = db.search_fts("rust programming", 10).unwrap();
        if results.len() >= 2 {
            assert!(results[0].score >= results[1].score);
        }
    }

    #[test]
    fn search_fts_scored_node_fields() {
        let db = Drevo::open_in_memory().unwrap();
        let node = db
            .create_node(test_node("note", "Rust language", ""))
            .unwrap();
        let results = db.search_fts("rust", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].node.id, node.id);
        assert_eq!(results[0].node.uuid, node.uuid);
    }

    // --- list_recent ---

    #[test]
    fn list_recent_empty_db() {
        let db = Drevo::open_in_memory().unwrap();
        let nodes = db.list_recent(10).unwrap();
        assert!(nodes.is_empty());
    }

    #[test]
    fn list_recent_returns_nodes_newest_first() {
        let db = Drevo::open_in_memory().unwrap();
        let n1 = db.create_node(test_node("note", "First", "")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let n2 = db.create_node(test_node("note", "Second", "")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let n3 = db.create_node(test_node("note", "Third", "")).unwrap();

        let nodes = db.list_recent(10).unwrap();
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0].id, n3.id);
        assert_eq!(nodes[1].id, n2.id);
        assert_eq!(nodes[2].id, n1.id);
    }

    #[test]
    fn list_recent_respects_limit() {
        let db = Drevo::open_in_memory().unwrap();
        for i in 0..5 {
            db.create_node(test_node("note", &format!("N{}", i), ""))
                .unwrap();
        }
        let nodes = db.list_recent(3).unwrap();
        assert_eq!(nodes.len(), 3);
    }

    #[test]
    fn list_recent_zero_limit() {
        let db = Drevo::open_in_memory().unwrap();
        db.create_node(test_node("note", "A", "")).unwrap();
        let nodes = db.list_recent(0).unwrap();
        assert!(nodes.is_empty());
    }

    #[test]
    fn list_recent_updated_node_moves_to_top() {
        let db = Drevo::open_in_memory().unwrap();
        let n1 = db.create_node(test_node("note", "First", "")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let _n2 = db.create_node(test_node("note", "Second", "")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));

        // Update the first node — it should move to the top
        db.update_node(
            n1.id,
            NodePatch {
                body: Some("updated body".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let nodes = db.list_recent(10).unwrap();
        assert_eq!(nodes[0].id, n1.id);
    }

    #[test]
    fn list_recent_deleted_node_is_excluded() {
        let db = Drevo::open_in_memory().unwrap();
        let n1 = db.create_node(test_node("note", "Stay", "")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let n2 = db.create_node(test_node("note", "Gone", "")).unwrap();

        db.delete_node(n2.id).unwrap();

        let nodes = db.list_recent(10).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].id, n1.id);
    }

    // --- updated_at index key helpers ---

    #[test]
    fn updated_key_format() {
        let key = updated_key(1000, 42);
        assert!(key.starts_with(PREFIX_UPDATED));
        let rest = &key[PREFIX_UPDATED.len()..];
        // inverted timestamp (8 bytes) + ':' + node_id (8 bytes)
        assert_eq!(rest.len(), 8 + 1 + 8);
        assert_eq!(rest[8], b':');
    }

    #[test]
    fn updated_key_newer_timestamp_sorts_first() {
        let old_key = updated_key(1000, 1);
        let new_key = updated_key(2000, 2);
        // Newer timestamp should produce a smaller key (lower inverted value)
        assert!(new_key < old_key);
    }
}
