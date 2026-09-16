//! `NativeBackend` — a Drevo-shaped facade over the native engine
//! ([`NativeService`]), so the Python binding runs on the native durable engine
//! instead of the KV/redb store (issue #446, epic #444 — "drevo-py off redb").
//!
//! The Python handle (`handle.rs`) was written against the `Drevo` (KV) inherent
//! API. This adapter presents the **same method names, signatures and
//! `Result<_, DrevoError>` shapes** over `NativeService` (+ its `NativeGraph`),
//! so the handle bodies are unchanged — all the KV↔native reconciliation lives
//! here, in one compiler-checked place. The Python API is byte-for-byte
//! identical to before; only the storage backend changed.
//!
//! Storage format: `open(path)` opens the native write-ahead log at `path`
//! (crash-recovering, fsync-per-write). This is a different on-disk format from
//! the old redb file; legacy data is migrated via GraphML export/import.

use std::path::Path;

use drevo::db::{BloatReport, CompactReport};
use drevo::dump::ImportReport;
use drevo::engine::GraphEngine;
use drevo::error::DrevoError;
use drevo::model::{
    Direction, Edge, EdgePatch, NewEdge, NewNode, Node, NodePatch, ScoredNode, SubGraph,
};
use drevo::native_service::NativeService;
use drevo::vector::{HnswConfig, HnswIndex, Vector};

type Result<T> = std::result::Result<T, DrevoError>;

/// The native engine behind the Python `Drevo` handle. Presents the KV handle's
/// inherent API over [`NativeService`].
pub struct NativeBackend {
    svc: NativeService,
}

impl NativeBackend {
    // ── Lifecycle ────────────────────────────────────────────────────────

    /// Open (or create) the durable native store — the WAL at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            svc: NativeService::open(path)?,
        })
    }

    /// An ephemeral in-memory store (lost on drop).
    pub fn in_memory() -> Self {
        Self {
            svc: NativeService::in_memory(),
        }
    }

    /// Flush + release. The durable engine fsyncs every write, so closing is
    /// just dropping the handle; kept for API parity.
    pub fn close(self) -> Result<()> {
        Ok(())
    }

    /// Reclaim log space + checkpoint. Unlike the KV handle this needs only
    /// `&self` (the WAL compactor quiesces writes internally).
    pub fn compact(&self) -> Result<CompactReport> {
        self.svc.compact()
    }

    /// Liveness probe.
    pub fn health_check(&self) -> Result<()> {
        self.svc.health_check()
    }

    /// Physical vs logical size report.
    pub fn bloat_report(&self) -> Result<BloatReport> {
        Ok(self.svc.storage_bloat())
    }

    // ── GraphML backup ───────────────────────────────────────────────────

    pub fn export_graphml(&self) -> Result<String> {
        self.svc.export_graphml()
    }

    pub fn export_graphml_to_path(&self, path: &Path) -> Result<()> {
        let xml = self.svc.export_graphml()?;
        std::fs::write(path, xml)?; // DrevoError: Io(#[from])
        Ok(())
    }

    pub fn import_graphml(&self, xml: &str) -> Result<ImportReport> {
        self.svc.import_graphml(xml)
    }

    pub fn import_graphml_from_path(&self, path: &Path) -> Result<ImportReport> {
        let xml = std::fs::read_to_string(path)?;
        self.svc.import_graphml(&xml)
    }

    // ── Node CRUD ────────────────────────────────────────────────────────

    pub fn create_node(&self, new_node: NewNode) -> Result<Node> {
        Ok(self.svc.graph().create_node(new_node)?)
    }

    pub fn create_nodes(&self, new_nodes: Vec<NewNode>) -> Result<Vec<Node>> {
        Ok(self.svc.graph().create_nodes(new_nodes)?)
    }

    pub fn get_node(&self, id: u64) -> Result<Option<Node>> {
        match self.svc.get_node(id) {
            Ok(node) => Ok(Some(node)),
            Err(DrevoError::NodeNotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn get_node_by_uuid(&self, uuid: &[u8; 16]) -> Result<Option<Node>> {
        Ok(self.svc.get_node_by_uuid(*uuid))
    }

    pub fn get_node_by_title(&self, title: &str) -> Result<Option<Node>> {
        Ok(self.svc.get_node_by_title(title))
    }

    pub fn update_node(&self, id: u64, patch: NodePatch) -> Result<Node> {
        Ok(self.svc.graph().update_node(id, patch)?)
    }

    pub fn delete_node(&self, id: u64) -> Result<()> {
        Ok(self.svc.graph().delete_node(id)?)
    }

    // ── Edge CRUD ────────────────────────────────────────────────────────

    pub fn create_edge(&self, new_edge: NewEdge) -> Result<Edge> {
        Ok(self.svc.graph().create_edge(new_edge)?)
    }

    pub fn create_edges(&self, new_edges: Vec<NewEdge>) -> Result<Vec<Edge>> {
        Ok(self.svc.graph().create_edges(new_edges)?)
    }

    pub fn get_edge(&self, id: u64) -> Result<Option<Edge>> {
        Ok(self.svc.graph().get_edge(id)?)
    }

    pub fn get_edge_by_uuid(&self, uuid: &[u8; 16]) -> Result<Option<Edge>> {
        match self.svc.graph().edge_id_of_uuid(*uuid) {
            Some(id) => Ok(self.svc.graph().get_edge(id)?),
            None => Ok(None),
        }
    }

    pub fn update_edge(&self, id: u64, patch: EdgePatch) -> Result<Edge> {
        Ok(self.svc.graph().update_edge(id, patch)?)
    }

    pub fn delete_edge(&self, id: u64) -> Result<()> {
        Ok(self.svc.graph().delete_edge(id)?)
    }

    pub fn edges_of(&self, node_id: u64, direction: Direction) -> Result<Vec<Edge>> {
        Ok(self.svc.graph().snapshot().edges_of(node_id, direction))
    }

    // ── Index queries ────────────────────────────────────────────────────

    pub fn list_nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Node>> {
        Ok(self
            .svc
            .graph()
            .snapshot()
            .nodes_by_kind(kind, limit, offset))
    }

    pub fn list_edges_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Edge>> {
        Ok(self.svc.list_edges_by_kind(kind, limit, offset))
    }

    pub fn list_recent(&self, limit: usize) -> Result<Vec<Node>> {
        Ok(self.svc.list_recent(limit))
    }

    // ── Traversal ────────────────────────────────────────────────────────

    pub fn bfs(
        &self,
        start_id: u64,
        max_depth: u8,
        direction: Direction,
        edge_kind: Option<&str>,
    ) -> Result<Vec<Node>> {
        self.svc.bfs(start_id, max_depth, direction, edge_kind)
    }

    pub fn dfs(
        &self,
        start_id: u64,
        max_depth: u8,
        direction: Direction,
        edge_kind: Option<&str>,
    ) -> Result<Vec<Node>> {
        self.svc.dfs(start_id, max_depth, direction, edge_kind)
    }

    pub fn shortest_path_filtered(
        &self,
        from: u64,
        to: u64,
        edge_kind: Option<&str>,
    ) -> Result<Option<Vec<u64>>> {
        self.svc.shortest_path_filtered(from, to, edge_kind)
    }

    pub fn subgraph_filtered(
        &self,
        root: u64,
        depth: u8,
        edge_kind: Option<&str>,
    ) -> Result<SubGraph> {
        self.svc.subgraph_filtered(root, depth, edge_kind)
    }

    pub fn neighbors(
        &self,
        node_id: u64,
        direction: Direction,
        edge_kind: Option<&str>,
    ) -> Result<Vec<Node>> {
        Ok(self.svc.neighbors(node_id, direction, edge_kind))
    }

    // ── Full-text search ─────────────────────────────────────────────────

    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<ScoredNode>> {
        Ok(self.svc.search_fts(query, limit))
    }

    // ── Vector embeddings ────────────────────────────────────────────────

    pub fn set_embedding(&self, node_id: u64, embedding: Vector) -> Result<()> {
        self.svc.set_embedding(node_id, embedding.0)
    }

    pub fn set_embeddings_batch(&self, embeddings: &[(u64, Vector)]) -> Result<()> {
        let owned: Vec<(u64, Vec<f32>)> = embeddings
            .iter()
            .map(|(id, v)| (*id, v.0.clone()))
            .collect();
        self.svc.set_embeddings_batch(&owned)
    }

    pub fn get_embedding(&self, node_id: u64) -> Result<Option<Vector>> {
        Ok(self.svc.get_embedding(node_id).map(Vector::from))
    }

    pub fn delete_embedding(&self, node_id: u64) -> Result<()> {
        self.svc.delete_embedding(node_id)
    }

    pub fn embedding_count(&self) -> Result<usize> {
        Ok(self.svc.embedding_count())
    }

    pub fn build_vector_index(&self, config: HnswConfig) -> Result<HnswIndex> {
        self.svc.build_vector_index(config)
    }
}
