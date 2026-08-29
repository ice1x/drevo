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
use std::sync::RwLock;

use crate::cypher::ast::Query;
use crate::cypher::executor::{
    execute_on_engine_with_indexes_and_values, ExecError, ExecResult, Value,
};
use crate::error::DrevoError;
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
        let graph = NativeGraph::open_durable(path)?;
        graph.compact_wal()?;
        let indexes = RwLock::new(ServiceIndexes::synced_over(&graph));
        Ok(Self { graph, indexes })
    }

    /// An ephemeral (non-durable) service — the same serving stack over an
    /// in-memory graph, for tests and embedded use.
    pub fn in_memory() -> Self {
        let graph = NativeGraph::new();
        let indexes = RwLock::new(ServiceIndexes::synced_over(&graph));
        Self { graph, indexes }
    }

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
        {
            let idx = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            if idx.synced_head == self.graph.change_head() {
                return self.execute_with(&idx, query, params);
            }
        }
        let mut idx = self.indexes.write().unwrap_or_else(|e| e.into_inner());
        if idx.synced_head != self.graph.change_head() {
            idx.catch_up(&self.graph);
        }
        // Executing under the exclusive guard is deliberate: it only happens
        // for the first statement after a write, and it keeps the sync +
        // serve pair simple (std's RwLock cannot downgrade).
        self.execute_with(&idx, query, params)
    }

    fn execute_with(
        &self,
        idx: &ServiceIndexes,
        query: &Query,
        params: HashMap<String, Value>,
    ) -> Result<ExecResult, ExecError> {
        execute_on_engine_with_indexes_and_values(
            query,
            &self.graph,
            Some(&idx.fts),
            Some(&idx.labels),
            Some(&idx.props),
            Some(&idx.values),
            params,
        )
    }
}
