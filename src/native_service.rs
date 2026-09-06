//! Durable-native serving layer (RFC `docs/rfc-native-core.md` #307,
//! Phase 4/7 — the track toward retiring redb).
//!
//! Where the read mirror ([`crate::native_mirror::NativeMirror`]) accelerates
//! reads *beside* a KV store of record, [`crate::native_service::NativeService`]
//! IS the store of record: a WAL-backed [`crate::native::NativeGraph`]
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
use crate::native_fts::NativeFtsIndex;
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
        let graph = NativeGraph::open_durable(path)?;
        graph.compact_wal()?;
        let indexes = RwLock::new(ServiceIndexes::synced_over(&graph));
        let last_compact_head = AtomicU64::new(graph.change_head());
        Ok(Self {
            graph,
            indexes,
            #[cfg(feature = "http")]
            embedder: std::sync::OnceLock::new(),
            compact_every_ops,
            last_compact_head,
            compacting: AtomicBool::new(false),
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

    fn execute_with(
        &self,
        idx: &ServiceIndexes,
        query: &Query,
        params: HashMap<String, Value>,
    ) -> Result<ExecResult, ExecError> {
        let ctx = NativeQueryContext {
            fts: Some(&idx.fts),
            labels: Some(&idx.labels),
            properties: Some(&idx.props),
            values: Some(&idx.values),
            #[cfg(feature = "http")]
            embedder: self.embedder.get(),
            #[cfg(not(feature = "http"))]
            embedder: None,
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

/// First-boot migration into the durable-native store: when `wal` does not
/// exist but the KV redb file at `redb` does, copy the whole graph into a
/// fresh durable WAL through the engine-independent dump cycle
/// ([`crate::migrate::migrate`]) — the same byte-exact path the read mirror
/// builds from — and report what moved. The redb file is never modified, an
/// existing WAL is never touched (the migration runs at most once), and
/// `Ok(None)` means there was nothing to do (already migrated, or no KV
/// data).
///
/// # Errors
///
/// Propagates [`crate::error::DrevoError`] from opening either store or from
/// the dump cycle; on error the WAL may exist partially and should be
/// removed before retrying.
#[cfg(all(not(target_arch = "wasm32"), feature = "redb-backend"))]
pub fn migrate_kv_into_wal_if_first_boot(
    redb: &std::path::Path,
    wal: &std::path::Path,
) -> Result<Option<crate::dump::ImportReport>, DrevoError> {
    if wal.exists() || !redb.exists() {
        return Ok(None);
    }
    let kv = crate::db::Drevo::open(redb)?;
    let native = crate::native::NativeGraph::open_durable(wal)?;
    let report = crate::migrate::migrate(&kv, &native)?;
    Ok(Some(report))
}
