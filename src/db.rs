//! Core database struct and lifecycle methods.
//!
//! [`Drevo`] is the main entry point for all database operations.
//! It wraps a [`StorageBackend`] and manages auto-increment counters,
//! indexes, and the graph data model.
//!
//! # WAL / crash recovery (Phase 9 task `00053`)
//!
//! drevo persists every redb transaction durably — each backend `put` /
//! `delete` is its own committed redb transaction with the upstream
//! double-write + fsync that *is* the redb on-disk WAL. The recovery
//! model layered on top of that is:
//!
//! 1. **Counter recovery.** `Drevo::open` re-derives the next-id
//!    counters from `max(stored_id) + 1` instead of trusting the
//!    persisted `meta:next_node_id` / `meta:next_edge_id` blindly. The
//!    persisted counter is a *hint*; the on-disk node / edge rows are
//!    the source of truth. This prevents the pre-`00053` id-collision
//!    bug where a process killed between two `create_node` calls would
//!    rewind the counter and the next allocation would reuse an
//!    already-stored id.
//! 2. **Integrity inspection.** [`Drevo::check_integrity`] returns an
//!    [`IntegrityReport`] enumerating any structural issues (orphan
//!    index entries, dangling edge endpoints, counter drift repaired
//!    on this open, corrupt rows).
//! 3. **Explicit recovery entry point.** `Drevo::recover` is a
//!    convenience wrapper: it opens the database, runs
//!    `check_integrity`, and returns both the handle and the report so
//!    operators can react to surprises after a known-bad crash.
//!
//! `Drevo::open` and `Drevo::recover` are written as plain code spans
//! rather than intra-doc links because both are gated behind the
//! `redb-backend` Cargo feature — the symbols do not exist on the
//! `wasm32-unknown-unknown` build, and rustdoc under `-D warnings`
//! would reject the unresolved link there.
//!
//! See `tests/wal_crash_recovery_tests.rs` for the contract surface.

#[cfg(feature = "redb-backend")]
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

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
/// Created via `Drevo::open` (disk-backed, requires the
/// `redb-backend` Cargo feature) or [`Drevo::open_in_memory`]
/// (ephemeral). All graph operations are methods on this struct.
pub struct Drevo {
    /// The underlying key-value storage backend.
    backend: Box<dyn StorageBackend>,
    /// Auto-increment counter for node IDs.
    next_node_id: AtomicU64,
    /// Auto-increment counter for edge IDs.
    next_edge_id: AtomicU64,
    /// Set to `true` by `Drevo::open` (gated on `redb-backend`) when
    /// `load_counters` had to clamp the in-memory counter above the
    /// persisted `meta:next_*_id` because an on-disk row already used a
    /// higher id. This is the signal surfaced by
    /// [`IntegrityReport::counter_drift_repaired`] — see the module-level
    /// "WAL / crash recovery" section.
    counter_drift_repaired: AtomicBool,
    /// Active explicit-transaction slot (Phase 11 task `00072`).
    ///
    /// `Idle` is the no-transaction state — every mutation autocommits as
    /// before. `Active(journal)` is set by [`Self::tx_begin`]; while held
    /// every mutation pushes its inverse [`UndoOp`] onto the journal so
    /// [`Self::tx_rollback`] can undo them in reverse order.
    /// `RollingBack` is held briefly inside `tx_rollback` to keep
    /// concurrent `tx_begin` callers from racing in while the replay is
    /// in flight.
    ///
    /// The MVP allows only one in-flight transaction per `Drevo` handle;
    /// proper multi-writer isolation lands with MVCC (`00081`).
    tx_state: Mutex<TxState>,
}

/// Per-`Drevo` explicit-transaction state — see the `tx_state` field on
/// [`Drevo`] for the lifecycle.
#[derive(Debug, Default)]
enum TxState {
    /// No explicit transaction is active.
    #[default]
    Idle,
    /// `tx_begin` was called; mutations append to the journal.
    Active(TxJournal),
    /// `tx_rollback` is replaying inverses; further `tx_begin` calls are
    /// rejected until the replay finishes.
    RollingBack,
}

/// Append-only undo log captured during an explicit transaction.
///
/// Phase 11 task `00072`. Every mutation method on [`Drevo`] —
/// `create_node`, `update_node`, `delete_node`, `create_edge`,
/// `update_edge`, `delete_edge` — records an [`UndoOp`] here when the
/// session has called [`Drevo::tx_begin`]. The replay in
/// [`Drevo::tx_rollback`] walks the vector in reverse so that nested
/// `CREATE`/`UPDATE`/`DELETE` chains unwind to the pre-transaction
/// state (a `CREATE`-then-`UPDATE` rolls back through the update, then
/// purges the create).
#[derive(Debug, Default, Clone)]
struct TxJournal {
    ops: Vec<UndoOp>,
}

/// Inverse of a single graph mutation, captured eagerly while the
/// transaction is live so [`Drevo::tx_rollback`] can replay it without
/// re-reading the database.
#[derive(Debug, Clone)]
enum UndoOp {
    /// A `create_node` was performed inside the tx — undo by deleting
    /// the node at this id.
    CreatedNode(u64),
    /// A `create_edge` was performed inside the tx — undo by deleting
    /// the edge at this id.
    CreatedEdge(u64),
    /// A `update_node` was performed inside the tx — undo by writing
    /// the captured pre-image back at the same id.
    UpdatedNode(Node),
    /// A `update_edge` was performed inside the tx — undo by writing
    /// the captured pre-image back at the same id.
    UpdatedEdge(Edge),
    /// A `delete_node` was performed inside the tx — undo by
    /// re-inserting the captured pre-image at the same id. Cascade
    /// edges are journaled separately as their own `DeletedEdge` ops
    /// and replayed in reverse order so the node exists by the time
    /// the edges restore.
    DeletedNode(Node),
    /// A `delete_edge` was performed inside the tx — undo by
    /// re-inserting the captured pre-image at the same id.
    DeletedEdge(Edge),
}

/// Structured report produced by [`Drevo::check_integrity`].
///
/// Phase 9 task `00053`. Every field is intentionally a plain count or a
/// flag so the report can be cheaply serialised over the HTTP / FFI / WASM
/// boundaries — no `Vec<String>` blowups, no nested error types. A
/// healthy database produces a report where [`is_clean`](Self::is_clean)
/// returns `true`; anything else is a structural anomaly an operator
/// should investigate.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IntegrityReport {
    /// Total number of `node:` rows stored in the backend.
    pub node_count: u64,
    /// Total number of `edge:` rows stored in the backend.
    pub edge_count: u64,
    /// Highest node id observed in the `node:` prefix scan, if any node
    /// exists. `None` on an empty database.
    pub max_node_id: Option<u64>,
    /// Highest edge id observed in the `edge:` prefix scan, if any edge
    /// exists. `None` on an empty database.
    pub max_edge_id: Option<u64>,
    /// The next-id value the allocator will hand out for a new node.
    pub next_node_id: u64,
    /// The next-id value the allocator will hand out for a new edge.
    pub next_edge_id: u64,
    /// `true` if `Drevo::open` had to clamp the in-memory counter above
    /// the persisted hint because an on-disk node / edge id was already
    /// past the persisted value — the headline crash-recovery signal.
    /// (Plain code span because `Drevo::open` is gated on `redb-backend`
    /// and does not exist on the WASM target.)
    pub counter_drift_repaired: bool,
    /// Count of `node_kind:` index entries that point at a missing node id.
    pub orphan_node_kind_entries: u64,
    /// Count of `node_title:` index entries that point at a missing node id.
    pub orphan_node_title_entries: u64,
    /// Count of `node_uuid:` index entries that point at a missing node id.
    pub orphan_node_uuid_entries: u64,
    /// Count of `edge_kind:` index entries that point at a missing edge id.
    pub orphan_edge_kind_entries: u64,
    /// Count of `edge_uuid:` index entries that point at a missing edge id.
    pub orphan_edge_uuid_entries: u64,
    /// Count of `out:` / `in:` adjacency entries that reference a missing
    /// edge id.
    pub orphan_adjacency_entries: u64,
    /// Count of edges whose `from_id` or `to_id` references a node id
    /// that no longer exists in the `node:` prefix.
    pub dangling_edge_endpoints: u64,
    /// Count of `node:` rows whose payload failed bincode decode (treated
    /// as a hard corruption — `check_integrity` records the count but
    /// does not bail).
    pub corrupt_node_rows: u64,
    /// Count of `edge:` rows whose payload failed bincode decode.
    pub corrupt_edge_rows: u64,
}

impl IntegrityReport {
    /// Returns `true` when every structural counter is zero and no
    /// counter drift was repaired on this open — i.e. the database is
    /// in a fully consistent state with no recovery work to report.
    pub fn is_clean(&self) -> bool {
        !self.counter_drift_repaired
            && self.orphan_node_kind_entries == 0
            && self.orphan_node_title_entries == 0
            && self.orphan_node_uuid_entries == 0
            && self.orphan_edge_kind_entries == 0
            && self.orphan_edge_uuid_entries == 0
            && self.orphan_adjacency_entries == 0
            && self.dangling_edge_endpoints == 0
            && self.corrupt_node_rows == 0
            && self.corrupt_edge_rows == 0
    }
}

/// Structured report produced by [`Drevo::compact`] (Phase 9 task `00054`).
///
/// Compaction has two side-effects that an operator cares about: the
/// physical file footprint shrinks (or stays the same), and the in-memory
/// next-id counters get checkpointed to `meta:next_*_id` so a process kill
/// immediately after compact would not rewind them. The report carries
/// both pieces of information in a single serde-serialisable struct so
/// it rides over the HTTP / FFI / WASM boundaries cleanly.
///
/// `bytes_before` / `bytes_after` are `Option<u64>` because the ephemeral
/// memory backend has no measurable on-disk footprint — fields stay
/// `None` rather than reporting a fake zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompactReport {
    /// Size in bytes of the backend file *before* compaction, if the
    /// backend can measure itself. `None` for ephemeral in-memory backends.
    pub bytes_before: Option<u64>,
    /// Size in bytes of the backend file *after* compaction. `None` when
    /// the backend cannot measure its on-disk footprint.
    pub bytes_after: Option<u64>,
    /// `bytes_before - bytes_after`, saturating at zero. Always a `u64`
    /// (never `Option`) so callers can render "X bytes reclaimed" without
    /// branching on the backend type. Zero for ephemeral backends and for
    /// already-compact disk-backed backends.
    pub bytes_reclaimed: u64,
    /// The next-id value the node allocator will hand out after the
    /// compaction checkpoint persisted to `meta:next_node_id`.
    pub next_node_id: u64,
    /// The next-id value the edge allocator will hand out after the
    /// compaction checkpoint persisted to `meta:next_edge_id`.
    pub next_edge_id: u64,
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
        let backend = RedbBackend::open(path)?;
        let backend = Box::new(backend);
        let (next_node_id, next_edge_id, drift_repaired) = Self::load_counters(&*backend)?;
        Ok(Self {
            backend,
            next_node_id: AtomicU64::new(next_node_id),
            next_edge_id: AtomicU64::new(next_edge_id),
            counter_drift_repaired: AtomicBool::new(drift_repaired),
            tx_state: Mutex::new(TxState::Idle),
        })
    }

    /// Open a disk-backed database and run [`Self::check_integrity`] in
    /// one shot.
    ///
    /// Returns the live database handle alongside an [`IntegrityReport`]
    /// summarising any structural anomalies discovered on this open. The
    /// counter rescan that fixes the headline crash-recovery bug runs
    /// inside [`Self::open`] regardless — `recover` adds the integrity
    /// scan so operators can react to surprises after a known-bad crash
    /// instead of opening blind.
    ///
    /// Cost: one extra full scan of `node:` + `edge:` + every secondary
    /// index — proportional to the database size, *not* to anything in
    /// memory. Call this on a known-bad open path, not on every start.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::Storage`] / [`DrevoError::Decode`] if the
    /// scan itself fails — that is a hard failure distinct from the
    /// soft-warning counts the [`IntegrityReport`] surfaces.
    ///
    /// # Availability
    ///
    /// Requires the `redb-backend` feature; mirrors [`open`](Self::open).
    #[cfg(feature = "redb-backend")]
    pub fn recover(path: &Path) -> Result<(Self, IntegrityReport)> {
        let db = Self::open(path)?;
        let report = db.check_integrity()?;
        Ok((db, report))
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
            counter_drift_repaired: AtomicBool::new(false),
            tx_state: Mutex::new(TxState::Idle),
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
        self.backend.flush()?;
        Ok(())
    }

    /// Reclaim unused storage and checkpoint the next-id counters.
    ///
    /// Phase 9 task `00054`. The exact backend-level reclaim is described
    /// on [`crate::storage::StorageBackend::compact`]; this wrapper adds
    /// the operator-facing semantics on top:
    ///
    /// 1. **Counter checkpoint** — `meta:next_node_id` /
    ///    `meta:next_edge_id` are persisted *before* the physical
    ///    compaction runs, so a process kill at any point during or after
    ///    the call leaves the file with a counter that reflects the
    ///    pre-compaction id allocator state. The `00053` rescan in
    ///    `Drevo::open` still corrects any gap if compaction never
    ///    reached the counter.
    /// 2. **Size measurement** — the backend's `size_bytes` is sampled
    ///    before and after the reclaim so the returned [`CompactReport`]
    ///    can quote a concrete "bytes reclaimed" figure to operators and
    ///    monitoring dashboards.
    /// 3. **Physical compaction** — `redb::Database::compact` releases
    ///    free pages on the redb backend; the persistent memory backend
    ///    rewrites its snapshot file; the ephemeral memory backend is a
    ///    no-op.
    ///
    /// Compaction is intentionally `&mut self` because the redb compactor
    /// takes `&mut Database` (it must hold an exclusive write transaction
    /// to relocate pages). Callers that share `Drevo` behind an `Arc`
    /// must therefore drop or `Arc::try_unwrap` the handle before
    /// compacting — there is no in-flight-safe variant.
    ///
    /// # Errors
    ///
    /// - [`DrevoError::Storage`] propagated from the counter checkpoint,
    ///   size measurement, or backend compaction call.
    /// - `DrevoError::Storage(StorageError::CompactNotExclusive)` when the
    ///   redb backend's inner `Arc<Database>` has outstanding clones
    ///   (e.g. an embedded user kept a copy of the backend) — the
    ///   compactor needs `&mut Database`.
    pub fn compact(&mut self) -> Result<CompactReport> {
        self.persist_counters()?;
        let bytes_before = self.backend.size_bytes()?;
        self.backend.compact()?;
        let bytes_after = self.backend.size_bytes()?;
        let bytes_reclaimed = match (bytes_before, bytes_after) {
            (Some(b), Some(a)) => b.saturating_sub(a),
            _ => 0,
        };
        Ok(CompactReport {
            bytes_before,
            bytes_after,
            bytes_reclaimed,
            next_node_id: self.next_node_id.load(Ordering::Relaxed),
            next_edge_id: self.next_edge_id.load(Ordering::Relaxed),
        })
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
        self.backend.get(META_NEXT_NODE_ID)?;
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

    /// Clamp the node-id allocator so the next ID it hands out is at least
    /// `min`. No-op if the counter is already higher. Used by JSON import
    /// (Phase 9 task `00055`) to resume allocation above an imported range.
    pub(crate) fn bump_node_counter_to_at_least(&self, min: u64) {
        self.next_node_id.fetch_max(min, Ordering::Relaxed);
    }

    /// Clamp the edge-id allocator so the next ID it hands out is at least
    /// `min`. See [`bump_node_counter_to_at_least`](Self::bump_node_counter_to_at_least).
    pub(crate) fn bump_edge_counter_to_at_least(&self, min: u64) {
        self.next_edge_id.fetch_max(min, Ordering::Relaxed);
    }

    /// Scan and decode every node currently stored in the backend.
    ///
    /// Used by JSON export (Phase 9 task `00055`). Returns nodes sorted by
    /// ascending id so the dump is deterministic regardless of `scan_prefix`
    /// implementation details on the chosen backend.
    pub(crate) fn collect_all_nodes(&self) -> Result<Vec<Node>> {
        let entries = self.backend.scan_prefix(PREFIX_NODE)?;
        let mut nodes = Vec::with_capacity(entries.len());
        for (key, bytes) in &entries {
            // `node:` prefix is also a substring of `node_uuid:`, `node_title:`,
            // `node_kind:`. The data rows are exactly `node:` + 8-byte le id,
            // so filter by total key length to keep `scan_prefix` correct
            // across both backends without depending on backend-specific
            // ordering quirks.
            if key.len() != PREFIX_NODE.len() + 8 {
                continue;
            }
            nodes.push(deserialize_node(bytes)?);
        }
        nodes.sort_by_key(|n| n.id);
        Ok(nodes)
    }

    /// Scan and decode every edge currently stored in the backend.
    pub(crate) fn collect_all_edges(&self) -> Result<Vec<Edge>> {
        let entries = self.backend.scan_prefix(PREFIX_EDGE)?;
        let mut edges = Vec::with_capacity(entries.len());
        for (key, bytes) in &entries {
            if key.len() != PREFIX_EDGE.len() + 8 {
                continue;
            }
            edges.push(deserialize_edge(bytes)?);
        }
        edges.sort_by_key(|e| e.id);
        Ok(edges)
    }

    /// Insert a verbatim [`Node`], preserving id / uuid / timestamps and
    /// rebuilding every secondary index. Used by JSON import (Phase 9 task
    /// `00055`) so dumps can round-trip without losing the producer's IDs.
    ///
    /// Errors:
    /// - [`DrevoError::DuplicateTitle`] if `node.title` is already indexed
    ///   to a *different* node id.
    pub(crate) fn insert_node_raw(&self, node: &Node) -> Result<()> {
        // Title uniqueness: only fail when a different id owns this title;
        // exact-content matches were filtered upstream in `dump::apply_dump`.
        let title_key = node_title_key(&node.title);
        if let Some(existing) = self.backend.get(&title_key)? {
            let existing_id = u64_from_bytes(&existing);
            if existing_id != node.id {
                return Err(DrevoError::DuplicateTitle(node.title.clone()));
            }
        }

        let data = serialize_node(node)?;
        self.backend.put(&node_key(node.id), &data)?;
        self.backend
            .put(&node_uuid_key(&node.uuid), &node.id.to_le_bytes())?;
        self.backend.put(&title_key, &node.id.to_le_bytes())?;
        self.backend.put(&node_kind_key(&node.kind, node.id), &[])?;
        fts_index::index_node(&*self.backend, node.id, &node.title, &node.body)?;
        self.backend
            .put(&updated_key(node.updated_at, node.id), &[])?;
        Ok(())
    }

    /// Insert a verbatim [`Edge`], preserving id / uuid / timestamp and
    /// rebuilding adjacency / kind indexes.
    pub(crate) fn insert_edge_raw(&self, edge: &Edge) -> Result<()> {
        let data = serialize_edge(edge)?;
        self.backend.put(&edge_key(edge.id), &data)?;
        self.backend
            .put(&edge_uuid_key(&edge.uuid), &edge.id.to_le_bytes())?;
        self.backend
            .put(&out_edge_key(edge.from_id, edge.id), &[])?;
        self.backend.put(&in_edge_key(edge.to_id, edge.id), &[])?;
        self.backend.put(&edge_kind_key(&edge.kind, edge.id), &[])?;
        Ok(())
    }

    /// Return a reference to the underlying storage backend.
    #[allow(dead_code)] // Reserved for future use (e.g. traversal, search)
    pub(crate) fn backend(&self) -> &dyn StorageBackend {
        &*self.backend
    }

    // ---------------------------------------------------------------
    // Explicit transactions (Phase 11 task `00072`)
    // ---------------------------------------------------------------
    //
    // The Bolt session layer (`src/bolt/session.rs`) maps the wire
    // `BEGIN` / `COMMIT` / `ROLLBACK` messages onto these methods. The
    // MVP design is an undo-log replay: every mutation method below
    // (`create_node`, `update_node`, `delete_node`, `create_edge`,
    // `update_edge`, `delete_edge`) pushes its inverse onto the active
    // [`TxJournal`] while the transaction is open, and `tx_rollback`
    // walks the journal in reverse order to restore the pre-transaction
    // state. Mutations called outside an explicit transaction continue
    // to autocommit exactly as before — the journal slot stays `Idle`
    // and `record_undo` becomes a no-op brief mutex acquisition.
    //
    // Concurrency: at most one explicit transaction is in flight per
    // `Drevo` handle. Concurrent autocommit writes from other sessions
    // are not blocked but *are* journaled, so a rollback affects them
    // as well — proper isolation lands with MVCC (`00080`–`00084`).

    /// Begin an explicit transaction on this `Drevo` handle.
    ///
    /// While a transaction is active, every successful mutation appends
    /// an inverse operation to an internal journal. [`tx_commit`]
    /// discards the journal; [`tx_rollback`] replays it in reverse to
    /// restore the pre-transaction state of the mutated entities.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::TransactionAlreadyActive`] if a previous
    /// transaction is still in flight (the MVP is single-writer; proper
    /// isolation lands with `00081`).
    ///
    /// [`tx_commit`]: Self::tx_commit
    /// [`tx_rollback`]: Self::tx_rollback
    pub fn tx_begin(&self) -> Result<()> {
        let mut state = self.lock_tx_state();
        match &*state {
            TxState::Idle => {
                *state = TxState::Active(TxJournal::default());
                Ok(())
            }
            TxState::Active(_) | TxState::RollingBack => Err(DrevoError::TransactionAlreadyActive),
        }
    }

    /// Commit the in-flight explicit transaction.
    ///
    /// The mutations performed since [`tx_begin`](Self::tx_begin) are
    /// already durable on disk (each redb `put` / `delete` autocommitted
    /// at the storage level). This call simply discards the undo
    /// journal so no rollback is possible afterwards.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::NoActiveTransaction`] if no transaction is
    /// active.
    pub fn tx_commit(&self) -> Result<()> {
        let mut state = self.lock_tx_state();
        match &*state {
            TxState::Active(_) => {
                *state = TxState::Idle;
                Ok(())
            }
            TxState::Idle | TxState::RollingBack => Err(DrevoError::NoActiveTransaction),
        }
    }

    /// Roll back the in-flight explicit transaction.
    ///
    /// Replays the captured inverse operations in reverse order, undoing
    /// every `create_*` / `update_*` / `delete_*` performed since
    /// [`tx_begin`](Self::tx_begin). The journal slot is moved to
    /// `RollingBack` before the replay starts so concurrent `tx_begin`
    /// calls from other sessions are rejected until the rollback
    /// completes; further mutations executed by the replay itself see
    /// `RollingBack` and skip journaling.
    ///
    /// # Errors
    ///
    /// * [`DrevoError::NoActiveTransaction`] if no transaction is
    ///   active.
    /// * The first error encountered while replaying an inverse op.
    ///   In that case the rollback is left partially applied and the
    ///   tx slot transitions back to `Idle` (the caller's session will
    ///   already be `Failed` and require `RESET` to recover).
    pub fn tx_rollback(&self) -> Result<()> {
        let journal = {
            let mut state = self.lock_tx_state();
            match std::mem::replace(&mut *state, TxState::RollingBack) {
                TxState::Active(j) => j,
                prev @ (TxState::Idle | TxState::RollingBack) => {
                    // Restore the pre-call state so we don't leak a
                    // bogus `RollingBack` slot on the error path.
                    *state = prev;
                    return Err(DrevoError::NoActiveTransaction);
                }
            }
        };
        let mut replay_err: Option<DrevoError> = None;
        for op in journal.ops.into_iter().rev() {
            if let Err(e) = self.apply_undo(op) {
                replay_err = Some(e);
                break;
            }
        }
        {
            let mut state = self.lock_tx_state();
            *state = TxState::Idle;
        }
        match replay_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// `true` while an explicit transaction is in flight. Inspected by
    /// the Bolt session machinery so it can refuse stray `COMMIT` /
    /// `ROLLBACK` messages without going through the locking dance.
    pub fn is_tx_active(&self) -> bool {
        matches!(*self.lock_tx_state(), TxState::Active(_))
    }

    /// Acquire the transaction-state mutex, recovering transparently
    /// from a poisoned lock. `unwrap_or_else(|p| p.into_inner())` is
    /// the standard idiom for "I am OK reading the inner state even if
    /// a previous holder panicked" — we have no consistency invariant
    /// the panic could have broken (the journal is structurally
    /// independent of every other field).
    fn lock_tx_state(&self) -> std::sync::MutexGuard<'_, TxState> {
        self.tx_state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Push an inverse op onto the active journal, if any. No-op when
    /// the slot is `Idle` or `RollingBack`. Called by every mutation
    /// method after the change has successfully landed on disk.
    fn record_undo(&self, op: UndoOp) {
        let mut state = self.lock_tx_state();
        if let TxState::Active(j) = &mut *state {
            j.ops.push(op);
        }
    }

    /// Apply one captured inverse op against the live backend. Called
    /// only from [`Self::tx_rollback`]; the tx slot is `RollingBack`
    /// for the duration, so mutations performed here do not re-journal.
    fn apply_undo(&self, op: UndoOp) -> Result<()> {
        match op {
            UndoOp::CreatedNode(id) => self.purge_node_no_journal(id),
            UndoOp::CreatedEdge(id) => self.purge_edge_no_journal(id),
            UndoOp::UpdatedNode(pre) => self.restore_node_in_place(&pre),
            UndoOp::UpdatedEdge(pre) => self.restore_edge_in_place(&pre),
            UndoOp::DeletedNode(pre) => self.recreate_node_at_id(pre),
            UndoOp::DeletedEdge(pre) => self.recreate_edge_at_id(pre),
        }
    }

    /// Re-insert a node and all its indexes at the given id. Called by
    /// the rollback path to undo a `delete_node`. The caller has already
    /// ensured (via journal ordering) that no edges reference this id.
    fn recreate_node_at_id(&self, node: Node) -> Result<()> {
        let data = serialize_node(&node)?;
        self.backend.put(&node_key(node.id), &data)?;
        self.backend
            .put(&node_uuid_key(&node.uuid), &node.id.to_le_bytes())?;
        self.backend
            .put(&node_title_key(&node.title), &node.id.to_le_bytes())?;
        self.backend.put(&node_kind_key(&node.kind, node.id), &[])?;
        fts_index::index_node(&*self.backend, node.id, &node.title, &node.body)?;
        self.backend
            .put(&updated_key(node.updated_at, node.id), &[])?;
        Ok(())
    }

    /// Re-insert an edge and all its indexes at the given id. Used by
    /// the rollback path to undo a `delete_edge`.
    fn recreate_edge_at_id(&self, edge: Edge) -> Result<()> {
        let data = serialize_edge(&edge)?;
        self.backend.put(&edge_key(edge.id), &data)?;
        self.backend
            .put(&edge_uuid_key(&edge.uuid), &edge.id.to_le_bytes())?;
        self.backend
            .put(&out_edge_key(edge.from_id, edge.id), &[])?;
        self.backend.put(&in_edge_key(edge.to_id, edge.id), &[])?;
        self.backend.put(&edge_kind_key(&edge.kind, edge.id), &[])?;
        Ok(())
    }

    /// Overwrite the node at `pre.id` with the supplied pre-image,
    /// rebuilding every secondary index from the currently-stored
    /// values. Used by the rollback path to undo an `update_node`.
    fn restore_node_in_place(&self, pre: &Node) -> Result<()> {
        let current = self
            .get_node(pre.id)?
            .ok_or(DrevoError::NodeNotFound(pre.id))?;
        // Drop the current secondary-index entries before re-inserting
        // the pre-image — title / kind / FTS / updated-at may all differ.
        self.backend.delete(&node_uuid_key(&current.uuid))?;
        self.backend.delete(&node_title_key(&current.title))?;
        self.backend
            .delete(&node_kind_key(&current.kind, current.id))?;
        fts_index::deindex_node(&*self.backend, current.id, &current.title, &current.body)?;
        self.backend
            .delete(&updated_key(current.updated_at, current.id))?;
        self.recreate_node_at_id(pre.clone())
    }

    /// Overwrite the edge at `pre.id` with the supplied pre-image,
    /// rebuilding every secondary index. Used by the rollback path to
    /// undo an `update_edge` (endpoints / uuid don't change but kind
    /// might).
    fn restore_edge_in_place(&self, pre: &Edge) -> Result<()> {
        let current = self
            .get_edge(pre.id)?
            .ok_or(DrevoError::EdgeNotFound(pre.id))?;
        self.backend
            .delete(&edge_kind_key(&current.kind, current.id))?;
        // uuid / endpoints don't change inside `update_edge`, but
        // rewriting the indexes unconditionally keeps the restore
        // self-contained.
        self.backend.delete(&edge_uuid_key(&current.uuid))?;
        self.backend
            .delete(&out_edge_key(current.from_id, current.id))?;
        self.backend
            .delete(&in_edge_key(current.to_id, current.id))?;
        self.recreate_edge_at_id(pre.clone())
    }

    /// Remove a node and all its indexes without journaling and without
    /// cascading to edges. The journal replay calls this after every
    /// edge that referenced the node has already been purged (the
    /// inverse-order replay guarantees this).
    fn purge_node_no_journal(&self, id: u64) -> Result<()> {
        let node = self.get_node(id)?.ok_or(DrevoError::NodeNotFound(id))?;
        self.backend.delete(&node_key(id))?;
        self.backend.delete(&node_uuid_key(&node.uuid))?;
        self.backend.delete(&node_title_key(&node.title))?;
        self.backend.delete(&node_kind_key(&node.kind, id))?;
        fts_index::deindex_node(&*self.backend, id, &node.title, &node.body)?;
        self.backend.delete(&updated_key(node.updated_at, id))?;
        Ok(())
    }

    /// Remove an edge and all its indexes without journaling.
    fn purge_edge_no_journal(&self, id: u64) -> Result<()> {
        let edge = self.get_edge(id)?.ok_or(DrevoError::EdgeNotFound(id))?;
        self.backend.delete(&edge_key(id))?;
        self.backend.delete(&edge_uuid_key(&edge.uuid))?;
        self.backend.delete(&out_edge_key(edge.from_id, id))?;
        self.backend.delete(&in_edge_key(edge.to_id, id))?;
        self.backend.delete(&edge_kind_key(&edge.kind, id))?;
        Ok(())
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
        if self.backend.get(&title_key)?.is_some() {
            return Err(DrevoError::DuplicateTitle(new_node.title));
        }

        let id = self.alloc_node_id();
        let node = new_node.into_node(id);

        // Store node data
        let data = serialize_node(&node)?;
        self.backend.put(&node_key(id), &data)?;

        // UUID index
        self.backend
            .put(&node_uuid_key(&node.uuid), &id.to_le_bytes())?;

        // Title index
        self.backend.put(&title_key, &id.to_le_bytes())?;

        // Kind index
        self.backend.put(&node_kind_key(&node.kind, id), &[])?;

        // FTS index
        fts_index::index_node(&*self.backend, id, &node.title, &node.body)?;

        // Updated-at index (newest-first ordering)
        self.backend.put(&updated_key(node.updated_at, id), &[])?;

        self.record_undo(UndoOp::CreatedNode(id));
        Ok(node)
    }

    /// Retrieve a node by its auto-increment ID.
    ///
    /// Returns `None` if the node does not exist.
    pub fn get_node(&self, id: u64) -> Result<Option<Node>> {
        match self.backend.get(&node_key(id))? {
            Some(bytes) => Ok(Some(deserialize_node(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Retrieve a node by its UUID v7.
    ///
    /// Returns `None` if no node has the given UUID.
    pub fn get_node_by_uuid(&self, uuid: &[u8; 16]) -> Result<Option<Node>> {
        match self.backend.get(&node_uuid_key(uuid))? {
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
        match self.backend.get(&node_title_key(title))? {
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

        // Capture the pre-image *before* mutation so an active explicit
        // transaction (Phase 11 task `00072`) can restore the prior
        // state on rollback. Cheap clone — `Node` is small bincode-
        // derived plain data.
        let pre_image = node.clone();

        let old_title = node.title.clone();
        let old_body = node.body.clone();
        let old_kind = node.kind.clone();
        let old_updated_at = node.updated_at;

        // Check title uniqueness before applying patch
        if let Some(ref new_title) = patch.title {
            if *new_title != old_title {
                let title_key = node_title_key(new_title);
                if self.backend.get(&title_key)?.is_some() {
                    return Err(DrevoError::DuplicateTitle(new_title.clone()));
                }
            }
        }

        node.apply_patch(patch);

        // Store updated node
        let data = serialize_node(&node)?;
        self.backend.put(&node_key(id), &data)?;

        // Update title index if title changed
        if node.title != old_title {
            self.backend.delete(&node_title_key(&old_title))?;
            self.backend
                .put(&node_title_key(&node.title), &id.to_le_bytes())?;
        }

        // Update kind index if kind changed
        if node.kind != old_kind {
            self.backend.delete(&node_kind_key(&old_kind, id))?;
            self.backend.put(&node_kind_key(&node.kind, id), &[])?;
        }

        // Update FTS index if title or body changed
        if node.title != old_title || node.body != old_body {
            fts_index::deindex_node(&*self.backend, id, &old_title, &old_body)?;
            fts_index::index_node(&*self.backend, id, &node.title, &node.body)?;
        }

        // Update updated-at index: remove old entry, add new one
        self.backend.delete(&updated_key(old_updated_at, id))?;
        self.backend.put(&updated_key(node.updated_at, id), &[])?;

        self.record_undo(UndoOp::UpdatedNode(pre_image));
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
        // Each cascade `delete_edge` records its own `DeletedEdge` undo
        // op, so an in-flight explicit transaction can replay them in
        // reverse and restore both the node and its edges.
        let connected_edges = self.edges_of(id, Direction::Both)?;
        for edge in &connected_edges {
            self.delete_edge(edge.id)?;
        }

        // Remove node data
        self.backend.delete(&node_key(id))?;

        // Remove UUID index
        self.backend.delete(&node_uuid_key(&node.uuid))?;

        // Remove title index
        self.backend.delete(&node_title_key(&node.title))?;

        // Remove kind index
        self.backend.delete(&node_kind_key(&node.kind, id))?;

        // Remove FTS index
        fts_index::deindex_node(&*self.backend, id, &node.title, &node.body)?;

        // Remove updated-at index
        self.backend.delete(&updated_key(node.updated_at, id))?;

        self.record_undo(UndoOp::DeletedNode(node));
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
    /// # Preconditions
    ///
    /// - `weight` MUST be a finite `f32` — NaN, +Inf, and -Inf are
    ///   rejected with [`DrevoError::InvalidWeight`]. Required because
    ///   `Edge` derives `PartialEq` (NaN ≠ NaN breaks the contract) and
    ///   Dijkstra in [`crate::traversal`] assumes finite weights.
    ///
    /// # Errors
    ///
    /// - [`DrevoError::NodeNotFound`] if either `from_id` or
    ///   `to_id` does not refer to an existing node.
    /// - [`DrevoError::InvalidWeight`] if `weight` is not finite.
    pub fn create_edge(&self, new_edge: NewEdge) -> Result<Edge> {
        // Validate weight finiteness — see `audit/AUDIT-model.md` F4
        if !new_edge.weight.is_finite() {
            return Err(DrevoError::InvalidWeight(new_edge.weight));
        }

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
        self.backend.put(&edge_key(id), &data)?;

        // UUID index
        self.backend
            .put(&edge_uuid_key(&edge.uuid), &id.to_le_bytes())?;

        // Outgoing adjacency: out:{from_id}:{edge_id}
        self.backend.put(&out_edge_key(edge.from_id, id), &[])?;

        // Incoming adjacency: in:{to_id}:{edge_id}
        self.backend.put(&in_edge_key(edge.to_id, id), &[])?;

        // Edge kind index
        self.backend.put(&edge_kind_key(&edge.kind, id), &[])?;

        self.record_undo(UndoOp::CreatedEdge(id));
        Ok(edge)
    }

    /// Retrieve an edge by its auto-increment ID.
    ///
    /// Returns `None` if the edge does not exist.
    pub fn get_edge(&self, id: u64) -> Result<Option<Edge>> {
        match self.backend.get(&edge_key(id))? {
            Some(bytes) => Ok(Some(deserialize_edge(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Retrieve an edge by its UUID v7.
    ///
    /// Returns `None` if no edge has the given UUID.
    pub fn get_edge_by_uuid(&self, uuid: &[u8; 16]) -> Result<Option<Edge>> {
        match self.backend.get(&edge_uuid_key(uuid))? {
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
    /// # Preconditions
    ///
    /// - When `patch.weight` is `Some(w)`, `w` MUST be a finite `f32`.
    ///   See [`create_edge`](Self::create_edge) for the rationale.
    ///
    /// # Errors
    ///
    /// - [`DrevoError::EdgeNotFound`] if the edge does not exist.
    /// - [`DrevoError::InvalidWeight`] if `patch.weight` carries a
    ///   non-finite `f32`.
    pub fn update_edge(&self, id: u64, patch: EdgePatch) -> Result<Edge> {
        // Validate weight finiteness before any storage mutation so the
        // failure is observable without leaving the indexes drifted.
        if let Some(w) = patch.weight {
            if !w.is_finite() {
                return Err(DrevoError::InvalidWeight(w));
            }
        }

        let mut edge = self.get_edge(id)?.ok_or(DrevoError::EdgeNotFound(id))?;

        // Pre-image for explicit-tx rollback (Phase 11 task `00072`).
        let pre_image = edge.clone();

        let old_kind = edge.kind.clone();

        edge.apply_patch(patch);

        let data = serialize_edge(&edge)?;
        self.backend.put(&edge_key(id), &data)?;

        // Update edge_kind index if kind changed
        if edge.kind != old_kind {
            self.backend.delete(&edge_kind_key(&old_kind, id))?;
            self.backend.put(&edge_kind_key(&edge.kind, id), &[])?;
        }

        self.record_undo(UndoOp::UpdatedEdge(pre_image));
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
        self.backend.delete(&edge_key(id))?;

        // Remove UUID index
        self.backend.delete(&edge_uuid_key(&edge.uuid))?;

        // Remove outgoing adjacency entry
        self.backend.delete(&out_edge_key(edge.from_id, id))?;

        // Remove incoming adjacency entry
        self.backend.delete(&in_edge_key(edge.to_id, id))?;

        // Remove edge kind index
        self.backend.delete(&edge_kind_key(&edge.kind, id))?;

        self.record_undo(UndoOp::DeletedEdge(edge));
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
        let entries = self.backend.scan_prefix(&prefix)?;

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
        let entries = self.backend.scan_prefix(&prefix)?;

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

        let entries = self.backend.scan_prefix(PREFIX_UPDATED)?;

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
    /// list intersection (AND semantics), scores each candidate using
    /// TF-IDF, and returns up to `limit` results sorted by descending
    /// score, then by ascending node id for stability.
    ///
    /// **TF-IDF formula (as implemented):**
    /// - TF (term frequency): the trigram set per node is deduplicated, so
    ///   per-trigram TF is `1 / |node_trigrams|` when the trigram is
    ///   present, otherwise `0`. (Binary presence normalised by node
    ///   trigram cardinality — a length-penalty that down-weights long
    ///   bodies.)
    /// - IDF (smoothed inverse document frequency):
    ///   `ln(1 + N / df)` where `N` is the total number of indexed
    ///   nodes and `df` is the number of nodes containing the trigram.
    ///   The `+ 1` smoothing keeps the IDF strictly positive when
    ///   `df == N`, preventing trigrams that appear in *every* node
    ///   from collapsing to a zero score.
    /// - Score = sum of `tf * idf` for each query trigram.
    ///
    /// Returns an empty list if the query produces no trigrams or no
    /// nodes match.
    ///
    /// # Performance
    ///
    /// `audit/AUDIT-fts.md` documents a measured ~800 ms vs 50 ms-target
    /// gap on broad single-token queries against ~10k nodes. The
    /// bottleneck is the per-candidate `extract_trigrams` call here
    /// combined with `scan_prefix(PREFIX_NODE)` to count `N`. Mitigations
    /// (cached posting-list lengths, persisted node-count meta key,
    /// inverted-index compaction) are tracked as a separate follow-up
    /// refactor in the audit report — out of scope for the audit task
    /// itself.
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
        let all_nodes = self.backend.scan_prefix(PREFIX_NODE)?;
        let total_nodes = all_nodes.len() as f32;

        // Precompute IDF for each query trigram
        let mut idf_values: Vec<f32> = Vec::with_capacity(query_trigrams.len());
        for trigram in &query_trigrams {
            let df = fts_index::posting_list_len(&*self.backend, trigram)? as f32;
            // Smoothed IDF: ln(1 + N / df) — avoids zero when df == N. We
            // use `f32::ln_1p` (i.e. `ln(1 + x)`) so the intermediate
            // `1 + x` stays accurate when `x` is near zero — clippy nursery
            // `suboptimal_flops` flag, applied in audit 00113.
            let idf = if df > 0.0 {
                (total_nodes / df).ln_1p()
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
    ///
    /// Edge weights must be non-negative; the model layer guarantees
    /// finiteness (NaN / ±∞ rejected at write time), but negative
    /// finite weights are admitted by storage and may cause this
    /// implementation to return a non-optimal path. See
    /// `traversal::shortest_path` rustdoc for the full precondition.
    pub fn shortest_path(&self, from: u64, to: u64) -> Result<Option<Vec<u64>>> {
        self.shortest_path_filtered(from, to, None)
    }

    /// Variant of [`Self::shortest_path`] that only considers edges with
    /// `kind == edge_kind` when `edge_kind` is `Some`. Passing `None`
    /// is equivalent to [`Self::shortest_path`]. Parity addition with
    /// `bfs` / `dfs`, audited under task `00107`.
    pub fn shortest_path_filtered(
        &self,
        from: u64,
        to: u64,
        edge_kind: Option<&str>,
    ) -> Result<Option<Vec<u64>>> {
        crate::traversal::shortest_path(from, to, edge_kind, &|id| self.get_node(id), &|id, dir| {
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
        self.subgraph_filtered(root, depth, None)
    }

    /// Variant of [`Self::subgraph`] that restricts both the discovery
    /// BFS and the edge-collection phase to edges with
    /// `kind == edge_kind` when `edge_kind` is `Some`. Nodes only
    /// reachable through filtered-out edges are not included in the
    /// returned subgraph. Passing `None` is equivalent to
    /// [`Self::subgraph`]. Parity addition with `bfs` / `dfs`,
    /// audited under task `00107`.
    pub fn subgraph_filtered(
        &self,
        root: u64,
        depth: u8,
        edge_kind: Option<&str>,
    ) -> Result<SubGraph> {
        crate::traversal::subgraph(
            root,
            depth,
            edge_kind,
            &|id| self.get_node(id),
            &|id, dir| self.edges_of(id, dir),
        )
    }

    /// Return immediate neighbors of a node (BFS depth=1).
    ///
    /// Convenience wrapper over [`Self::bfs`] with `max_depth=1`.
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
    // Invariant verification (test-only — `00106`)
    // ---------------------------------------------------------------

    /// Verify the four storage-layer invariants from
    /// `.claude/skills/drevo-database/SKILL.md` §"Invariants".
    ///
    /// 1. **Adjacency consistency** — every `out:{from_id}:{edge_id}` entry
    ///    is mirrored by `in:{to_id}:{edge_id}`, and vice versa.
    /// 2. **No dangling adjacency** — every adjacency entry references an
    ///    edge that exists and points at the correct node.
    /// 3. **Index consistency** — every `node_uuid:` / `node_title:` /
    ///    `node_kind:` index entry resolves to an existing node;
    ///    `edge_uuid:` / `edge_kind:` to an existing edge.
    /// 4. **`updated_idx` parity** — every node has exactly one entry in
    ///    the inverted-timestamp `updated:` index.
    ///
    /// Returned vector is empty when all invariants hold. Each element is
    /// a human-readable description of a single violation; the caller is
    /// expected to `assert!(violations.is_empty(), "{:?}", violations)`
    /// inside a test.
    ///
    /// This is a **test-only** helper. It is exposed for the integration
    /// test in `tests/db_invariants_tests.rs` and gated `pub(crate)` so
    /// it does not leak through the public API of the crate.
    #[doc(hidden)]
    pub fn verify_invariants(&self) -> Result<Vec<String>> {
        let mut violations: Vec<String> = Vec::new();

        // Collect every edge by scanning the edge: prefix once.
        let edge_entries = self.backend.scan_prefix(PREFIX_EDGE)?;
        let mut edges_by_id: std::collections::HashMap<u64, Edge> =
            std::collections::HashMap::with_capacity(edge_entries.len());
        for (key, bytes) in &edge_entries {
            // Skip edge_uuid: and edge_kind: entries which share the
            // "edge" string but have longer prefixes that don't match
            // PREFIX_EDGE (b"edge:") followed by an 8-byte id.
            if key.len() != PREFIX_EDGE.len() + 8 {
                continue;
            }
            let edge = deserialize_edge(bytes)?;
            edges_by_id.insert(edge.id, edge);
        }

        // Collect every node by scanning the node: prefix once.
        let node_entries = self.backend.scan_prefix(PREFIX_NODE)?;
        let mut nodes_by_id: std::collections::HashMap<u64, Node> =
            std::collections::HashMap::with_capacity(node_entries.len());
        for (key, bytes) in &node_entries {
            if key.len() != PREFIX_NODE.len() + 8 {
                continue;
            }
            let node = deserialize_node(bytes)?;
            nodes_by_id.insert(node.id, node);
        }

        // ---- Invariant #1 & #2 — adjacency consistency + no dangling ----
        let out_entries = self.backend.scan_prefix(PREFIX_OUT)?;
        for (key, _) in &out_entries {
            // out:{from_id_8}:{edge_id_8}
            let expected_len = PREFIX_OUT.len() + 8 + 1 + 8;
            if key.len() != expected_len {
                violations.push(format!(
                    "adjacency key has unexpected length: out: key len = {}",
                    key.len()
                ));
                continue;
            }
            let from_id = u64_from_adjacency_key_first_id(key, PREFIX_OUT);
            let edge_id = edge_id_from_adjacency_key(
                key,
                &[PREFIX_OUT, &from_id.to_le_bytes(), b":"].concat(),
            );
            match edges_by_id.get(&edge_id) {
                None => violations.push(format!(
                    "out adjacency points at missing edge: from_id={from_id}, edge_id={edge_id}"
                )),
                Some(e) => {
                    if e.from_id != from_id {
                        violations.push(format!(
                            "out adjacency from_id mismatch: key from_id={from_id}, \
                             edge.from_id={}",
                            e.from_id
                        ));
                    }
                    // Invariant #1 — the corresponding in: entry MUST exist.
                    let in_key = in_edge_key(e.to_id, e.id);
                    if self.backend.get(&in_key)?.is_none() {
                        violations.push(format!(
                            "out adjacency missing in mirror: edge_id={edge_id}, \
                             from_id={from_id}, to_id={}",
                            e.to_id
                        ));
                    }
                }
            }
        }

        let in_entries = self.backend.scan_prefix(PREFIX_IN)?;
        for (key, _) in &in_entries {
            let expected_len = PREFIX_IN.len() + 8 + 1 + 8;
            if key.len() != expected_len {
                violations.push(format!(
                    "adjacency key has unexpected length: in: key len = {}",
                    key.len()
                ));
                continue;
            }
            let to_id = u64_from_adjacency_key_first_id(key, PREFIX_IN);
            let edge_id =
                edge_id_from_adjacency_key(key, &[PREFIX_IN, &to_id.to_le_bytes(), b":"].concat());
            match edges_by_id.get(&edge_id) {
                None => violations.push(format!(
                    "in adjacency points at missing edge: to_id={to_id}, edge_id={edge_id}"
                )),
                Some(e) => {
                    if e.to_id != to_id {
                        violations.push(format!(
                            "in adjacency to_id mismatch: key to_id={to_id}, edge.to_id={}",
                            e.to_id
                        ));
                    }
                    // Invariant #1 — mirror direction.
                    let out_key = out_edge_key(e.from_id, e.id);
                    if self.backend.get(&out_key)?.is_none() {
                        violations.push(format!(
                            "in adjacency missing out mirror: edge_id={edge_id}, \
                             to_id={to_id}, from_id={}",
                            e.from_id
                        ));
                    }
                }
            }
        }

        // Every edge must have both adjacency entries — symmetrical check.
        for edge in edges_by_id.values() {
            if self
                .backend
                .get(&out_edge_key(edge.from_id, edge.id))?
                .is_none()
            {
                violations.push(format!(
                    "edge {} missing its out: adjacency entry (from_id={})",
                    edge.id, edge.from_id
                ));
            }
            if self
                .backend
                .get(&in_edge_key(edge.to_id, edge.id))?
                .is_none()
            {
                violations.push(format!(
                    "edge {} missing its in: adjacency entry (to_id={})",
                    edge.id, edge.to_id
                ));
            }
            // Edge endpoints must reference real nodes.
            if !nodes_by_id.contains_key(&edge.from_id) {
                violations.push(format!(
                    "edge {} references missing from_id={}",
                    edge.id, edge.from_id
                ));
            }
            if !nodes_by_id.contains_key(&edge.to_id) {
                violations.push(format!(
                    "edge {} references missing to_id={}",
                    edge.id, edge.to_id
                ));
            }
        }

        // ---- Invariant #3 — index consistency ----
        // node_uuid index
        for (key, value) in self.backend.scan_prefix(PREFIX_NODE_UUID)? {
            let id = u64_from_bytes(&value);
            if !nodes_by_id.contains_key(&id) {
                violations.push(format!(
                    "node_uuid index points at missing node id={id} (key len {})",
                    key.len()
                ));
            }
        }
        // node_title index — also asserts at most one entry per node
        let mut titles_seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for (_, value) in self.backend.scan_prefix(PREFIX_NODE_TITLE)? {
            let id = u64_from_bytes(&value);
            if !nodes_by_id.contains_key(&id) {
                violations.push(format!("node_title index points at missing node id={id}"));
            }
            if !titles_seen.insert(id) {
                violations.push(format!(
                    "node_title index has duplicate entries for node id={id}"
                ));
            }
        }
        // node_kind index
        for (key, _) in self.backend.scan_prefix(PREFIX_NODE_KIND)? {
            let id = id_from_kind_key(&key, b"node_kind:does_not_matter:");
            // The above is a hack to reuse the same suffix decoder — but we
            // need to find the actual node_id. Easier: tail 8 bytes.
            let id = if key.len() >= 8 {
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&key[key.len() - 8..]);
                u64::from_le_bytes(arr)
            } else {
                id
            };
            if !nodes_by_id.contains_key(&id) {
                violations.push(format!("node_kind index points at missing node id={id}"));
            }
        }
        // edge_uuid index
        for (_, value) in self.backend.scan_prefix(PREFIX_EDGE_UUID)? {
            let id = u64_from_bytes(&value);
            if !edges_by_id.contains_key(&id) {
                violations.push(format!("edge_uuid index points at missing edge id={id}"));
            }
        }
        // edge_kind index — extract trailing 8 bytes as edge id
        for (key, _) in self.backend.scan_prefix(PREFIX_EDGE_KIND)? {
            if key.len() < 8 {
                continue;
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&key[key.len() - 8..]);
            let id = u64::from_le_bytes(arr);
            if !edges_by_id.contains_key(&id) {
                violations.push(format!("edge_kind index points at missing edge id={id}"));
            }
        }

        // ---- Invariant #4 — updated_idx parity ----
        let mut updated_seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for (key, _) in self.backend.scan_prefix(PREFIX_UPDATED)? {
            let id = node_id_from_updated_key(&key);
            if !nodes_by_id.contains_key(&id) {
                violations.push(format!("updated_idx points at missing node id={id}"));
            }
            if !updated_seen.insert(id) {
                violations.push(format!(
                    "updated_idx has duplicate entries for node id={id}"
                ));
            }
        }
        for node in nodes_by_id.values() {
            if !updated_seen.contains(&node.id) {
                violations.push(format!("node {} has no entry in updated_idx", node.id));
            }
        }

        Ok(violations)
    }

    // ---------------------------------------------------------------
    // Internal helpers
    // ---------------------------------------------------------------

    /// Collect outgoing edges for a node by scanning the `out:` prefix.
    fn outgoing_edges(&self, node_id: u64) -> Result<Vec<Edge>> {
        let prefix = out_prefix(node_id);
        let entries = self.backend.scan_prefix(&prefix)?;
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
        let entries = self.backend.scan_prefix(&prefix)?;
        let mut edges = Vec::with_capacity(entries.len());
        for (key, _) in entries {
            let edge_id = edge_id_from_adjacency_key(&key, &prefix);
            if let Some(edge) = self.get_edge(edge_id)? {
                edges.push(edge);
            }
        }
        Ok(edges)
    }

    /// Load auto-increment counters with a max-scan recovery pass.
    ///
    /// Phase 9 task `00053`. Returns `(next_node_id, next_edge_id,
    /// drift_repaired)`. The persisted `meta:next_*_id` values are read
    /// first (default to 1 if missing); then the `node:` and `edge:`
    /// prefixes are scanned to find the highest id already stored. The
    /// effective counter is `max(persisted, max_stored + 1)` — so if a
    /// process is killed between two `create_node` calls (before
    /// `close()` ever persists the bumped counter), the next `Drevo::open`
    /// still hands out a fresh id instead of colliding with an already-
    /// stored row. `drift_repaired` reflects whether the rescan had to
    /// clamp the counter upward; it surfaces through
    /// [`IntegrityReport::counter_drift_repaired`].
    #[cfg(feature = "redb-backend")]
    fn load_counters(backend: &dyn StorageBackend) -> Result<(u64, u64, bool)> {
        let persisted_node = match backend.get(META_NEXT_NODE_ID)? {
            Some(bytes) => u64_from_bytes(&bytes),
            None => 1,
        };
        let persisted_edge = match backend.get(META_NEXT_EDGE_ID)? {
            Some(bytes) => u64_from_bytes(&bytes),
            None => 1,
        };

        let max_node_id = Self::scan_max_id(backend, PREFIX_NODE)?;
        let max_edge_id = Self::scan_max_id(backend, PREFIX_EDGE)?;

        // The on-disk rows are the source of truth — the persisted
        // counter is a hint. After a crash the persisted counter may be
        // stale (only `close()` writes it); the rescan ensures the next
        // allocation cannot collide with an existing id.
        let next_node = std::cmp::max(persisted_node, max_node_id.map_or(1, |m| m + 1));
        let next_edge = std::cmp::max(persisted_edge, max_edge_id.map_or(1, |m| m + 1));
        let drift_repaired = next_node > persisted_node || next_edge > persisted_edge;

        Ok((next_node, next_edge, drift_repaired))
    }

    /// Scan the given prefix (one of `PREFIX_NODE` / `PREFIX_EDGE`) and
    /// return the highest 8-byte little-endian id appended after the
    /// prefix — the recovery primitive used by `load_counters`.
    ///
    /// `node:` and `edge:` are substrings of `node_uuid:` / `edge_uuid:`
    /// etc., so the per-key length guard isolates the data rows from the
    /// secondary-index rows. Returns `None` on an empty database.
    #[cfg(feature = "redb-backend")]
    fn scan_max_id(backend: &dyn StorageBackend, prefix: &[u8]) -> Result<Option<u64>> {
        let entries = backend.scan_prefix(prefix)?;
        let mut max_id: Option<u64> = None;
        for (key, _) in entries {
            if key.len() != prefix.len() + 8 {
                continue;
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&key[prefix.len()..]);
            let id = u64::from_le_bytes(arr);
            max_id = Some(max_id.map_or(id, |m| m.max(id)));
        }
        Ok(max_id)
    }

    /// Run an integrity scan and return a structured [`IntegrityReport`].
    ///
    /// Phase 9 task `00053`. Scans every secondary index and the
    /// adjacency lists for orphan / dangling entries, counts corrupt
    /// node / edge payloads, and reports whether `Drevo::open` had to
    /// repair counter drift on this open. The report is data, not a
    /// hard error — a caller decides whether a non-clean report blocks
    /// startup or is logged for an operator.
    ///
    /// Cost: O(N) over every key in the backend (one `scan_prefix` per
    /// index family). Run after a known-bad crash, not on the hot path.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::Storage`] on backend failure. Corrupt
    /// `node:` / `edge:` payloads are counted in the report — they do
    /// *not* short-circuit the scan, so a single corrupt row never hides
    /// downstream issues.
    pub fn check_integrity(&self) -> Result<IntegrityReport> {
        // 1) Collect node ids by scanning the `node:` prefix. Reject keys
        //    of unexpected length (those are `node_uuid:` / `node_title:`
        //    / `node_kind:` rows that share the "node" string).
        let mut node_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut max_node_id: Option<u64> = None;
        let mut corrupt_node_rows: u64 = 0;
        for (key, bytes) in self.backend.scan_prefix(PREFIX_NODE)? {
            if key.len() != PREFIX_NODE.len() + 8 {
                continue;
            }
            // Decode payload to surface corruption — but trust the key
            // for id extraction so a corrupt payload still contributes
            // to the max-id calculation.
            if deserialize_node(&bytes).is_err() {
                corrupt_node_rows += 1;
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&key[PREFIX_NODE.len()..]);
            let id = u64::from_le_bytes(arr);
            node_ids.insert(id);
            max_node_id = Some(max_node_id.map_or(id, |m| m.max(id)));
        }

        // 2) Collect edges (and their endpoints) so we can flag dangling
        //    references.
        let mut edge_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut max_edge_id: Option<u64> = None;
        let mut corrupt_edge_rows: u64 = 0;
        let mut dangling_edge_endpoints: u64 = 0;
        for (key, bytes) in self.backend.scan_prefix(PREFIX_EDGE)? {
            if key.len() != PREFIX_EDGE.len() + 8 {
                continue;
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&key[PREFIX_EDGE.len()..]);
            let id = u64::from_le_bytes(arr);
            edge_ids.insert(id);
            max_edge_id = Some(max_edge_id.map_or(id, |m| m.max(id)));
            match deserialize_edge(&bytes) {
                Ok(edge) => {
                    if !node_ids.contains(&edge.from_id) {
                        dangling_edge_endpoints += 1;
                    }
                    if !node_ids.contains(&edge.to_id) && edge.from_id != edge.to_id {
                        dangling_edge_endpoints += 1;
                    } else if !node_ids.contains(&edge.to_id) {
                        // self-loop with both endpoints missing: count once.
                    }
                }
                Err(_) => corrupt_edge_rows += 1,
            }
        }

        // 3) Secondary indexes — count orphans.
        let mut orphan_node_kind_entries: u64 = 0;
        for (key, _) in self.backend.scan_prefix(PREFIX_NODE_KIND)? {
            // node_kind:{kind}:{node_id_8} — the trailing 8 bytes are the
            // node id when the key has at least that many bytes after the
            // prefix and a `:` separator. Decode tail-first; any key
            // shorter than expected is an unrelated row.
            if key.len() < PREFIX_NODE_KIND.len() + 1 + 8 {
                continue;
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&key[key.len() - 8..]);
            let id = u64::from_le_bytes(arr);
            if !node_ids.contains(&id) {
                orphan_node_kind_entries += 1;
            }
        }

        let mut orphan_node_title_entries: u64 = 0;
        for (_key, value) in self.backend.scan_prefix(PREFIX_NODE_TITLE)? {
            let id = u64_from_bytes(&value);
            if !node_ids.contains(&id) {
                orphan_node_title_entries += 1;
            }
        }

        let mut orphan_node_uuid_entries: u64 = 0;
        for (_key, value) in self.backend.scan_prefix(PREFIX_NODE_UUID)? {
            let id = u64_from_bytes(&value);
            if !node_ids.contains(&id) {
                orphan_node_uuid_entries += 1;
            }
        }

        let mut orphan_edge_kind_entries: u64 = 0;
        for (key, _) in self.backend.scan_prefix(PREFIX_EDGE_KIND)? {
            if key.len() < PREFIX_EDGE_KIND.len() + 1 + 8 {
                continue;
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&key[key.len() - 8..]);
            let id = u64::from_le_bytes(arr);
            if !edge_ids.contains(&id) {
                orphan_edge_kind_entries += 1;
            }
        }

        let mut orphan_edge_uuid_entries: u64 = 0;
        for (_key, value) in self.backend.scan_prefix(PREFIX_EDGE_UUID)? {
            let id = u64_from_bytes(&value);
            if !edge_ids.contains(&id) {
                orphan_edge_uuid_entries += 1;
            }
        }

        // 4) Adjacency lists — orphan if the referenced edge id is gone.
        let mut orphan_adjacency_entries: u64 = 0;
        for prefix in [PREFIX_OUT, PREFIX_IN] {
            for (key, _) in self.backend.scan_prefix(prefix)? {
                // {prefix}{node_id_8}:{edge_id_8}
                let expected = prefix.len() + 8 + 1 + 8;
                if key.len() != expected {
                    continue;
                }
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&key[key.len() - 8..]);
                let edge_id = u64::from_le_bytes(arr);
                if !edge_ids.contains(&edge_id) {
                    orphan_adjacency_entries += 1;
                }
            }
        }

        Ok(IntegrityReport {
            node_count: node_ids.len() as u64,
            edge_count: edge_ids.len() as u64,
            max_node_id,
            max_edge_id,
            next_node_id: self.next_node_id.load(Ordering::Relaxed),
            next_edge_id: self.next_edge_id.load(Ordering::Relaxed),
            counter_drift_repaired: self.counter_drift_repaired.load(Ordering::Relaxed),
            orphan_node_kind_entries,
            orphan_node_title_entries,
            orphan_node_uuid_entries,
            orphan_edge_kind_entries,
            orphan_edge_uuid_entries,
            orphan_adjacency_entries,
            dangling_edge_endpoints,
            corrupt_node_rows,
            corrupt_edge_rows,
        })
    }

    /// Load auto-increment counters from storage metadata — legacy entry
    /// retained for tests that pre-date the recovery rescan. Returns just
    /// the persisted counters, no rescan.
    #[cfg(all(feature = "redb-backend", test))]
    #[allow(dead_code)]
    fn load_counters_persisted_only(backend: &dyn StorageBackend) -> Result<(u64, u64)> {
        let node_id = match backend.get(META_NEXT_NODE_ID)? {
            Some(bytes) => u64_from_bytes(&bytes),
            None => 1,
        };
        let edge_id = match backend.get(META_NEXT_EDGE_ID)? {
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
            .put(META_NEXT_NODE_ID, &node_id.to_le_bytes())?;
        self.backend
            .put(META_NEXT_EDGE_ID, &edge_id.to_le_bytes())?;
        Ok(())
    }
}

/// Decode a u64 from little-endian bytes, defaulting to 1 on invalid input.
///
/// Refactored in `00106` to eliminate `.unwrap()` from library code
/// (`drevo-rust` §"Error Handling" + `drevo-architecture` anti-pattern #5).
/// The previous implementation called `bytes.try_into().unwrap()` after a
/// `bytes.len() == 8` guard — provably unreachable in practice, but still a
/// rule violation. The new form uses `copy_from_slice` into a pre-allocated
/// array, which is panic-free by construction.
fn u64_from_bytes(bytes: &[u8]) -> u64 {
    let mut arr = [0u8; 8];
    if bytes.len() == 8 {
        arr.copy_from_slice(bytes);
        u64::from_le_bytes(arr)
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

/// Decode the first u64 from an `out:`/`in:` adjacency key.
///
/// Format: `{prefix}{first_id_8}:{second_id_8}` — this helper returns
/// `first_id` (the indexed-from node for `out:`, the indexed-to node for
/// `in:`). Panic-free per `drevo-rust` §"Error Handling".
fn u64_from_adjacency_key_first_id(key: &[u8], prefix: &[u8]) -> u64 {
    let start = prefix.len();
    let end = start + 8;
    if key.len() < end {
        return 0;
    }
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&key[start..end]);
    u64::from_le_bytes(arr)
}

/// Extract the edge ID from an adjacency key by stripping the prefix.
///
/// Panic-free per `drevo-rust` §"Error Handling" — uses `copy_from_slice`
/// instead of `try_into().unwrap()` even though the length guard makes the
/// previous form unreachable.
fn edge_id_from_adjacency_key(key: &[u8], prefix: &[u8]) -> u64 {
    let suffix = &key[prefix.len()..];
    let mut arr = [0u8; 8];
    if suffix.len() == 8 {
        arr.copy_from_slice(suffix);
        u64::from_le_bytes(arr)
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
///
/// Panic-free per `drevo-rust` §"Error Handling".
fn node_id_from_updated_key(key: &[u8]) -> u64 {
    // Format: PREFIX_UPDATED (8) + inverted_ts (8) + ':' (1) + node_id (8)
    let offset = PREFIX_UPDATED.len() + 8 + 1;
    if key.len() < offset {
        return 0;
    }
    let suffix = &key[offset..];
    let mut arr = [0u8; 8];
    if suffix.len() == 8 {
        arr.copy_from_slice(suffix);
        u64::from_le_bytes(arr)
    } else {
        0
    }
}

/// Extract the ID (u64) from a kind index key by stripping the prefix.
///
/// Panic-free per `drevo-rust` §"Error Handling".
fn id_from_kind_key(key: &[u8], prefix: &[u8]) -> u64 {
    if key.len() < prefix.len() {
        return 0;
    }
    let suffix = &key[prefix.len()..];
    let mut arr = [0u8; 8];
    if suffix.len() == 8 {
        arr.copy_from_slice(suffix);
        u64::from_le_bytes(arr)
    } else {
        0
    }
}

/// Serialize an edge to bincode bytes.
fn serialize_edge(edge: &Edge) -> Result<Vec<u8>> {
    Ok(bincode::serde::encode_to_vec(edge, BINCODE_CONFIG)?)
}

/// Deserialize an edge from bincode bytes.
fn deserialize_edge(bytes: &[u8]) -> Result<Edge> {
    let (edge, _) = bincode::serde::decode_from_slice(bytes, BINCODE_CONFIG)?;
    Ok(edge)
}

/// Serialize a node to bincode bytes.
fn serialize_node(node: &Node) -> Result<Vec<u8>> {
    Ok(bincode::serde::encode_to_vec(node, BINCODE_CONFIG)?)
}

/// Deserialize a node from bincode bytes.
fn deserialize_node(bytes: &[u8]) -> Result<Node> {
    let (node, _) = bincode::serde::decode_from_slice(bytes, BINCODE_CONFIG)?;
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
        let mut db = Drevo::open_in_memory().unwrap();
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
        let mut db = Drevo::open(&path).unwrap();
        let report = db.compact().unwrap();
        // Disk-backed → both sizes measurable, bytes_after <= bytes_before.
        assert!(report.bytes_before.is_some());
        assert!(report.bytes_after.is_some());
        assert!(report.bytes_after.unwrap() <= report.bytes_before.unwrap());
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

    // -----------------------------------------------------------------
    // Explicit transactions (Phase 11 task `00072`).
    //
    // These inline tests pin the `tx_begin` / `tx_commit` / `tx_rollback`
    // contract before the Bolt session machinery exercises it across the
    // wire — they are the cheapest reproducer for any future regression
    // in the journal-replay path.
    // -----------------------------------------------------------------

    fn sample_node(title: &str) -> NewNode {
        NewNode {
            kind: "note".into(),
            title: title.into(),
            body: "body".into(),
            body_html: String::new(),
            properties: Default::default(),
        }
    }

    #[test]
    fn tx_begin_then_commit_returns_to_idle() {
        let db = Drevo::open_in_memory().unwrap();
        assert!(!db.is_tx_active());
        db.tx_begin().unwrap();
        assert!(db.is_tx_active());
        db.tx_commit().unwrap();
        assert!(!db.is_tx_active());
    }

    #[test]
    fn tx_begin_twice_in_a_row_is_rejected() {
        let db = Drevo::open_in_memory().unwrap();
        db.tx_begin().unwrap();
        let err = db.tx_begin().unwrap_err();
        assert!(matches!(err, DrevoError::TransactionAlreadyActive));
    }

    #[test]
    fn tx_commit_without_begin_is_rejected() {
        let db = Drevo::open_in_memory().unwrap();
        let err = db.tx_commit().unwrap_err();
        assert!(matches!(err, DrevoError::NoActiveTransaction));
    }

    #[test]
    fn tx_rollback_without_begin_is_rejected() {
        let db = Drevo::open_in_memory().unwrap();
        let err = db.tx_rollback().unwrap_err();
        assert!(matches!(err, DrevoError::NoActiveTransaction));
    }

    #[test]
    fn tx_rollback_undoes_create_node() {
        let db = Drevo::open_in_memory().unwrap();
        db.tx_begin().unwrap();
        let node = db.create_node(sample_node("alpha")).unwrap();
        assert!(db.get_node(node.id).unwrap().is_some());
        db.tx_rollback().unwrap();
        assert!(db.get_node(node.id).unwrap().is_none());
        // Title index also rolled back so the title is free again.
        let again = db.create_node(sample_node("alpha")).unwrap();
        assert_eq!(again.title, "alpha");
    }

    #[test]
    fn tx_commit_keeps_create_node() {
        let db = Drevo::open_in_memory().unwrap();
        db.tx_begin().unwrap();
        let node = db.create_node(sample_node("alpha")).unwrap();
        db.tx_commit().unwrap();
        assert!(db.get_node(node.id).unwrap().is_some());
    }

    #[test]
    fn tx_rollback_undoes_update_node_restoring_title_and_properties() {
        let db = Drevo::open_in_memory().unwrap();
        let node = db.create_node(sample_node("alpha")).unwrap();
        db.tx_begin().unwrap();
        let patch = NodePatch {
            title: Some("alpha2".into()),
            body: Some("changed".into()),
            ..Default::default()
        };
        db.update_node(node.id, patch).unwrap();
        let mid = db.get_node(node.id).unwrap().unwrap();
        assert_eq!(mid.title, "alpha2");
        db.tx_rollback().unwrap();
        let after = db.get_node(node.id).unwrap().unwrap();
        assert_eq!(after.title, "alpha");
        assert_eq!(after.body, "body");
        // Old title index also restored — looking it up returns the node.
        let by_title = db.get_node_by_title("alpha").unwrap().unwrap();
        assert_eq!(by_title.id, node.id);
        // New title was freed.
        assert!(db.get_node_by_title("alpha2").unwrap().is_none());
    }

    #[test]
    fn tx_rollback_undoes_delete_node_and_restores_cascade_edges() {
        let db = Drevo::open_in_memory().unwrap();
        let a = db.create_node(sample_node("a")).unwrap();
        let b = db.create_node(sample_node("b")).unwrap();
        let e = db
            .create_edge(NewEdge {
                from_id: a.id,
                to_id: b.id,
                kind: "links_to".into(),
                weight: 1.0,
                properties: Default::default(),
            })
            .unwrap();
        db.tx_begin().unwrap();
        db.delete_node(a.id).unwrap();
        // Mid-tx: A and its edge are gone.
        assert!(db.get_node(a.id).unwrap().is_none());
        assert!(db.get_edge(e.id).unwrap().is_none());
        db.tx_rollback().unwrap();
        let restored_a = db.get_node(a.id).unwrap().expect("A re-created");
        assert_eq!(restored_a.title, "a");
        let restored_e = db.get_edge(e.id).unwrap().expect("edge re-created");
        assert_eq!(restored_e.from_id, a.id);
        assert_eq!(restored_e.to_id, b.id);
        // Adjacency restored too.
        let out = db.edges_of(a.id, Direction::Outgoing).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, e.id);
    }

    #[test]
    fn tx_rollback_undoes_mixed_create_update_delete_chain() {
        let db = Drevo::open_in_memory().unwrap();
        let pre = db.create_node(sample_node("keep")).unwrap();
        db.tx_begin().unwrap();
        let _new = db.create_node(sample_node("tmp")).unwrap();
        let patch = NodePatch {
            body: Some("twiddled".into()),
            ..Default::default()
        };
        db.update_node(pre.id, patch).unwrap();
        db.delete_node(pre.id).unwrap();
        db.tx_rollback().unwrap();
        // Everything pre-tx is back; everything tx-only is gone.
        let restored = db.get_node(pre.id).unwrap().unwrap();
        assert_eq!(restored.title, "keep");
        assert_eq!(restored.body, "body");
        assert!(db.get_node_by_title("tmp").unwrap().is_none());
    }

    #[test]
    fn tx_rollback_undoes_create_edge_and_update_edge() {
        let db = Drevo::open_in_memory().unwrap();
        let a = db.create_node(sample_node("a")).unwrap();
        let b = db.create_node(sample_node("b")).unwrap();
        let e = db
            .create_edge(NewEdge {
                from_id: a.id,
                to_id: b.id,
                kind: "links_to".into(),
                weight: 1.0,
                properties: Default::default(),
            })
            .unwrap();
        db.tx_begin().unwrap();
        let e2 = db
            .create_edge(NewEdge {
                from_id: b.id,
                to_id: a.id,
                kind: "reply".into(),
                weight: 2.0,
                properties: Default::default(),
            })
            .unwrap();
        let patch = EdgePatch {
            kind: Some("renamed".into()),
            weight: Some(7.0),
            properties: None,
        };
        db.update_edge(e.id, patch).unwrap();
        db.tx_rollback().unwrap();
        assert!(db.get_edge(e2.id).unwrap().is_none());
        let original = db.get_edge(e.id).unwrap().unwrap();
        assert_eq!(original.kind, "links_to");
        assert!((original.weight - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn rollback_does_not_re_journal_replayed_mutations() {
        let db = Drevo::open_in_memory().unwrap();
        db.tx_begin().unwrap();
        db.create_node(sample_node("alpha")).unwrap();
        // Rollback walks the journal, calling delete_node. If that call
        // re-journaled itself we would end up looping; the
        // RollingBack-state guard documented on `tx_state` prevents it.
        db.tx_rollback().unwrap();
        // Slot must be Idle again — a follow-up tx_begin succeeds.
        db.tx_begin().unwrap();
        db.tx_commit().unwrap();
    }
}
