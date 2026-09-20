//! Durable-native serving layer (RFC `docs/rfc-native-core.md` #307,
//! Phase 4/7 — the track toward retiring redb).
//!
//! [`crate::native_service::NativeService`] IS the store of record: a
//! WAL-backed [`crate::native::NativeGraph`]
//! (crash-recovering, fsync-per-statement — see
//! [`crate::native::NativeGraph::open_durable`]) plus the full native index
//! stack — label, property, value cache, **and full-text** — kept current by
//! tailing the engine's change-feed between statements.
//!
//! # Consistency model
//!
//! Indexes are synced *between* statements: before a query runs, the service
//! catches the indexes up to the change-feed head; within a statement that
//! writes, the executor already distrusts index narrowing (the in-statement
//! staleness gate), so a query always sees its own writes. A write committed
//! by a *concurrent* statement mid-query may or may not be observed — the
//! same read-committed-style race the KV engine has today; snapshot-isolated
//! serving is the MVCC phase's business (RFC Phase 3 knob), not this layer's.
//!
//! The service is `Sync`: reads run concurrently under a shared lock; only
//! the first query after a write pays the (incremental) index re-sync under
//! the exclusive lock.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::RwLock;

use crate::cypher::ast::Query;
use crate::cypher::executor::{
    execute_on_engine_with_context, ExecError, ExecResult, NativeQueryContext, Value,
};
use crate::error::DrevoError;
use crate::lww::{OriginId, Stamp};
use crate::native::NativeGraph;
use crate::native_fts::{NativeFtsIndex, NativeFtsRelIndex};
use crate::native_label_index::NativeLabelIndex;
use crate::native_property_index::NativePropertyIndex;
use crate::native_value_cache::NativeValueCache;

/// The index stack plus the change-feed head it is synced to.
struct ServiceIndexes {
    /// Label index (secondary `_labels`).
    labels: NativeLabelIndex,
    /// Property-equality index.
    props: NativePropertyIndex,
    /// Executor `NodeValue` projection cache.
    values: NativeValueCache,
    /// Trigram BM25 full-text index — `fts.search` served natively.
    fts: NativeFtsIndex,
    /// Trigram BM25 relationship full-text index — `fts.searchRelationships`
    /// served natively.
    fts_rel: NativeFtsRelIndex,
    /// [`crate::native::NativeGraph::change_head`] at the last sync.
    synced_head: u64,
}

impl ServiceIndexes {
    fn synced_over(graph: &NativeGraph) -> Self {
        let mut idx = ServiceIndexes {
            labels: NativeLabelIndex::new(),
            props: NativePropertyIndex::new(),
            values: NativeValueCache::new(),
            fts: NativeFtsIndex::new(),
            fts_rel: NativeFtsRelIndex::new(),
            synced_head: 0,
        };
        idx.catch_up(graph);
        idx
    }

    fn catch_up(&mut self, graph: &NativeGraph) {
        self.labels.sync(graph);
        self.props.sync(graph);
        self.values.sync(graph);
        self.fts.sync(graph);
        self.fts_rel.sync(graph);
        self.synced_head = graph.change_head();
    }
}

/// A durable native graph serving Cypher with its full index stack. See the
/// [module docs](self).
pub struct NativeService {
    /// The WAL-backed store of record.
    graph: NativeGraph,
    /// Index stack; shared for reads, exclusive to re-sync after writes.
    indexes: RwLock<ServiceIndexes>,
    /// Server-side text embedder, when the operator configured one —
    /// installed once at startup (mirroring the KV handle's embedder), read
    /// lock-free afterwards. Lets `drevo.semantic.embed` / `.query` run on
    /// the durable engine.
    #[cfg(feature = "http")]
    embedder: std::sync::OnceLock<std::sync::Arc<dyn crate::embeddings::TextEmbedder>>,
    /// Compact the WAL (and trim the change-feed) after this many appended
    /// ops since the last compaction — the runtime bound on log growth
    /// (reopen-time compaction alone lets a long-running server's log grow
    /// with history, not state).
    compact_every_ops: u64,
    /// [`crate::native::NativeGraph::change_head`] at the last compaction.
    last_compact_head: AtomicU64,
    /// Guards against overlapping compactions.
    compacting: AtomicBool,
    /// Node-side auto-embedding registry — the native home of what the KV
    /// handle keeps in `Drevo::semantic` (issue #447). Authoritative
    /// control-plane state, persisted to the `semantic.json` sidecar.
    semantic: RwLock<crate::semantic_index::SemanticIndexRegistry>,
    /// Relationship-side registry — the edge mirror of [`Self::semantic`],
    /// the native home of `Drevo::rel_semantic`.
    rel_semantic: RwLock<crate::semantic_index::SemanticIndexRegistry>,
    /// Sidecar path for the two registries (`<wal dir>/semantic.json`), or
    /// `None` for an in-memory service (nothing to persist).
    semantic_sidecar: Option<std::path::PathBuf>,
    /// Cumulative auto-embed failures per target (issue #447; native mirror of
    /// `Drevo::embed_failures`), keyed by `(target_kind, name, embedding_property)`.
    /// Auto-embed on write is fail-open — a transient embedder outage never
    /// fails a write — so each swallowed failure is tallied here to feed
    /// `drevo.semantic.status`'s `failed_count` / `last_error`. Runtime-only
    /// (not persisted); `http`-gated like the embedder that produces it.
    #[cfg(feature = "http")]
    embed_failures:
        std::sync::Mutex<std::collections::HashMap<(String, String, String), EmbedFailureStat>>,
}

/// A per-target tally of swallowed native auto-embed failures (issue #447;
/// mirrors the KV `crate::db::EmbedFailureStat`).
#[cfg(feature = "http")]
#[derive(Debug, Clone, Default)]
struct EmbedFailureStat {
    /// How many embed attempts have been swallowed for this target.
    count: u64,
    /// The most recent failure message, for the `last_error` column.
    last_error: String,
}

/// On-disk shape of the `semantic.json` sidecar: both registries in one file,
/// atomically rewritten on every control-plane mutation (the native
/// counterpart of the KV `meta:semantic_registry` / `..._rel_registry` blobs).
#[cfg(not(target_arch = "wasm32"))]
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SemanticSidecar {
    node: crate::semantic_index::SemanticIndexRegistry,
    rel: crate::semantic_index::SemanticIndexRegistry,
}

#[cfg(not(target_arch = "wasm32"))]
fn semantic_sidecar_path(wal_path: &std::path::Path) -> std::path::PathBuf {
    wal_path.with_file_name("semantic.json")
}

/// Load both registries from the sidecar, or empty ones if absent/unparseable
/// (best-effort, mirroring the tombstone sidecar and the KV loader).
#[cfg(not(target_arch = "wasm32"))]
fn load_semantic_sidecar(path: &std::path::Path) -> SemanticSidecar {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => SemanticSidecar::default(),
    }
}

impl NativeService {
    /// Open (or create) the durable store at `path` — the write-ahead log the
    /// graph recovers from and appends to. The log is compacted on open, so a
    /// long overwrite history costs restart time only once, and the indexes
    /// are built before the first query.
    ///
    /// # Errors
    ///
    /// Propagates [`crate::error::DrevoError`] from WAL recovery, compaction,
    /// or index construction.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, DrevoError> {
        Self::open_with_compact_threshold(path, Self::DEFAULT_COMPACT_EVERY_OPS)
    }

    /// [`Self::open`] with an explicit runtime-compaction threshold: after
    /// `compact_every_ops` appended ops, the next statement compacts the
    /// WAL in place (writes are quiesced for the rewrite's duration) and
    /// trims the consumed change-feed history. Exposed for operators and
    /// tests; [`Self::open`] uses [`Self::DEFAULT_COMPACT_EVERY_OPS`].
    ///
    /// # Errors
    ///
    /// Propagates [`crate::error::DrevoError`] from WAL recovery,
    /// compaction, or index construction.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open_with_compact_threshold(
        path: impl AsRef<std::path::Path>,
        compact_every_ops: u64,
    ) -> Result<Self, DrevoError> {
        // The replica identity (issue #389) lives on the store of record:
        // `NativeGraph::open_durable` loads/mints+persists `origin.json` next to
        // the WAL, reused across restarts.
        let path = path.as_ref();
        let graph = NativeGraph::open_durable(path)?;
        graph.compact_wal()?;
        let indexes = RwLock::new(ServiceIndexes::synced_over(&graph));
        let last_compact_head = AtomicU64::new(graph.change_head());
        let sidecar = semantic_sidecar_path(path);
        let SemanticSidecar { node, rel } = load_semantic_sidecar(&sidecar);
        Ok(Self {
            graph,
            indexes,
            #[cfg(feature = "http")]
            embedder: std::sync::OnceLock::new(),
            compact_every_ops,
            last_compact_head,
            compacting: AtomicBool::new(false),
            semantic: RwLock::new(node),
            rel_semantic: RwLock::new(rel),
            semantic_sidecar: Some(sidecar),
            #[cfg(feature = "http")]
            embed_failures: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Install the server-side query embedder (once; later calls return
    /// `false` and change nothing) — the durable-engine counterpart of
    /// [`crate::db::Drevo::set_embedder`].
    #[cfg(feature = "http")]
    pub fn set_embedder(
        &self,
        embedder: std::sync::Arc<dyn crate::embeddings::TextEmbedder>,
    ) -> bool {
        self.embedder.set(embedder).is_ok()
    }

    /// An ephemeral (non-durable) service — the same serving stack over an
    /// in-memory graph, for tests and embedded use.
    pub fn in_memory() -> Self {
        let graph = NativeGraph::new();
        let indexes = RwLock::new(ServiceIndexes::synced_over(&graph));
        Self {
            graph,
            indexes,
            #[cfg(feature = "http")]
            embedder: std::sync::OnceLock::new(),
            compact_every_ops: Self::DEFAULT_COMPACT_EVERY_OPS,
            last_compact_head: AtomicU64::new(0),
            compacting: AtomicBool::new(false),
            semantic: RwLock::new(crate::semantic_index::SemanticIndexRegistry::new()),
            rel_semantic: RwLock::new(crate::semantic_index::SemanticIndexRegistry::new()),
            semantic_sidecar: None,
            #[cfg(feature = "http")]
            embed_failures: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// This replica's stable [`OriginId`] (issue #389) — delegated to the store
    /// of record, which persists it next to the WAL.
    pub fn origin_id(&self) -> OriginId {
        self.graph.origin_id()
    }

    /// Issue the next causal [`Stamp`] `(hlc, origin)` for a write on this
    /// replica (delegated to the store of record). Strictly increasing, so a
    /// sequence of local writes is totally ordered; two replicas' stamps are
    /// ordered by HLC then origin.
    pub fn next_stamp(&self) -> Stamp {
        self.graph.next_stamp()
    }

    /// Default runtime-compaction threshold (appended ops between
    /// compactions). Large enough that steady write loads compact rarely,
    /// small enough that the log never dwarfs the state.
    pub const DEFAULT_COMPACT_EVERY_OPS: u64 = 4096;

    /// The underlying engine — read-side introspection (counts, status).
    pub fn graph(&self) -> &NativeGraph {
        &self.graph
    }

    /// The storage-panel bloat report for the WAL store — the engine-agnostic
    /// counterpart of [`crate::db::Drevo::bloat_report`]. `file_bytes` is the
    /// physical WAL size, `logical_bytes`/`stored_bytes` the size a compacted
    /// log would occupy (the append-only WAL accumulates superseded upserts,
    /// tombstones and old versions); `bloat_ratio = file / logical` is the
    /// reclaimable fraction. Secondary indexes are in-memory (rebuilt from the
    /// WAL) so they add no on-disk footprint (`index_bytes = 0`).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn storage_bloat(&self) -> crate::db::BloatReport {
        let g = &self.graph;
        let file_bytes = g.wal_bytes();
        let logical = g.wal_compacted_bytes();
        let bloat_ratio = match file_bytes {
            Some(f) if logical > 0 => Some(f as f64 / logical as f64),
            _ => None,
        };
        crate::db::BloatReport {
            file_bytes,
            stored_bytes: logical,
            logical_bytes: logical,
            index_bytes: 0,
            node_count: g.node_count(),
            edge_count: g.edge_count(),
            bloat_ratio,
        }
    }

    /// Per-keyspace storage breakdown for the panel — the WAL engine's
    /// counterpart of [`crate::db::Drevo::keyspace_stats`]. The durable WAL
    /// stores only records on disk; these rows are the live in-memory index
    /// structures (records, adjacency, title, kind), so the panel's Keyspaces
    /// table is populated on both engines. FTS/vector keyspaces are
    /// KV-secondary-only and absent here.
    #[must_use]
    pub fn keyspace_stats(&self) -> Vec<crate::db::KeyspaceStat> {
        self.graph
            .keyspace_stats()
            .into_iter()
            .map(|(prefix, entries, content_bytes)| crate::db::KeyspaceStat {
                prefix,
                entries,
                content_bytes,
            })
            .collect()
    }

    /// Compact the WAL — rewrite it as the current state (atomic temp + fsync +
    /// rename) — and report the reclaimed bytes: the engine-agnostic counterpart
    /// of [`crate::db::Drevo::shrink_online`] (the storage panel's `shrink`).
    /// Writes are quiesced for the rewrite's duration.
    ///
    /// # Errors
    /// Propagates [`DrevoError`] on a filesystem or encode failure.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn compact(&self) -> Result<crate::db::CompactReport, DrevoError> {
        let stats = self.graph.compact_wal()?;
        // The log now equals the current state, so restart the runtime
        // auto-compaction threshold from here.
        self.last_compact_head
            .store(self.graph.change_head(), Ordering::SeqCst);
        Ok(crate::db::CompactReport {
            bytes_before: stats.bytes_before,
            bytes_after: stats.bytes_after,
            bytes_reclaimed: stats.reclaimed(),
            next_node_id: self.graph.next_node_id(),
            next_edge_id: self.graph.next_edge_id(),
        })
    }

    /// Execute one Cypher statement with the index stack attached. Reads run
    /// concurrently; the first statement after a write re-syncs the indexes
    /// first.
    ///
    /// # Errors
    ///
    /// Returns the executor's [`crate::cypher::executor::ExecError`];
    /// KV-secondary-only features (vector / semantic procedures) surface
    /// [`crate::cypher::executor::ExecError::EngineCapability`].
    pub fn execute(
        &self,
        query: &Query,
        params: HashMap<String, Value>,
    ) -> Result<ExecResult, ExecError> {
        let result = self.with_fresh_indexes(|idx| self.execute_with(idx, query, params));
        self.maybe_compact();
        result
    }

    /// Compact the WAL and trim the consumed change-feed once enough ops
    /// have accumulated since the last compaction — the runtime bound on
    /// disk (log rewrite as the state snapshot; a no-op for an in-memory
    /// engine) and memory (feed history behind every index's cursor is
    /// dropped). Runs inline on the statement that crosses the threshold:
    /// the pause is one state rewrite, and only one caller compacts at a
    /// time. A failed compaction leaves the log valid and is retried at
    /// the next threshold crossing.
    /// Rewrite the WAL as the state snapshot; `true` on success. On wasm
    /// there is no WAL (the durable constructor is not compiled), so only
    /// the feed trim applies.
    #[cfg(not(target_arch = "wasm32"))]
    fn compact_wal_ok(&self) -> bool {
        self.graph.compact_wal().is_ok()
    }

    /// See the non-wasm variant — nothing on disk to rewrite here.
    #[cfg(target_arch = "wasm32")]
    fn compact_wal_ok(&self) -> bool {
        true
    }

    fn maybe_compact(&self) {
        let head = self.graph.change_head();
        if head.saturating_sub(self.last_compact_head.load(Ordering::SeqCst))
            < self.compact_every_ops
        {
            return;
        }
        if self.compacting.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.compact_wal_ok() {
            self.last_compact_head
                .store(self.graph.change_head(), Ordering::SeqCst);
            // Every index re-syncs to the head before serving, so history at
            // or before the synced cursor is consumed; trim it to bound the
            // feed. (A cursor below the new floor would only mean a lagged
            // rebuild — every index handles that — but by construction the
            // service's indexes never lag past their own synced head.)
            let synced = self
                .indexes
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .synced_head;
            self.graph.trim_before(synced);
        }
        self.compacting.store(false, Ordering::SeqCst);
    }

    /// Run `serve` with the index stack caught up to the change-feed head:
    /// the shared lock when already fresh (reads run concurrently), the
    /// exclusive lock to re-sync after a write. Running under the exclusive
    /// guard in the stale case is deliberate: it only happens for the first
    /// request after a write, and it keeps the sync + serve pair simple
    /// (std's `RwLock` cannot downgrade).
    fn with_fresh_indexes<R>(&self, serve: impl FnOnce(&ServiceIndexes) -> R) -> R {
        {
            let idx = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            if idx.synced_head == self.graph.change_head() {
                return serve(&idx);
            }
        }
        let mut idx = self.indexes.write().unwrap_or_else(|e| e.into_inner());
        if idx.synced_head != self.graph.change_head() {
            idx.catch_up(&self.graph);
        }
        serve(&idx)
    }

    /// Full-text search over the service's BM25 index — the engine of the
    /// `POST /search/fts` route, matching the KV store's response shape.
    pub fn search_fts(&self, query: &str, limit: usize) -> Vec<crate::model::ScoredNode> {
        self.with_fresh_indexes(|idx| {
            idx.fts
                .search(query, limit)
                .into_iter()
                .filter_map(|(id, score)| {
                    self.graph
                        .get_node_arc(id)
                        .map(|node| crate::model::ScoredNode {
                            node: (*node).clone(),
                            score,
                        })
                })
                .collect()
        })
    }

    /// The graph as the `drevo-json-v1` dump document — the engine of
    /// `GET /export/json`, matching the KV route's body.
    ///
    /// # Errors
    ///
    /// Propagates scan failures and non-serialisable property values.
    pub fn export_json(&self) -> Result<String, DrevoError> {
        use crate::engine::GraphEngine;
        let dump = GraphEngine::export_dump(&self.graph)?;
        serde_json::to_string_pretty(&dump)
            .map_err(|e| DrevoError::Io(std::io::Error::other(e.to_string())))
    }

    /// Begin a registered transaction on the durable store — the session
    /// keeps the returned id between statements. See
    /// [`crate::native::NativeGraph::tx_begin`].
    pub fn begin_tx(&self) -> crate::native::NativeTxId {
        self.graph.tx_begin()
    }

    /// Execute one statement inside a registered transaction: the executor
    /// runs over the transaction's working copy (read-your-writes, invisible
    /// to concurrent statements) **without** index narrowing — the service's
    /// indexes describe the committed graph, not this transaction's view.
    ///
    /// # Errors
    ///
    /// The executor's [`crate::cypher::executor::ExecError`]; a closed
    /// transaction surfaces as a storage error rather than a panic.
    pub fn execute_in_tx(
        &self,
        tx: crate::native::NativeTxId,
        query: &Query,
        params: HashMap<String, Value>,
    ) -> Result<ExecResult, ExecError> {
        let Some(engine) = self.graph.tx_engine(tx) else {
            return Err(ExecError::Storage(DrevoError::Io(std::io::Error::other(
                "the transaction has already been closed",
            ))));
        };
        crate::cypher::executor::execute_on_engine(query, &engine, params)
    }

    /// Commit a registered transaction (one fsynced WAL batch, atomic swap),
    /// then apply the runtime compaction policy — a committed batch counts
    /// toward the threshold like any other write.
    ///
    /// # Errors
    ///
    /// [`crate::native::CommitError`] — `Conflict` when another writer
    /// committed since the transaction began (retryable), `Constraint` on a
    /// schema violation, `Io` on a WAL failure or a closed transaction.
    pub fn commit_tx(
        &self,
        tx: crate::native::NativeTxId,
    ) -> std::result::Result<(), crate::native::CommitError> {
        let result = self.graph.tx_commit(tx);
        if result.is_ok() {
            self.maybe_compact();
        }
        result
    }

    /// Discard a registered transaction. `false` when it was already closed.
    pub fn rollback_tx(&self, tx: crate::native::NativeTxId) -> bool {
        self.graph.tx_rollback(tx)
    }

    /// One node by storage id — the engine of `GET /nodes/{id}`.
    ///
    /// # Errors
    ///
    /// [`crate::error::DrevoError::NodeNotFound`] when the id does not
    /// exist, exactly as the KV route reports it.
    pub fn get_node(&self, id: u64) -> Result<crate::model::Node, DrevoError> {
        self.graph
            .get_node_arc(id)
            .map(|n| (*n).clone())
            .ok_or(crate::error::DrevoError::NodeNotFound(id))
    }

    /// Look up a node by its globally-unique UUID — embedded-handle parity with
    /// `Drevo::get_node_by_uuid` (issue #445). `None` if no node carries it.
    pub fn get_node_by_uuid(&self, uuid: [u8; 16]) -> Option<crate::model::Node> {
        self.graph.get_node_by_uuid(uuid)
    }

    /// Look up a node by its unique title — parity with
    /// `Drevo::get_node_by_title`. `None` if no node has that title.
    pub fn get_node_by_title(&self, title: &str) -> Option<crate::model::Node> {
        self.graph.get_node_by_title(title)
    }

    /// Most-recently-updated nodes first, capped at `limit` — parity with
    /// `Drevo::list_recent` (`updated_at` desc, id desc).
    pub fn list_recent(&self, limit: usize) -> Vec<crate::model::Node> {
        self.graph.list_recent(limit)
    }

    /// Edges of `kind`, id-ascending, paginated — parity with
    /// `Drevo::list_edges_by_kind`.
    pub fn list_edges_by_kind(
        &self,
        kind: &str,
        limit: usize,
        offset: usize,
    ) -> Vec<crate::model::Edge> {
        self.graph.list_edges_by_kind(kind, limit, offset)
    }

    /// Create a node — the engine of `POST /nodes`, embedded-handle parity with
    /// `Drevo::create_node`. Returns the stored node with generated id, uuid and
    /// timestamps.
    ///
    /// # Errors
    /// Propagates a WAL/encode failure as [`DrevoError`].
    pub fn create_node(
        &self,
        mut new_node: crate::model::NewNode,
    ) -> Result<crate::model::Node, DrevoError> {
        use crate::engine::GraphEngine;
        // Server-side auto-embed on ingest (#447): embed the configured text
        // property before the write, so `drevo.semantic.query` retrieves the
        // node with no client round-trip. Fail-open, no-op without an embedder.
        self.apply_auto_embeddings(&new_node.kind, &mut new_node.properties, None);
        Ok(self.graph.create_node(new_node)?)
    }

    /// Create many nodes in one durable batch — parity with
    /// `Drevo::create_nodes`. Each node is passed through the server-side
    /// auto-embed step (#447) before the batch is written, so a registered
    /// label's text property is embedded on bulk ingest exactly as it is on the
    /// single-node [`create_node`](Self::create_node) path. Fail-open: without a
    /// configured embedder (or for an unregistered label) the node is stored
    /// unchanged.
    ///
    /// # Errors
    /// Propagates a duplicate-title validation error (the whole batch fails and
    /// nothing is written) or a WAL/encode failure as [`DrevoError`].
    pub fn create_nodes(
        &self,
        mut new_nodes: Vec<crate::model::NewNode>,
    ) -> Result<Vec<crate::model::Node>, DrevoError> {
        for nn in &mut new_nodes {
            self.apply_auto_embeddings(&nn.kind, &mut nn.properties, None);
        }
        Ok(self.graph.create_nodes(new_nodes)?)
    }

    /// Create an edge — the engine of `POST /edges`, parity with
    /// `Drevo::create_edge`. Returns the stored edge.
    ///
    /// # Errors
    /// [`DrevoError::NodeNotFound`] when either endpoint is absent; propagates a
    /// WAL/encode failure.
    pub fn create_edge(
        &self,
        mut new_edge: crate::model::NewEdge,
    ) -> Result<crate::model::Edge, DrevoError> {
        use crate::engine::GraphEngine;
        // Relationship auto-embed on ingest (#447), mirroring `create_node`.
        self.apply_auto_embeddings_edge(&new_edge.kind, &mut new_edge.properties, None);
        Ok(self.graph.create_edge(new_edge)?)
    }

    /// Apply a partial update to a node — embedded-handle parity with
    /// `Drevo::update_node`. Returns the updated node.
    ///
    /// # Errors
    /// [`DrevoError::NodeNotFound`] when the id does not exist; propagates a
    /// WAL/encode failure.
    pub fn update_node(
        &self,
        id: u64,
        patch: crate::model::NodePatch,
    ) -> Result<crate::model::Node, DrevoError> {
        use crate::engine::GraphEngine;
        Ok(self.graph.update_node(id, patch)?)
    }

    /// Delete a node (and its incident edges) — parity with `Drevo::delete_node`.
    ///
    /// # Errors
    /// [`DrevoError::NodeNotFound`] when the id does not exist; propagates a
    /// WAL/encode failure.
    pub fn delete_node(&self, id: u64) -> Result<(), DrevoError> {
        use crate::engine::GraphEngine;
        Ok(self.graph.delete_node(id)?)
    }

    /// Fetch an edge by id — parity with `Drevo::get_edge`. `None` if absent.
    ///
    /// # Errors
    /// Propagates a WAL/decode failure as [`DrevoError`].
    pub fn get_edge(&self, id: u64) -> Result<Option<crate::model::Edge>, DrevoError> {
        use crate::engine::GraphEngine;
        Ok(self.graph.get_edge(id)?)
    }

    /// Apply a partial update to an edge — parity with `Drevo::update_edge`.
    /// Returns the updated edge.
    ///
    /// # Errors
    /// [`DrevoError::EdgeNotFound`] when the id does not exist; propagates a
    /// WAL/encode failure.
    pub fn update_edge(
        &self,
        id: u64,
        patch: crate::model::EdgePatch,
    ) -> Result<crate::model::Edge, DrevoError> {
        use crate::engine::GraphEngine;
        Ok(self.graph.update_edge(id, patch)?)
    }

    /// Delete an edge — parity with `Drevo::delete_edge`.
    ///
    /// # Errors
    /// [`DrevoError::EdgeNotFound`] when the id does not exist; propagates a
    /// WAL/encode failure.
    pub fn delete_edge(&self, id: u64) -> Result<(), DrevoError> {
        use crate::engine::GraphEngine;
        Ok(self.graph.delete_edge(id)?)
    }

    /// Bounded subgraph around `root` within `depth` hops — parity with
    /// `Drevo::subgraph`. Convenience wrapper over
    /// [`subgraph_filtered`](Self::subgraph_filtered) with no edge-kind filter.
    ///
    /// # Errors
    /// [`DrevoError::NodeNotFound`] when the root does not exist.
    pub fn subgraph(&self, root: u64, depth: u8) -> Result<crate::model::SubGraph, DrevoError> {
        self.subgraph_filtered(root, depth, None)
    }

    /// Nodes of `kind`, id-ascending, paginated — parity with
    /// `Drevo::list_nodes_by_kind` (the engine of `GET /nodes?kind=`).
    pub fn list_nodes_by_kind(
        &self,
        kind: &str,
        limit: usize,
        offset: usize,
    ) -> Vec<crate::model::Node> {
        use crate::engine::GraphEngine;
        self.graph
            .nodes_by_kind(kind, limit, offset)
            .unwrap_or_default()
            .into_iter()
            .map(|n| (*n).clone())
            .collect()
    }

    /// Keyword facets over nodes of `kind` — the engine of `GET /facets` and the
    /// engine-agnostic counterpart of [`crate::db::Drevo::facets`]. Keywords are
    /// scored with the native FTS corpus statistics (`doc_count` / `trigram_df`)
    /// rather than the KV backend, so the facet counts match what `fts.search`
    /// would rank; `build_facets`/`node_property_text` are shared with the KV
    /// path.
    ///
    /// # Errors
    /// Propagates a keyword-extraction failure as [`DrevoError`].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn facets(
        &self,
        kind: &str,
        property: &str,
        k: usize,
        collapse: &crate::fts::facet::FacetCollapse<'_>,
    ) -> Result<Vec<crate::fts::facet::Facet>, DrevoError> {
        let nodes = self.list_nodes_by_kind(kind, usize::MAX, 0);
        self.with_fresh_indexes(|idx| {
            let mut per_doc: Vec<(u64, Vec<String>)> = Vec::with_capacity(nodes.len());
            for node in &nodes {
                let Some(text) = crate::fts::facet::node_property_text(node, property) else {
                    continue;
                };
                let keywords = crate::fts::keywords::extract_keywords_scored(
                    &text,
                    k,
                    false,
                    idx.fts.doc_count(),
                    &|term_trigrams| Ok(idx.fts.trigram_df(term_trigrams)),
                )?;
                if !keywords.is_empty() {
                    per_doc.push((node.id, keywords));
                }
            }
            Ok(crate::fts::facet::build_facets(&per_doc, collapse))
        })
    }

    // ----- durable embedding store + HNSW (embedded parity, #446) ------------
    //
    // The native counterpart of the KV `vec:` store + `build_vector_index`.
    // Reads/writes delegate to the durable `NativeGraph` store; the HNSW index
    // is rebuilt on demand from that store via the shared, engine-agnostic
    // `vector_store::build_hnsw_from` (issue #446 S0).

    /// Store (or replace) a node's embedding, durably — parity with
    /// `Drevo::set_embedding`.
    ///
    /// # Errors
    /// [`DrevoError::NodeNotFound`] if the node is absent; propagates a WAL
    /// failure.
    pub fn set_embedding(&self, node_id: u64, embedding: Vec<f32>) -> Result<(), DrevoError> {
        Ok(self.graph.set_embedding(node_id, embedding)?)
    }

    /// Store many embeddings in one durable batch (one fsync), all-or-nothing —
    /// parity with `Drevo::set_embeddings_batch`.
    ///
    /// # Errors
    /// [`DrevoError::NodeNotFound`] for the first absent node (nothing written);
    /// propagates a WAL failure.
    pub fn set_embeddings_batch(&self, embeddings: &[(u64, Vec<f32>)]) -> Result<(), DrevoError> {
        Ok(self.graph.set_embeddings_batch(embeddings)?)
    }

    /// Fetch a node's embedding, or `None` — parity with `Drevo::get_embedding`.
    pub fn get_embedding(&self, node_id: u64) -> Option<Vec<f32>> {
        self.graph.get_embedding(node_id)
    }

    /// Delete a node's embedding (idempotent), durably — parity with
    /// `Drevo::delete_embedding`.
    ///
    /// # Errors
    /// Propagates a WAL failure.
    pub fn delete_embedding(&self, node_id: u64) -> Result<(), DrevoError> {
        Ok(self.graph.delete_embedding(node_id)?)
    }

    /// The number of stored embeddings — parity with `Drevo::embedding_count`.
    pub fn embedding_count(&self) -> usize {
        self.graph.embedding_count()
    }

    /// Liveness probe — parity with `Drevo::health_check`. Touches the store
    /// (node/edge counts) to prove the engine is readable; the durable engine
    /// recovers on open, so there is no separate integrity/recover step.
    ///
    /// # Errors
    /// Infallible today (reads cannot fail); returns `Result` to match the KV
    /// handle's signature so the embedded/Python surface is identical.
    pub fn health_check(&self) -> Result<(), DrevoError> {
        let _ = self.graph.node_count();
        let _ = self.graph.edge_count();
        Ok(())
    }

    // ── Semantic auto-embedding registry (issue #447) ────────────────────────
    //
    // The native home of the KV handle's `Drevo::semantic` / `rel_semantic`
    // control plane. The registry type (`SemanticIndexRegistry`) is pure and
    // reused verbatim; mutations persist to the `semantic.json` sidecar so a
    // registration survives a restart. The query/embed path already runs on
    // native; these are the register/status control-plane methods the executor
    // will resolve native-first (a later slice), replacing the KV `secondary`.

    /// Register (or re-enable) a node-side auto-embedding target — parity with
    /// `Drevo::semantic_register`.
    ///
    /// # Errors
    /// Propagates the registry transition error (e.g. already enabled).
    pub fn semantic_register(
        &self,
        label: &str,
        text_property: &str,
        embedding_property: &str,
        mode: crate::semantic_index::IndexMode,
        model: Option<String>,
    ) -> Result<crate::semantic_index::SemanticIndex, crate::semantic_index::IndexError> {
        let target = {
            let mut reg = self.semantic.write().unwrap_or_else(|e| e.into_inner());
            reg.enable(label, text_property, embedding_property, mode, model)
                .cloned()?
        };
        self.persist_semantic();
        Ok(target)
    }

    /// Register (or re-enable) a relationship-side target — parity with
    /// `Drevo::semantic_register_rel`.
    ///
    /// # Errors
    /// Propagates the registry transition error.
    pub fn semantic_register_rel(
        &self,
        rel_type: &str,
        text_property: &str,
        embedding_property: &str,
        mode: crate::semantic_index::IndexMode,
        model: Option<String>,
    ) -> Result<crate::semantic_index::SemanticIndex, crate::semantic_index::IndexError> {
        let target = {
            let mut reg = self.rel_semantic.write().unwrap_or_else(|e| e.into_inner());
            reg.enable(rel_type, text_property, embedding_property, mode, model)
                .cloned()?
        };
        self.persist_semantic();
        Ok(target)
    }

    /// The registered node-side targets — parity with `Drevo::semantic_status`.
    pub fn semantic_status(&self) -> Vec<crate::semantic_index::SemanticIndex> {
        self.semantic
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .list()
            .to_vec()
    }

    /// The registered relationship-side targets.
    pub fn semantic_status_rel(&self) -> Vec<crate::semantic_index::SemanticIndex> {
        self.rel_semantic
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .list()
            .to_vec()
    }

    /// Detailed status for `drevo.semantic.status` — parity with
    /// `Drevo::semantic_status_detailed` (#263/#266, ported to native in #447).
    ///
    /// For each Auto target it scans the live backlog (`pending` = matching
    /// nodes/edges that carry the text property but no embedding) and reads the
    /// cumulative swallowed-failure tally (`failed` / `last_error`) recorded by
    /// the fail-open auto-embed write path. `degraded` is `pending > 0`, so a
    /// client can tell "fully embedded" from "writes landed, embeddings
    /// missing". Manual targets have no drevo-managed backlog, so they always
    /// read clean.
    pub fn semantic_status_detailed(&self) -> Vec<crate::db::SemanticTargetStatus> {
        let mut out = Vec::new();
        for index in self.semantic_status() {
            out.push(self.status_for("node", index));
        }
        for index in self.semantic_status_rel() {
            out.push(self.status_for("relationship", index));
        }
        out
    }

    /// Build the health row for one target (#447; native mirror of
    /// `Drevo::status_for`). Only Auto targets are drevo-managed, so only they
    /// have a backlog.
    fn status_for(
        &self,
        kind: &'static str,
        index: crate::semantic_index::SemanticIndex,
    ) -> crate::db::SemanticTargetStatus {
        let pending = if matches!(index.mode, crate::semantic_index::IndexMode::Auto) {
            match kind {
                "relationship" => self.semantic_pending_count_rel(
                    &index.label,
                    &index.text_property,
                    &index.embedding_property,
                ),
                _ => self.semantic_pending_count(
                    &index.label,
                    &index.text_property,
                    &index.embedding_property,
                ),
            }
        } else {
            0
        };
        let (failed, last_error) =
            self.embed_failure_stat(kind, &index.label, &index.embedding_property);
        crate::db::SemanticTargetStatus {
            target_kind: kind,
            index,
            pending,
            failed,
            last_error,
            degraded: pending > 0,
        }
    }

    /// Count nodes of `label` that carry a non-empty `text_property` but still
    /// lack `embedding_property` — the live auto-embed backlog (#447; mirror of
    /// `Drevo::semantic_pending_count`). Scans a consistent snapshot.
    fn semantic_pending_count(
        &self,
        label: &str,
        text_property: &str,
        embedding_property: &str,
    ) -> usize {
        let mut pending = 0;
        for node in self.graph.snapshot().all_nodes() {
            let matches_label = node.kind == label
                || matches!(
                    node.properties.0.get("_labels"),
                    Some(serde_json::Value::Array(arr))
                        if arr.iter().any(|v| v.as_str() == Some(label))
                );
            if !matches_label {
                continue;
            }
            let has_text = node
                .properties
                .0
                .get(text_property)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| !s.is_empty());
            let has_embedding = node.properties.0.contains_key(embedding_property);
            if has_text && !has_embedding {
                pending += 1;
            }
        }
        pending
    }

    /// Relationship mirror of [`Self::semantic_pending_count`] (#447): edges of
    /// `rel_type` with a non-empty `text_property` but no `embedding_property`.
    fn semantic_pending_count_rel(
        &self,
        rel_type: &str,
        text_property: &str,
        embedding_property: &str,
    ) -> usize {
        let mut pending = 0;
        for edge in self.graph.snapshot().all_edges() {
            if edge.kind != rel_type {
                continue;
            }
            let has_text = edge
                .properties
                .0
                .get(text_property)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| !s.is_empty());
            let has_embedding = edge.properties.0.contains_key(embedding_property);
            if has_text && !has_embedding {
                pending += 1;
            }
        }
        pending
    }

    /// Record a swallowed auto-embed failure for a target (#447; mirror of
    /// `Drevo::record_embed_failure`), keyed by `kind` so a node label and a
    /// relationship type of the same name never collide.
    #[cfg(feature = "http")]
    fn record_embed_failure(&self, kind: &str, name: &str, embedding_property: &str, error: &str) {
        let mut map = self
            .embed_failures
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let stat = map
            .entry((
                kind.to_string(),
                name.to_string(),
                embedding_property.to_string(),
            ))
            .or_default();
        stat.count += 1;
        stat.last_error = error.to_string();
    }

    /// Read the cumulative failure count and most-recent error for a target
    /// (#447). `(0, None)` when nothing has failed — and always so on a build
    /// without `http` (no embedder to fail).
    fn embed_failure_stat(
        &self,
        kind: &str,
        name: &str,
        embedding_property: &str,
    ) -> (u64, Option<String>) {
        #[cfg(feature = "http")]
        {
            let map = self
                .embed_failures
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match map.get(&(
                kind.to_string(),
                name.to_string(),
                embedding_property.to_string(),
            )) {
                Some(stat) if stat.count > 0 => (stat.count, Some(stat.last_error.clone())),
                _ => (0, None),
            }
        }
        #[cfg(not(feature = "http"))]
        {
            let _ = (kind, name, embedding_property);
            (0, None)
        }
    }

    /// Apply server-side auto-embedding to a **node's** properties just before
    /// it is persisted (#447; native mirror of `Drevo::apply_auto_embeddings`).
    ///
    /// For every registered [`crate::semantic_index::IndexMode::Auto`] target
    /// whose label matches this node (primary `kind` or a `_labels` secondary
    /// label), embed the text in the target's `text_property` and write the
    /// vector into its `embedding_property`. A double no-op keeps the common
    /// path free: it returns immediately when no embedder is installed and when
    /// no Auto target matches. `old` is the pre-patch property map on update; a
    /// target whose source text is unchanged and whose embedding is already
    /// present is skipped. An upstream failure is logged, tallied, and swallowed
    /// — a transient embedder outage must never fail a write.
    #[cfg(feature = "http")]
    pub fn apply_auto_embeddings(
        &self,
        kind: &str,
        properties: &mut crate::model::Properties,
        old: Option<&crate::model::Properties>,
    ) {
        let Some(embedder) = self.embedder.get() else {
            return;
        };
        let targets: Vec<(String, String, String)> = {
            let reg = self.semantic.read().unwrap_or_else(|e| e.into_inner());
            reg.list()
                .iter()
                .filter(|t| matches!(t.mode, crate::semantic_index::IndexMode::Auto))
                .map(|t| {
                    (
                        t.label.clone(),
                        t.text_property.clone(),
                        t.embedding_property.clone(),
                    )
                })
                .collect()
        };
        if targets.is_empty() {
            return;
        }
        let mut labels = vec![kind.to_string()];
        if let Some(serde_json::Value::Array(arr)) = properties.0.get("_labels") {
            for item in arr {
                if let serde_json::Value::String(s) = item {
                    if !labels.iter().any(|l| l == s) {
                        labels.push(s.clone());
                    }
                }
            }
        }
        for (label, text_prop, emb_prop) in targets {
            if !labels.iter().any(|l| l == &label) {
                continue;
            }
            let Some(text) = properties
                .0
                .get(&text_prop)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            if let Some(old) = old {
                let unchanged = old.0.get(&text_prop).and_then(serde_json::Value::as_str)
                    == Some(text.as_str());
                if unchanged && properties.0.contains_key(&emb_prop) {
                    continue;
                }
            }
            match embedder.embed_query(&text) {
                Ok(vector) => {
                    let arr = serde_json::Value::Array(
                        vector.into_iter().map(|f| serde_json::json!(f)).collect(),
                    );
                    properties.0.insert(emb_prop, arr);
                }
                Err(error) => {
                    tracing::warn!(label = %label, %error, "native auto-embed failed (ignored)");
                    self.record_embed_failure("node", &label, &emb_prop, &error.to_string());
                }
            }
        }
    }

    /// No-op stand-in on a build without `http` (no embedder is compiled in).
    #[cfg(not(feature = "http"))]
    pub fn apply_auto_embeddings(
        &self,
        kind: &str,
        properties: &mut crate::model::Properties,
        old: Option<&crate::model::Properties>,
    ) {
        let _ = (kind, properties, old);
    }

    /// Relationship mirror of [`Self::apply_auto_embeddings`] (#447): apply
    /// auto-embedding to an **edge's** properties before it is persisted.
    /// `rel_type` is the edge's `kind`; matching Auto-mode targets in the
    /// relationship registry embed `text_property` into `embedding_property`.
    #[cfg(feature = "http")]
    pub fn apply_auto_embeddings_edge(
        &self,
        rel_type: &str,
        properties: &mut crate::model::Properties,
        old: Option<&crate::model::Properties>,
    ) {
        let Some(embedder) = self.embedder.get() else {
            return;
        };
        let targets: Vec<(String, String, String)> = {
            let reg = self.rel_semantic.read().unwrap_or_else(|e| e.into_inner());
            reg.list()
                .iter()
                .filter(|t| {
                    matches!(t.mode, crate::semantic_index::IndexMode::Auto) && t.label == rel_type
                })
                .map(|t| {
                    (
                        t.label.clone(),
                        t.text_property.clone(),
                        t.embedding_property.clone(),
                    )
                })
                .collect()
        };
        for (rel, text_prop, emb_prop) in targets {
            let Some(text) = properties
                .0
                .get(&text_prop)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            if let Some(old) = old {
                let unchanged = old.0.get(&text_prop).and_then(serde_json::Value::as_str)
                    == Some(text.as_str());
                if unchanged && properties.0.contains_key(&emb_prop) {
                    continue;
                }
            }
            match embedder.embed_query(&text) {
                Ok(vector) => {
                    let arr = serde_json::Value::Array(
                        vector.into_iter().map(|f| serde_json::json!(f)).collect(),
                    );
                    properties.0.insert(emb_prop, arr);
                }
                Err(error) => {
                    tracing::warn!(rel_type = %rel, %error, "native auto-embed (rel) failed (ignored)");
                    self.record_embed_failure("relationship", &rel, &emb_prop, &error.to_string());
                }
            }
        }
    }

    /// No-op stand-in on a build without `http`.
    #[cfg(not(feature = "http"))]
    pub fn apply_auto_embeddings_edge(
        &self,
        rel_type: &str,
        properties: &mut crate::model::Properties,
        old: Option<&crate::model::Properties>,
    ) {
        let _ = (rel_type, properties, old);
    }

    /// Backfill embeddings for existing nodes of `label` — parity with
    /// `Drevo::semantic_reindex` (#262). Scans the label's nodes, embeds
    /// `text_property` into `embedding_property` (a node property) for up to
    /// `batch_size` that still lack it, and returns resumable counts. Idempotent
    /// (already-embedded nodes are skipped); fail-open (an embed failure leaves
    /// the node for a later pass). Needs the server-side embedder, so it is
    /// gated on `http`.
    ///
    /// # Errors
    /// Propagates a node-update failure from the graph.
    #[cfg(feature = "http")]
    pub fn semantic_reindex(
        &self,
        label: &str,
        text_property: &str,
        embedding_property: &str,
        batch_size: usize,
    ) -> Result<crate::db::SemanticReindexReport, DrevoError> {
        use crate::engine::GraphEngine;
        let mut report = crate::db::SemanticReindexReport::default();
        let mut budget = batch_size;
        // One consistent snapshot to scan; writes below mutate the live graph.
        for node in self.graph.snapshot().all_nodes() {
            // Match the primary kind plus any secondary `_labels` (same as KV).
            let matches_label = node.kind == label
                || matches!(
                    node.properties.0.get("_labels"),
                    Some(serde_json::Value::Array(arr))
                        if arr.iter().any(|v| v.as_str() == Some(label))
                );
            if !matches_label {
                continue;
            }
            report.scanned += 1;
            let text = node
                .properties
                .0
                .get(text_property)
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let already_embedded = node.properties.0.contains_key(embedding_property);
            let Some(text) = text.filter(|_| !already_embedded) else {
                report.skipped += 1;
                continue;
            };
            if budget == 0 {
                report.remaining += 1;
                continue;
            }
            let Some(embedder) = self.embedder.get() else {
                report.remaining += 1;
                continue;
            };
            match embedder.embed_query(&text) {
                Ok(vector) => {
                    let mut props = node.properties.clone();
                    props.0.insert(
                        embedding_property.to_string(),
                        serde_json::Value::Array(
                            vector.into_iter().map(|f| serde_json::json!(f)).collect(),
                        ),
                    );
                    let patch = crate::model::NodePatch {
                        properties: Some(props),
                        ..Default::default()
                    };
                    self.graph.update_node(node.id, patch)?;
                    report.embedded += 1;
                    budget -= 1;
                }
                Err(error) => {
                    tracing::warn!(label, error = %error, "native reindex embed failed (ignored)");
                    report.remaining += 1;
                }
            }
        }
        Ok(report)
    }

    /// Backfill embeddings for existing **edges** of `rel_type` — the edge
    /// mirror of [`Self::semantic_reindex`], parity with
    /// `Drevo::semantic_reindex_rel` (#266). Same idempotent / resumable /
    /// fail-open semantics.
    ///
    /// # Errors
    /// Propagates an edge-update failure from the graph.
    #[cfg(feature = "http")]
    pub fn semantic_reindex_rel(
        &self,
        rel_type: &str,
        text_property: &str,
        embedding_property: &str,
        batch_size: usize,
    ) -> Result<crate::db::SemanticReindexReport, DrevoError> {
        use crate::engine::GraphEngine;
        let mut report = crate::db::SemanticReindexReport::default();
        let mut budget = batch_size;
        for edge in self.graph.snapshot().all_edges() {
            if edge.kind != rel_type {
                continue;
            }
            report.scanned += 1;
            let text = edge
                .properties
                .0
                .get(text_property)
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let already_embedded = edge.properties.0.contains_key(embedding_property);
            let Some(text) = text.filter(|_| !already_embedded) else {
                report.skipped += 1;
                continue;
            };
            if budget == 0 {
                report.remaining += 1;
                continue;
            }
            let Some(embedder) = self.embedder.get() else {
                report.remaining += 1;
                continue;
            };
            match embedder.embed_query(&text) {
                Ok(vector) => {
                    let mut props = edge.properties.clone();
                    props.0.insert(
                        embedding_property.to_string(),
                        serde_json::Value::Array(
                            vector.into_iter().map(|f| serde_json::json!(f)).collect(),
                        ),
                    );
                    let patch = crate::model::EdgePatch {
                        properties: Some(props),
                        ..Default::default()
                    };
                    self.graph.update_edge(edge.id, patch)?;
                    report.embedded += 1;
                    budget -= 1;
                }
                Err(error) => {
                    tracing::warn!(rel_type, error = %error, "native reindex (rel) embed failed (ignored)");
                    report.remaining += 1;
                }
            }
        }
        Ok(report)
    }

    /// Atomically rewrite the `semantic.json` sidecar with both registries.
    /// Best-effort (a storage hiccup must not fail a control-plane call); a
    /// no-op for an in-memory service. Mirrors the KV `persist_semantic_registry`.
    fn persist_semantic(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(path) = self.semantic_sidecar.as_ref() else {
                return;
            };
            let sidecar = SemanticSidecar {
                node: self
                    .semantic
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                rel: self
                    .rel_semantic
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            };
            let Ok(bytes) = serde_json::to_vec(&sidecar) else {
                return;
            };
            let tmp = path.with_extension("json.tmp");
            let _ = std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, path));
        }
    }

    /// Rebuild an in-memory HNSW index over every stored embedding — parity
    /// with `Drevo::build_vector_index`. The index is not persisted (the
    /// embeddings are); rebuild after reopen.
    ///
    /// # Errors
    /// [`DrevoError::Vector`] if a stored embedding cannot be inserted (e.g. a
    /// dimension mismatch against the first).
    pub fn build_vector_index(
        &self,
        config: crate::vector::HnswConfig,
    ) -> Result<crate::vector::HnswIndex, DrevoError> {
        crate::vector::store::build_hnsw_from(
            self.graph
                .all_embeddings()
                .into_iter()
                .map(|(id, v)| (id, crate::vector::Vector::from(v))),
            config,
        )
    }

    /// k-nearest embeddings to `query` by the default (cosine) metric —
    /// `(node_id, distance)`, nearest first. Parity with the Python handle's
    /// `vector_search`: rebuilds the HNSW index, then searches.
    ///
    /// # Errors
    /// [`DrevoError::Vector`] on a dimension mismatch (query vs stored vectors).
    pub fn vector_search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>, DrevoError> {
        let index = self.build_vector_index(crate::vector::HnswConfig::default())?;
        Ok(index
            .search(query, k)?
            .into_iter()
            .map(|n| (n.key, n.distance))
            .collect())
    }

    // ----- traversal (embedded parity with the KV `Drevo` handle, #445) ------
    //
    // These reuse the same engine-agnostic `crate::traversal` algorithms the KV
    // handle does, driven by closures over the native graph — so BFS/DFS visit
    // order, depth semantics (start node excluded), and Dijkstra weighting are
    // identical on both engines. Native reads are infallible, hence the `Ok(..)`
    // wrap into the traversal closures' `Result` contract.

    /// Breadth-first search from `start_id` (start excluded), depth-capped —
    /// parity with `Drevo::bfs`.
    pub fn bfs(
        &self,
        start_id: u64,
        max_depth: u8,
        direction: crate::model::Direction,
        edge_kind: Option<&str>,
    ) -> Result<Vec<crate::model::Node>, DrevoError> {
        let snap = self.graph.snapshot();
        crate::traversal::bfs(
            start_id,
            max_depth,
            direction,
            edge_kind,
            &|id| Ok(snap.get_node(id)),
            &|id, dir| Ok(snap.edges_of(id, dir)),
        )
    }

    /// Depth-first search from `start_id` (start excluded), depth-capped —
    /// parity with `Drevo::dfs`.
    pub fn dfs(
        &self,
        start_id: u64,
        max_depth: u8,
        direction: crate::model::Direction,
        edge_kind: Option<&str>,
    ) -> Result<Vec<crate::model::Node>, DrevoError> {
        let snap = self.graph.snapshot();
        crate::traversal::dfs(
            start_id,
            max_depth,
            direction,
            edge_kind,
            &|id| Ok(snap.get_node(id)),
            &|id, dir| Ok(snap.edges_of(id, dir)),
        )
    }

    /// Lowest-total-weight path `from → to` over outgoing edges (Dijkstra) —
    /// parity with `Drevo::shortest_path`. `None` if unreachable.
    pub fn shortest_path(&self, from: u64, to: u64) -> Result<Option<Vec<u64>>, DrevoError> {
        self.shortest_path_filtered(from, to, None)
    }

    /// [`Self::shortest_path`] restricted to edges of `edge_kind` when `Some` —
    /// parity with `Drevo::shortest_path_filtered`.
    pub fn shortest_path_filtered(
        &self,
        from: u64,
        to: u64,
        edge_kind: Option<&str>,
    ) -> Result<Option<Vec<u64>>, DrevoError> {
        let snap = self.graph.snapshot();
        crate::traversal::shortest_path(
            from,
            to,
            edge_kind,
            &|id| Ok(snap.get_node(id)),
            &|id, dir| Ok(snap.edges_of(id, dir)),
        )
    }

    /// The connected subgraph within `depth` hops of `root` — parity with
    /// `Drevo::subgraph_filtered`.
    pub fn subgraph_filtered(
        &self,
        root: u64,
        depth: u8,
        edge_kind: Option<&str>,
    ) -> Result<crate::model::SubGraph, DrevoError> {
        let snap = self.graph.snapshot();
        crate::traversal::subgraph(
            root,
            depth,
            edge_kind,
            &|id| Ok(snap.get_node(id)),
            &|id, dir| Ok(snap.edges_of(id, dir)),
        )
    }

    /// Immediate neighbours of `node_id` (BFS depth 1) — parity with
    /// `Drevo::neighbors`; served straight from the native adjacency index.
    pub fn neighbors(
        &self,
        node_id: u64,
        direction: crate::model::Direction,
        kind: Option<&str>,
    ) -> Vec<crate::model::Node> {
        self.graph.snapshot().neighbors(node_id, direction, kind)
    }

    /// Edges incident to `node_id` in `direction` — the engine of
    /// `GET /nodes/{id}/edges`, parity with `Drevo::edges_of`. A missing node
    /// yields an empty list (the KV route does the same — it does not 404).
    pub fn edges_of(
        &self,
        node_id: u64,
        direction: crate::model::Direction,
    ) -> Vec<crate::model::Edge> {
        self.graph.snapshot().edges_of(node_id, direction)
    }

    /// Import a `drevo-json-v1` dump into the durable store — the engine of
    /// `POST /import/json`, parity with `Drevo::import_json`. Parses and
    /// validates the format header, then replays the records through the WAL
    /// (durable, idempotent for drevo's own exports).
    ///
    /// # Errors
    /// [`DrevoError::Io`] on malformed JSON or an unknown format; propagates a
    /// title collision / WAL failure.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn import_json(&self, raw: &str) -> Result<crate::dump::ImportReport, DrevoError> {
        use crate::engine::GraphEngine;
        let dump: crate::dump::Dump = serde_json::from_str(raw)
            .map_err(|e| DrevoError::Io(std::io::Error::other(e.to_string())))?;
        if dump.format != crate::dump::FORMAT_V1 {
            return Err(DrevoError::Io(std::io::Error::other(format!(
                "unsupported dump format: {:?} — expected {}",
                dump.format,
                crate::dump::FORMAT_V1
            ))));
        }
        Ok(self.graph.apply_dump(dump)?)
    }

    fn execute_with(
        &self,
        idx: &ServiceIndexes,
        query: &Query,
        params: HashMap<String, Value>,
    ) -> Result<ExecResult, ExecError> {
        let ctx = NativeQueryContext {
            fts: Some(&idx.fts),
            fts_rel: Some(&idx.fts_rel),
            labels: Some(&idx.labels),
            properties: Some(&idx.props),
            values: Some(&idx.values),
            #[cfg(feature = "http")]
            embedder: self.embedder.get(),
            #[cfg(not(feature = "http"))]
            embedder: None,
            // The native semantic control plane — `semantic.register/status`
            // manage this service's registry instead of a KV secondary (#447).
            semantic: Some(self),
        };
        execute_on_engine_with_context(query, &self.graph, &ctx, params)
    }
    /// Export the whole graph as a GraphML 1.0 document — the same wire
    /// format (and renderer) as the KV server's `GET /export/graphml`, so a
    /// durable-native backup is interchangeable with a KV one.
    ///
    /// # Errors
    ///
    /// Propagates [`crate::error::DrevoError`] from the scan or a
    /// non-serialisable property value.
    pub fn export_graphml(&self) -> Result<String, DrevoError> {
        use crate::engine::GraphEngine;
        let nodes: Vec<crate::model::Node> = GraphEngine::all_nodes(&self.graph)?
            .iter()
            .map(|n| (**n).clone())
            .collect();
        let edges = GraphEngine::all_edges(&self.graph)?;
        crate::dump::render_graphml(&nodes, &edges)
    }

    /// Import a GraphML document (drevo's own export, or any GraphML
    /// following the same conventions) into the durable graph — the
    /// engine-generic inverse of [`Self::export_graphml`]. Applied through
    /// the engine's dump cycle, so everything lands in the WAL as one
    /// fsynced atomic batch and the indexes catch up on the next statement.
    /// Re-importing an own export is idempotent (identical rows are
    /// skipped).
    ///
    /// # Errors
    ///
    /// Propagates parse failures ([`crate::dump::DumpError`] lifted into
    /// [`crate::error::DrevoError`]) and id-collision conflicts, exactly as
    /// the KV import reports them.
    pub fn import_graphml(&self, xml: &str) -> Result<crate::dump::ImportReport, DrevoError> {
        use crate::engine::GraphEngine;
        let all = GraphEngine::all_nodes(&self.graph)?;
        let db_max_node = all.iter().map(|n| n.id).max().unwrap_or(0);
        let db_max_edge = GraphEngine::all_edges(&self.graph)?
            .iter()
            .map(|e| e.id)
            .max()
            .unwrap_or(0);
        let (nodes, edges) = crate::dump::graphml_records(xml, db_max_node, db_max_edge)?;
        let next_node_id = nodes.iter().map(|n| n.id).max().map_or(1, |m| m + 1);
        let next_edge_id = edges.iter().map(|e| e.id).max().map_or(1, |m| m + 1);
        let report = self.graph.apply_dump(crate::dump::Dump {
            format: crate::dump::FORMAT_V1.to_string(),
            exported_at: crate::model::now_ms(),
            next_node_id,
            next_edge_id,
            nodes,
            edges,
        })?;
        Ok(report)
    }
}

#[cfg(test)]
mod traversal_parity_tests {
    //! Embedded-parity traversal on native (#445): the BFS/DFS/shortest-path/
    //! subgraph/neighbours wrappers drive the same `crate::traversal` algorithms
    //! the KV `Drevo` handle uses, over a native snapshot.
    use super::NativeService;
    use crate::engine::GraphEngine;
    use crate::model::{Direction, NewEdge, NewNode, Properties};

    fn nn(title: &str) -> NewNode {
        NewNode {
            kind: "n".into(),
            title: title.into(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties(Default::default()),
        }
    }
    fn ne(from_id: u64, to_id: u64, kind: &str) -> NewEdge {
        NewEdge {
            from_id,
            to_id,
            kind: kind.into(),
            weight: 1.0,
            properties: Properties(Default::default()),
        }
    }
    fn titles(nodes: &[crate::model::Node]) -> Vec<String> {
        let mut t: Vec<String> = nodes.iter().map(|n| n.title.clone()).collect();
        t.sort();
        t
    }

    /// a -link-> b -link-> c ; a -other-> d
    fn fixture() -> (NativeService, u64, u64, u64, u64) {
        let svc = NativeService::in_memory();
        let g = svc.graph();
        let a = g.create_node(nn("a")).unwrap().id;
        let b = g.create_node(nn("b")).unwrap().id;
        let c = g.create_node(nn("c")).unwrap().id;
        let d = g.create_node(nn("d")).unwrap().id;
        g.create_edge(ne(a, b, "link")).unwrap();
        g.create_edge(ne(b, c, "link")).unwrap();
        g.create_edge(ne(a, d, "other")).unwrap();
        (svc, a, b, c, d)
    }

    #[test]
    fn bfs_depth_and_kind_filter() {
        let (svc, a, _b, _c, _d) = fixture();
        // depth 2, all kinds → b, c, d (start excluded)
        let all = svc.bfs(a, 2, Direction::Outgoing, None).unwrap();
        assert_eq!(titles(&all), vec!["b", "c", "d"]);
        // kind-filtered to "link" → only the b, c chain
        let links = svc.bfs(a, 2, Direction::Outgoing, Some("link")).unwrap();
        assert_eq!(titles(&links), vec!["b", "c"]);
        // depth 1 stops at direct neighbours
        let d1 = svc.bfs(a, 1, Direction::Outgoing, Some("link")).unwrap();
        assert_eq!(titles(&d1), vec!["b"]);
    }

    #[test]
    fn dfs_reaches_the_same_set() {
        let (svc, a, _b, _c, _d) = fixture();
        let out = svc.dfs(a, 2, Direction::Outgoing, Some("link")).unwrap();
        assert_eq!(titles(&out), vec!["b", "c"]);
    }

    #[test]
    fn shortest_path_follows_edges() {
        let (svc, a, b, c, d) = fixture();
        // a -link-> b -link-> c  → path [a, b, c]
        assert_eq!(svc.shortest_path(a, c).unwrap(), Some(vec![a, b, c]));
        // within the "link" subgraph, d (only reachable via "other") is unreachable
        assert_eq!(
            svc.shortest_path_filtered(a, d, Some("link")).unwrap(),
            None
        );
    }

    #[test]
    fn neighbors_and_subgraph() {
        let (svc, a, _b, _c, _d) = fixture();
        let neigh = svc.neighbors(a, Direction::Outgoing, None);
        assert_eq!(titles(&neigh), vec!["b", "d"]);
        let only_link = svc.neighbors(a, Direction::Outgoing, Some("link"));
        assert_eq!(titles(&only_link), vec!["b"]);
        // subgraph within 2 hops over "link": a, b, c
        let sg = svc.subgraph_filtered(a, 2, Some("link")).unwrap();
        assert_eq!(titles(&sg.nodes), vec!["a", "b", "c"]);
    }
}

#[cfg(test)]
mod crud_parity_tests {
    //! The inherent CRUD wrappers (`update_node`/`delete_node`/`get_edge`/
    //! `update_edge`/`delete_edge`/`subgraph`) added for embedded-handle parity
    //! with `Drevo` (epic #444) — the surface the C-FFI and WASM bindings drive.
    use super::NativeService;
    use crate::error::DrevoError;
    use crate::model::{EdgePatch, NewEdge, NewNode, NodePatch, Properties};

    fn nn(title: &str) -> NewNode {
        NewNode {
            kind: "n".into(),
            title: title.into(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties(Default::default()),
        }
    }

    #[test]
    fn update_node_changes_fields_and_reports_missing() {
        let svc = NativeService::in_memory();
        let id = svc.create_node(nn("orig")).unwrap().id;
        let patch = NodePatch {
            title: Some("renamed".into()),
            ..Default::default()
        };
        let updated = svc.update_node(id, patch).unwrap();
        assert_eq!(updated.title, "renamed");
        assert_eq!(svc.get_node(id).unwrap().title, "renamed");
        // A patch against a missing id surfaces NodeNotFound.
        let miss = svc.update_node(9999, NodePatch::default());
        assert!(matches!(miss, Err(DrevoError::NodeNotFound(9999))));
    }

    #[test]
    fn delete_node_removes_it() {
        let svc = NativeService::in_memory();
        let id = svc.create_node(nn("gone")).unwrap().id;
        svc.delete_node(id).unwrap();
        assert!(matches!(svc.get_node(id), Err(DrevoError::NodeNotFound(_))));
        // Deleting a missing node reports NodeNotFound.
        assert!(matches!(
            svc.delete_node(id),
            Err(DrevoError::NodeNotFound(_))
        ));
    }

    #[test]
    fn edge_get_update_delete_roundtrip() {
        let svc = NativeService::in_memory();
        let a = svc.create_node(nn("a")).unwrap().id;
        let b = svc.create_node(nn("b")).unwrap().id;
        let e = svc
            .create_edge(NewEdge {
                from_id: a,
                to_id: b,
                kind: "link".into(),
                weight: 1.0,
                properties: Properties(Default::default()),
            })
            .unwrap();
        // get_edge returns the stored edge; a missing id is Ok(None).
        assert_eq!(svc.get_edge(e.id).unwrap().unwrap().kind, "link");
        assert!(svc.get_edge(9999).unwrap().is_none());
        // update_edge applies a patch.
        let updated = svc
            .update_edge(
                e.id,
                EdgePatch {
                    weight: Some(2.5),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.weight, 2.5);
        // delete_edge removes it; a second delete reports EdgeNotFound.
        svc.delete_edge(e.id).unwrap();
        assert!(svc.get_edge(e.id).unwrap().is_none());
        assert!(matches!(
            svc.delete_edge(e.id),
            Err(DrevoError::EdgeNotFound(_))
        ));
    }

    #[test]
    fn subgraph_extracts_the_local_neighbourhood() {
        let svc = NativeService::in_memory();
        let a = svc.create_node(nn("a")).unwrap().id;
        let b = svc.create_node(nn("b")).unwrap().id;
        let c = svc.create_node(nn("c")).unwrap().id;
        svc.create_edge(NewEdge {
            from_id: a,
            to_id: b,
            kind: "link".into(),
            weight: 1.0,
            properties: Properties(Default::default()),
        })
        .unwrap();
        svc.create_edge(NewEdge {
            from_id: b,
            to_id: c,
            kind: "link".into(),
            weight: 1.0,
            properties: Properties(Default::default()),
        })
        .unwrap();
        // depth 1 from a reaches b (and the a→b edge), not c.
        let sub = svc.subgraph(a, 1).unwrap();
        let ids: std::collections::BTreeSet<u64> = sub.nodes.iter().map(|n| n.id).collect();
        assert!(ids.contains(&a) && ids.contains(&b));
        assert!(!ids.contains(&c), "depth-1 subgraph must not reach c");
    }
}

#[cfg(test)]
mod embedding_store_tests {
    //! Native embedding store + HNSW on the service layer (#446): set/get/
    //! delete/count/batch and `build_vector_index`/`vector_search`.
    use super::NativeService;
    use crate::engine::GraphEngine;
    use crate::model::{NewNode, Properties};

    fn nn(title: &str) -> NewNode {
        NewNode {
            kind: "doc".into(),
            title: title.into(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties(Default::default()),
        }
    }

    #[test]
    fn store_roundtrip_and_node_validation() {
        let svc = NativeService::in_memory();
        let a = svc.graph().create_node(nn("a")).unwrap().id;

        svc.set_embedding(a, vec![1.0, 0.0, 0.0]).unwrap();
        assert_eq!(svc.embedding_count(), 1);
        assert_eq!(svc.get_embedding(a), Some(vec![1.0, 0.0, 0.0]));
        svc.delete_embedding(a).unwrap();
        assert_eq!(svc.get_embedding(a), None);

        // set on a missing node is an error surfaced as DrevoError.
        assert!(svc.set_embedding(999, vec![1.0]).is_err());
    }

    #[test]
    fn vector_search_finds_nearest() {
        let svc = NativeService::in_memory();
        let a = svc.graph().create_node(nn("a")).unwrap().id;
        let b = svc.graph().create_node(nn("b")).unwrap().id;
        let c = svc.graph().create_node(nn("c")).unwrap().id;
        svc.set_embeddings_batch(&[
            (a, vec![1.0, 0.0, 0.0]),
            (b, vec![0.0, 1.0, 0.0]),
            (c, vec![0.0, 0.0, 1.0]),
        ])
        .unwrap();
        assert_eq!(svc.embedding_count(), 3);

        let hits = svc.vector_search(&[0.9, 0.1, 0.0], 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, a, "nearest to [0.9,0.1,0] is a=[1,0,0]");
    }
}

#[cfg(test)]
mod health_and_batch_tests {
    //! Service-level `health_check` + batch create (#446 S3 prerequisite).
    use super::NativeService;
    use crate::model::{NewNode, Properties};

    fn nn(title: &str) -> NewNode {
        NewNode {
            kind: "doc".into(),
            title: title.into(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties(Default::default()),
        }
    }

    #[test]
    fn health_check_ok_on_a_live_service() {
        let svc = NativeService::in_memory();
        svc.health_check().unwrap();
        svc.graph().create_nodes(vec![nn("a"), nn("b")]).unwrap();
        svc.health_check().unwrap();
    }

    #[test]
    fn create_nodes_stores_the_whole_batch() {
        // The service-level batch create (auto-embed-aware parity with
        // `Drevo::create_nodes`) writes every node and hands back the stored
        // rows with generated ids. Without a configured embedder the auto-embed
        // step is a fail-open no-op, so this exercises the storage path itself.
        let svc = NativeService::in_memory();
        let stored = svc.create_nodes(vec![nn("a"), nn("b"), nn("c")]).unwrap();
        assert_eq!(stored.len(), 3);
        assert!(stored.iter().all(|n| n.id != 0), "ids are assigned");
        assert!(svc.get_node_by_title("a").is_some());
        assert!(svc.get_node_by_title("c").is_some());
    }

    #[test]
    fn create_nodes_is_atomic_on_duplicate_title() {
        // A duplicate title fails the whole batch — nothing is written, matching
        // `NativeGraph::create_nodes`' all-or-nothing contract.
        let svc = NativeService::in_memory();
        svc.create_node(nn("dup")).unwrap();
        assert!(svc.create_nodes(vec![nn("fresh"), nn("dup")]).is_err());
        assert!(
            svc.get_node_by_title("fresh").is_none(),
            "the batch must not have written the first node"
        );
    }
}

#[cfg(test)]
mod semantic_registry_tests {
    //! Native semantic registry (#447): register/status + sidecar durability.
    use super::NativeService;
    use crate::semantic_index::IndexMode;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn tmp_wal() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "drevo_sem_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("native.wal")
    }

    #[test]
    fn register_and_status_in_memory() {
        let svc = NativeService::in_memory();
        assert!(svc.semantic_status().is_empty());
        svc.semantic_register("Doc", "text", "embedding", IndexMode::Manual, None)
            .unwrap();
        svc.semantic_register_rel("MENTIONS", "note", "vec", IndexMode::Auto, None)
            .unwrap();
        let node = svc.semantic_status();
        assert_eq!(node.len(), 1);
        assert_eq!(node[0].label, "Doc");
        assert_eq!(svc.semantic_status_rel().len(), 1);
        // In-memory has no sidecar, so this is a no-op (must not panic).
    }

    #[test]
    fn registration_survives_reopen_via_sidecar() {
        let wal = tmp_wal();
        {
            let svc = NativeService::open(&wal).unwrap();
            svc.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
                .unwrap();
        }
        // A fresh service over the same WAL dir reloads the registry from
        // semantic.json next to the WAL.
        let svc = NativeService::open(&wal).unwrap();
        let targets = svc.semantic_status();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "Doc");
        assert_eq!(targets[0].mode, IndexMode::Auto);
        let _ = std::fs::remove_dir_all(wal.parent().unwrap());
    }
}
