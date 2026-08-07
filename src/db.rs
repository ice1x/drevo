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
use crate::fts::edge_index;
use crate::fts::facet::{build_facets, Facet, FacetCollapse};
use crate::fts::index as fts_index;
use crate::fts::tokenizer::extract_trigrams;
use crate::model::{
    Direction, Edge, EdgePatch, FtsRanking, NewEdge, NewNode, Node, NodePatch, Properties,
    ScoredEdge, ScoredNode, SubGraph,
};
use crate::property_index;
use crate::semantic_index::{IndexError, IndexMode, SemanticIndex, SemanticIndexRegistry};
#[cfg(feature = "redb-backend")]
use crate::storage::RedbBackend;
use crate::storage::{MemoryBackend, StorageBackend};
use crate::vector::store as vector_store;
use crate::vector::{HnswConfig, HnswIndex, Vector};

/// Meta key for the next node ID counter.
const META_NEXT_NODE_ID: &[u8] = b"meta:next_node_id";

/// Meta key for the next edge ID counter.
const META_NEXT_EDGE_ID: &[u8] = b"meta:next_edge_id";

/// Meta key for the persisted Phase 21 semantic-index registry (#251) — a
/// JSON blob so `drevo.semantic.register` targets survive a restart.
const META_SEMANTIC_REGISTRY: &[u8] = b"meta:semantic_registry";

/// Meta key for the persisted #266 relationship semantic-index registry — the
/// edge-side mirror of [`META_SEMANTIC_REGISTRY`].
const META_SEMANTIC_REL_REGISTRY: &[u8] = b"meta:semantic_rel_registry";

/// The reserved property key holding a node's secondary `:Label`s (mirrors the
/// private constant in [`crate::cypher::executor`]). #251 slice 4 matches
/// auto-embed targets against these in addition to the primary `kind`; #263's
/// pending-backlog scan (compiled without `http` too) uses it as well.
const SECONDARY_LABELS_KEY: &str = "_labels";

/// Outcome of one [`Drevo::semantic_reindex`] backfill pass (#262).
///
/// Backs the `drevo.semantic.reindex` procedure. The counts let a client drive
/// the backfill to completion: keep calling while `remaining > 0`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SemanticReindexReport {
    /// Nodes of the target label examined this pass.
    pub scanned: usize,
    /// Nodes embedded this pass (text present, embedding written).
    pub embedded: usize,
    /// Nodes skipped — already carrying the embedding, or no text to embed.
    pub skipped: usize,
    /// Candidates still needing embedding after this pass: those left when
    /// `batch_size` was reached, plus any whose embed attempt failed this pass.
    /// A client re-runs `reindex` until this reaches zero.
    pub remaining: usize,
}

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

/// Key prefix for outgoing adjacency. v2 layout (#243 slice 2):
/// `out:{from_id}:{kind}:{edge_id}` -> `(to_id, kind)`.
const PREFIX_OUT: &[u8] = b"out:";

/// Key prefix for incoming adjacency. v2 layout (#243 slice 2):
/// `in:{to_id}:{kind}:{edge_id}` -> `(from_id, kind)`.
const PREFIX_IN: &[u8] = b"in:";

/// Current adjacency-index layout **major** version this build reads and
/// writes (#243 slice 2 — the kind-in-key layout). Kept in lockstep with the
/// redb on-disk [`crate::storage::redb::FORMAT_MAJOR`]; a
/// `debug_assert`-backed test pins them equal. A database whose adjacency
/// index is an older major is refused by [`Drevo::open`] with
/// [`DrevoError::NeedsMigration`] until [`Drevo::migrate_adjacency`] upgrades
/// it.
const ADJ_FORMAT_MAJOR: u32 = 2;

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
    /// Phase 21 semantic-index control plane (#251) — the registry of
    /// auto-embedding targets, reachable over Cypher via the
    /// `drevo.semantic.*` procedures. Persisted to the
    /// `meta:semantic_registry` key so registrations survive a restart
    /// ([`Self::load_semantic_registry`] / [`Self::persist_semantic_registry`]).
    /// Guarded by a `Mutex` so the `&Drevo` shared by the executor / Bolt
    /// sessions can register and introspect targets through interior mutability.
    semantic: Mutex<SemanticIndexRegistry>,
    /// #251 slice 3 — the server-side query embedder backing
    /// `drevo.semantic.query`. Installed once at server startup from
    /// `EmbeddingsConfig` (see [`Self::set_embedder`]); until then the handle
    /// holds `None` and `drevo.semantic.query` reports "not configured".
    ///
    /// A `OnceLock` (not a `Mutex`) because it is written exactly once and only
    /// ever read afterwards, so a `&Drevo` shared by the executor / Bolt
    /// sessions can consult it lock-free. Feature-gated on `http`: the embedder
    /// type lives in the `http`-gated [`crate::embeddings`] module, and only the
    /// HTTP/Bolt server ever installs one.
    #[cfg(feature = "http")]
    embedder: std::sync::OnceLock<std::sync::Arc<dyn crate::embeddings::TextEmbedder>>,
    /// #263 — cumulative auto-embed failures per target, keyed by
    /// `(label, embedding_property)`.
    ///
    /// Auto-embed is intentionally fail-open (a transient embedder outage never
    /// fails a write, #261), but that hid the degraded condition from clients.
    /// Every swallowed failure is tallied here so `drevo.semantic.status` can
    /// surface `failed_count` / `last_error`, letting a client detect that
    /// writes landed with no embedding. Runtime-only (not persisted); guarded by
    /// a `Mutex` for the shared `&Drevo`. Feature-gated on `http` like the
    /// embedder that produces these failures.
    #[cfg(feature = "http")]
    embed_failures: Mutex<std::collections::HashMap<(String, String, String), EmbedFailureStat>>,
    /// #267 — cached embedding vector dimension, discovered by a one-off probe
    /// the first time `drevo.semantic.info` is asked (embedding one short string
    /// and recording the result length). `None` until probed or when no
    /// embedder is installed. Feature-gated on `http` like the embedder.
    #[cfg(feature = "http")]
    embedder_dimension: Mutex<Option<usize>>,
    /// #266 — relationship-side mirror of [`Self::semantic`]: auto-embedding
    /// targets registered against a **relationship type** (rather than a node
    /// label), reachable over Cypher via the `drevo.semantic.*Rel` procedures.
    /// Kept in a separate registry (the `fts.search` / `fts.searchRelationships`
    /// split) and persisted to `meta:semantic_rel_registry`.
    rel_semantic: Mutex<SemanticIndexRegistry>,
}

/// Capability descriptor for the server-side embedder (#267), backing
/// `drevo.semantic.info`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbedderCapability {
    /// Whether a server-side embedder is actually installed and configured (not
    /// merely whether the procedures exist).
    pub present: bool,
    /// Configured model id, if known.
    pub model: Option<String>,
    /// Configured upstream endpoint URL, if known (never the API key).
    pub upstream: Option<String>,
    /// Embedding vector dimension, discovered by a one-off probe.
    pub dimension: Option<usize>,
}

impl EmbedderCapability {
    /// The "no embedder installed" descriptor: `present = false`, all-`None`.
    #[must_use]
    pub fn absent() -> Self {
        Self::default()
    }
}

/// A per-target tally of swallowed auto-embed failures (#263).
#[cfg(feature = "http")]
#[derive(Debug, Clone, Default)]
struct EmbedFailureStat {
    /// How many embed attempts have been swallowed for this target.
    count: u64,
    /// The most recent failure message, for `drevo.semantic.status`'s
    /// `last_error` column.
    last_error: String,
}

/// A registered semantic target plus its live health signals (#263), backing
/// the enriched `drevo.semantic.status` output.
#[derive(Debug, Clone)]
pub struct SemanticTargetStatus {
    /// Whether this target matches a node label (`"node"`) or a relationship
    /// type (`"relationship"`) — #266. For a node target `index.label` is the
    /// node label; for a relationship target it is the relationship type.
    pub target_kind: &'static str,
    /// The registered target (label, properties, mode, control-plane state).
    pub index: SemanticIndex,
    /// Auto-mode nodes of the label that still lack an embedding (a live
    /// backlog that `drevo.semantic.reindex` or a rewrite drains). Always 0 for
    /// `Manual` targets, which drevo does not embed.
    pub pending: usize,
    /// Cumulative count of swallowed auto-embed failures for this target.
    pub failed: u64,
    /// The most recent swallowed failure message, if any.
    pub last_error: Option<String>,
    /// True when `pending > 0` — writes have landed with embeddings missing, so
    /// semantic search under-returns until the backlog is drained.
    pub degraded: bool,
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

/// Storage-bloat snapshot (#253 slice 1) — the physical file footprint versus
/// the irreducible logical data it holds, so operators and automation can see
/// how much of a redb file is reclaimable copy-on-write high-water-mark bloat.
///
/// redb never returns freed pages to the OS on its own (see #240 / #241 /
/// #243): under churn the file grows to its high-water mark and only
/// [`Drevo::compact`] (or the `drevo shrink` CLI) reclaims it. The ratio is
/// measured against `stored_bytes` — records **plus** every secondary index —
/// precisely because a text-heavy graph's FTS index is a large but legitimate
/// cost: measuring against records alone would report such a file as massively
/// bloated when a rebuild cannot shrink it at all. A ratio near 1 is a minimal
/// file; a ratio well above 1 is genuine reclaimable slack. The follow-up
/// slices act on it (opt-in auto-compaction + a steady-state churn test).
///
/// `file_bytes` and `bloat_ratio` are `Option` because the ephemeral in-memory
/// backend has no on-disk footprint — they stay `None` rather than reporting a
/// fake zero.
///
/// Three byte totals are reported, coarse → fine:
/// - `stored_bytes` — **every** stored row (records + all secondary indexes),
///   the honest total of real data in the file. This is the ratio denominator.
/// - `logical_bytes` — just the `node:` + `edge:` record rows, comparable to a
///   GraphML dump.
/// - `index_bytes` — `stored_bytes − logical_bytes`, the secondary structures
///   (uuid / title / kind keys, adjacency, property index, FTS trigrams,
///   vectors). For text-heavy graphs the FTS index alone can dwarf the records,
///   so this is a large but entirely *legitimate* cost — not bloat.
///
/// `bloat_ratio = file_bytes / stored_bytes` is therefore the *reclaimable*
/// bloat signal: a value near 1 means the file is essentially minimal for its
/// data (compaction/rebuild cannot help), while a value well above 1 is
/// copy-on-write high-water-mark slack that [`Drevo::compact`] / `drevo shrink`
/// return to the OS. (An earlier version divided by `logical_bytes`, which
/// counted the legitimate index footprint as if it were bloat and grossly
/// over-reported.)
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BloatReport {
    /// Physical on-disk size of the backend file, or `None` for the ephemeral
    /// in-memory backend.
    pub file_bytes: Option<u64>,
    /// Summed size (key + value bytes) of **all** stored rows — records *and*
    /// every secondary index. The real logical data footprint, and the
    /// denominator of [`bloat_ratio`](Self::bloat_ratio).
    pub stored_bytes: u64,
    /// Summed size (key + value bytes) of the `node:` + `edge:` record rows —
    /// the irreducible graph data, excluding indexes.
    pub logical_bytes: u64,
    /// `stored_bytes − logical_bytes` — the secondary-index footprint
    /// (adjacency, uuid/title/kind keys, property index, FTS trigrams,
    /// vectors). Legitimate overhead, not reclaimable bloat.
    pub index_bytes: u64,
    /// Number of node records scanned.
    pub node_count: u64,
    /// Number of edge records scanned.
    pub edge_count: u64,
    /// `file_bytes / stored_bytes` — how many physical bytes back each byte of
    /// real stored data. `None` when the footprint is unmeasurable (in-memory
    /// backend) or there is no data yet (`stored_bytes == 0`). A value well
    /// above 1 signals reclaimable high-water-mark bloat; near 1 means the file
    /// is already minimal.
    pub bloat_ratio: Option<f64>,
}

/// Per-keyspace storage breakdown (#275 investigation): for each logical
/// keyspace prefix, how many rows it holds and their summed key+value bytes.
///
/// Physical bytes per prefix are not exposed by redb, but **entry count** is the
/// signal that matters for the FTS blowup: the FTS index stores one tiny row per
/// `(trigram, node_id)` pair, so on a text-heavy graph `fts` dwarfs every other
/// keyspace in row count — and redb's fixed per-row / per-page overhead on those
/// millions of near-empty rows is what inflates the physical file to several×
/// its content. This report makes that dominance measurable.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeyspaceStat {
    /// Human-readable keyspace label (the prefix without its trailing `:`).
    pub prefix: &'static str,
    /// Number of rows under this prefix.
    pub entries: u64,
    /// Summed key + value bytes of those rows (logical content, not physical).
    pub content_bytes: u64,
}

/// Opt-in policy for automatic compaction (#253 slice 2).
///
/// redb only reclaims high-water-mark bloat on an explicit `compact()`, which
/// needs **exclusive** access to the file (see [`Drevo::compact`]). The single
/// point where a [`Drevo`] handle is the sole owner of its backend is right
/// after [`Drevo::open`] builds it — before it is shared behind an `Arc`. This
/// policy lets that open path reclaim bloat automatically when a database has
/// grown past a configured ratio, so a churny long-lived store (the
/// agent-memory / KG workload) stays bounded across restarts instead of
/// climbing forever.
///
/// **Disabled by default** — bloat reclamation stays a deliberate opt-in
/// (`DREVO_AUTO_COMPACT=1`). See [`AutoCompactPolicy::from_env`] for the knobs.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoCompactPolicy {
    /// Whether automatic compaction is enabled at all. `false` by default.
    pub enabled: bool,
    /// Compact only when [`BloatReport::bloat_ratio`] is at least this. Guards
    /// against churning a file that is barely bloated.
    pub min_ratio: f64,
    /// Compact only when the physical file is at least this many bytes. Small
    /// files have a noisy ratio and little to reclaim, so they are skipped.
    pub min_bytes: u64,
}

impl Default for AutoCompactPolicy {
    fn default() -> Self {
        // Off by default; a 2× ratio and a 10 MiB floor when enabled.
        Self {
            enabled: false,
            min_ratio: 2.0,
            min_bytes: 10 * 1024 * 1024,
        }
    }
}

impl AutoCompactPolicy {
    /// Build a policy from environment-style configuration, using `get` to look
    /// up each key (mirrors the testable `from_env` pattern used elsewhere —
    /// pass `|k| std::env::var(k).ok()` in production).
    ///
    /// | Variable | Meaning | Default |
    /// |---|---|---|
    /// | `DREVO_AUTO_COMPACT` | enable (`1`/`true`/`yes`/`on`, case-insensitive) | off |
    /// | `DREVO_AUTO_COMPACT_RATIO` | minimum bloat ratio to trigger | `2.0` |
    /// | `DREVO_AUTO_COMPACT_MIN_BYTES` | minimum file size to consider | `10485760` (10 MiB) |
    ///
    /// Unparseable numeric values fall back to the default rather than failing —
    /// a misconfigured knob must not stop the database from opening.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Self {
        let default = Self::default();
        let enabled = get("DREVO_AUTO_COMPACT")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                matches!(v.as_str(), "1" | "true" | "yes" | "on")
            })
            .unwrap_or(false);
        let min_ratio = get("DREVO_AUTO_COMPACT_RATIO")
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|r| r.is_finite() && *r > 0.0)
            .unwrap_or(default.min_ratio);
        let min_bytes = get("DREVO_AUTO_COMPACT_MIN_BYTES")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(default.min_bytes);
        Self {
            enabled,
            min_ratio,
            min_bytes,
        }
    }
}

/// Direction of the #243 slice 2 adjacency-format migration, as passed to
/// [`Drevo::migrate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationDirection {
    /// Upgrade a legacy (v1) database to the current kind-in-key layout.
    /// After a successful `Up`, the file opens normally.
    Up,
    /// Downgrade a kind-in-key (v2) database back to the v1 layout so an
    /// older, pre-#243-slice-2 drevo build can read it again.
    Down,
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
        let mut db = Self::open_ungated(path)?;
        // #243 slice 2: refuse a database whose adjacency index predates the
        // kind-in-key layout, rather than silently misreading it. The graph
        // is never at risk — the fix is a reversible, index-only migration.
        if db.adjacency_needs_migration()? {
            let found_major = db.backend.format_major()?.unwrap_or(1);
            return Err(DrevoError::NeedsMigration {
                found_major,
                required_major: ADJ_FORMAT_MAJOR,
            });
        }
        // #253 slice 2: opt-in automatic compaction. This is the one moment the
        // handle solely owns its backend (refcount 1), satisfying compact()'s
        // exclusive-access requirement. Best-effort: the graph data is intact
        // whether or not the reclaim succeeds, so a maintenance failure must
        // never deny access — it is logged (when tracing is built in) and
        // swallowed rather than propagated.
        let policy = AutoCompactPolicy::from_env(|k| std::env::var(k).ok());
        if policy.enabled {
            match db.maybe_auto_compact(&policy) {
                Ok(Some(_report)) => {
                    #[cfg(feature = "http")]
                    tracing::info!(
                        reclaimed = _report.bytes_reclaimed,
                        "auto-compaction reclaimed storage on open"
                    );
                }
                Ok(None) => {}
                Err(_e) => {
                    #[cfg(feature = "http")]
                    tracing::warn!(error = %_e, "auto-compaction on open failed (ignored)");
                }
            }
        }
        Ok(db)
    }

    /// Open the redb-backed handle **without** the #243 slice 2 migration
    /// gate. Used by [`Self::open`] (which then runs the gate) and by
    /// [`Self::migrate`] (which must open a pre-migration file to upgrade it).
    #[cfg(feature = "redb-backend")]
    fn open_ungated(path: &Path) -> Result<Self> {
        let backend = RedbBackend::open(path)?;
        let backend = Box::new(backend);
        let (next_node_id, next_edge_id, drift_repaired) = Self::load_counters(&*backend)?;
        // Restore any persisted semantic-index registry (#251) so registered
        // auto-embedding targets survive a restart. The relationship registry
        // (#266) is restored the same way from its own meta key.
        let semantic = Self::load_semantic_registry(&*backend, META_SEMANTIC_REGISTRY);
        let rel_semantic = Self::load_semantic_registry(&*backend, META_SEMANTIC_REL_REGISTRY);
        Ok(Self {
            backend,
            next_node_id: AtomicU64::new(next_node_id),
            next_edge_id: AtomicU64::new(next_edge_id),
            counter_drift_repaired: AtomicBool::new(drift_repaired),
            tx_state: Mutex::new(TxState::Idle),
            semantic: Mutex::new(semantic),
            #[cfg(feature = "http")]
            embedder: std::sync::OnceLock::new(),
            #[cfg(feature = "http")]
            embed_failures: Mutex::new(std::collections::HashMap::new()),
            #[cfg(feature = "http")]
            embedder_dimension: Mutex::new(None),
            rel_semantic: Mutex::new(rel_semantic),
        })
    }

    /// Open a database that needs migration, run the #243 slice 2 adjacency
    /// migration in the requested `direction`, and return the number of edges
    /// re-indexed.
    ///
    /// `direction` is [`MigrationDirection::Up`] to upgrade a legacy file to
    /// the kind-in-key layout (the common case — after this the file opens
    /// normally) or [`MigrationDirection::Down`] to revert to the v1 layout so
    /// an older drevo build can read it again.
    ///
    /// The migration is **safe**: it rebuilds the derived `out:`/`in:`
    /// adjacency index from the intact node/edge records, so an interrupted
    /// run loses no graph data and simply resumes on the next call (the
    /// per-edge rewrite is idempotent). Callers SHOULD still take a GraphML
    /// backup first — the `drevo migrate` CLI does so automatically.
    ///
    /// # Availability
    ///
    /// Requires the `redb-backend` feature; the ephemeral in-memory backend is
    /// always current and never needs migration.
    #[cfg(feature = "redb-backend")]
    pub fn migrate(path: &Path, direction: MigrationDirection) -> Result<u64> {
        let db = Self::open_ungated(path)?;
        let to_major = match direction {
            MigrationDirection::Up => ADJ_FORMAT_MAJOR,
            MigrationDirection::Down => 1,
        };
        db.migrate_adjacency(to_major)
    }

    /// Whether the open database's adjacency index predates the current
    /// kind-in-key layout and must be migrated (#243 slice 2).
    ///
    /// Two independent signals are consulted so the gate is robust: the
    /// persisted format-version stamp (`< ADJ_FORMAT_MAJOR` ⇒ not yet
    /// migrated, and — because [`Self::migrate_adjacency`] stamps only on
    /// completion — also catches a half-finished migration), and a direct
    /// sample of an actual adjacency key (catches a pre-versioning file that
    /// the storage layer stamped current but whose keys are still v1).
    /// Backends without a durable stamp (in-memory) rely on the sample alone,
    /// which for a freshly written database is unambiguously v2.
    fn adjacency_needs_migration(&self) -> Result<bool> {
        if let Some(major) = self.backend.format_major()? {
            if major < ADJ_FORMAT_MAJOR {
                return Ok(true);
            }
        }
        for prefix in [PREFIX_OUT, PREFIX_IN] {
            let sample = self.backend.scan_prefix_limited(prefix, None, 1)?;
            if sample
                .first()
                .is_some_and(|(key, _)| adjacency_key_is_v1(key, prefix))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Rewrite the `out:`/`in:` adjacency index into the layout for
    /// `to_major` (2 = kind-in-key, 1 = legacy) and re-stamp the on-disk
    /// format version (#243 slice 2).
    ///
    /// The index is a projection of the edge records, so this rebuilds it from
    /// scratch: for every edge it deletes both possible key layouts (making
    /// the operation idempotent and direction-agnostic) and writes the target
    /// layout with the denormalized `(neighbor_id, kind)` value. The node and
    /// edge tables are never touched, so the graph cannot be lost even if the
    /// process is killed mid-run — the format stamp is written last, so an
    /// interrupted migration still reports "needs migration" and completes on
    /// the next call.
    ///
    /// Returns the number of edges re-indexed.
    pub fn migrate_adjacency(&self, to_major: u32) -> Result<u64> {
        let edge_entries = self.backend.scan_prefix(PREFIX_EDGE)?;
        let mut migrated = 0u64;
        for (key, bytes) in &edge_entries {
            if key.len() != PREFIX_EDGE.len() + 8 {
                continue; // not an edge record row
            }
            let edge = deserialize_edge(bytes)?;
            // Drop whichever layout currently holds this edge's entries.
            self.backend
                .delete(&out_edge_key_v1(edge.from_id, edge.id))?;
            self.backend.delete(&in_edge_key_v1(edge.to_id, edge.id))?;
            self.backend
                .delete(&out_edge_key(edge.from_id, &edge.kind, edge.id))?;
            self.backend
                .delete(&in_edge_key(edge.to_id, &edge.kind, edge.id))?;
            // Write the target layout with a fully denormalized value.
            let (out_key, in_key) = if to_major >= ADJ_FORMAT_MAJOR {
                (
                    out_edge_key(edge.from_id, &edge.kind, edge.id),
                    in_edge_key(edge.to_id, &edge.kind, edge.id),
                )
            } else {
                (
                    out_edge_key_v1(edge.from_id, edge.id),
                    in_edge_key_v1(edge.to_id, edge.id),
                )
            };
            self.backend
                .put(&out_key, &adjacency_value(edge.to_id, &edge.kind))?;
            self.backend
                .put(&in_key, &adjacency_value(edge.from_id, &edge.kind))?;
            migrated += 1;
        }
        self.backend.flush()?;
        // Stamp last: the on-disk version only advances once every edge is
        // re-indexed, so a crash before this point re-triggers the gate.
        self.backend.set_format_version(to_major, 0)?;
        Ok(migrated)
    }

    /// Register (or re-enable) a semantic-index target and return its current
    /// [`SemanticIndex`] record (#251 Phase 21 control plane).
    ///
    /// Backs the `drevo.semantic.register` Cypher procedure: it enables
    /// auto-embedding bookkeeping for `(label, embedding_property)` sourced from
    /// `text_property`. This slice records the intent in the in-memory registry;
    /// the actual server-side embedding is wired by a follow-up slice, so a
    /// registered target's `state` reflects the control plane, not embedding
    /// readiness.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError`] (e.g. `AlreadyEnabled`) from the underlying
    /// registry transition.
    pub fn semantic_register(
        &self,
        label: &str,
        text_property: &str,
        embedding_property: &str,
        mode: IndexMode,
        model: Option<String>,
    ) -> std::result::Result<SemanticIndex, IndexError> {
        let mut registry = self.semantic.lock().unwrap_or_else(|e| e.into_inner());
        let target = registry
            .enable(label, text_property, embedding_property, mode, model)
            .cloned()?;
        // Persist so the registration survives a restart (#251). Best-effort:
        // the in-memory registry is authoritative for the running server, and a
        // storage hiccup must not fail a control-plane call — the next
        // successful mutation re-persists. Ephemeral in-memory backends simply
        // drop the blob on close, which is the correct behaviour there.
        self.persist_semantic_registry(META_SEMANTIC_REGISTRY, &registry);
        Ok(target)
    }

    /// Register (or re-enable) a **relationship**-side auto-embedding target and
    /// return its current record (#266) — the edge mirror of
    /// [`Self::semantic_register`]. `rel_type` matches an edge's `kind`.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError`] from the underlying registry transition.
    pub fn semantic_register_rel(
        &self,
        rel_type: &str,
        text_property: &str,
        embedding_property: &str,
        mode: IndexMode,
        model: Option<String>,
    ) -> std::result::Result<SemanticIndex, IndexError> {
        let mut registry = self.rel_semantic.lock().unwrap_or_else(|e| e.into_inner());
        let target = registry
            .enable(rel_type, text_property, embedding_property, mode, model)
            .cloned()?;
        self.persist_semantic_registry(META_SEMANTIC_REL_REGISTRY, &registry);
        Ok(target)
    }

    /// Serialize a semantic-index registry to `key` (#251 / #266). Best-effort —
    /// a failure is logged (when tracing is built in) and swallowed rather than
    /// surfaced to the control-plane caller.
    fn persist_semantic_registry(&self, key: &[u8], registry: &SemanticIndexRegistry) {
        let bytes = match serde_json::to_vec(registry) {
            Ok(b) => b,
            Err(_e) => {
                #[cfg(feature = "http")]
                tracing::warn!(error = %_e, "failed to serialize semantic registry (ignored)");
                return;
            }
        };
        if let Err(_e) = self.backend.put(key, &bytes) {
            #[cfg(feature = "http")]
            tracing::warn!(error = %_e, "failed to persist semantic registry (ignored)");
        }
    }

    /// Load a persisted semantic-index registry from `key`, or an empty one when
    /// absent or unreadable (#251 / #266). Used by [`Self::open`].
    fn load_semantic_registry(backend: &dyn StorageBackend, key: &[u8]) -> SemanticIndexRegistry {
        match backend.get(key) {
            Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_default(),
            _ => SemanticIndexRegistry::new(),
        }
    }

    /// Snapshot every registered semantic-index target (#251 Phase 21).
    ///
    /// Backs the `drevo.semantic.status` Cypher procedure — lets a client
    /// introspect the control plane (which targets exist and their state) and
    /// branch on the capability's presence.
    pub fn semantic_status(&self) -> Vec<SemanticIndex> {
        self.semantic
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .list()
            .to_vec()
    }

    /// Every registered semantic target plus its live health signals (#263):
    /// pending backlog, cumulative failure count, last error, and a derived
    /// `degraded` flag. Backs the enriched `drevo.semantic.status` output so a
    /// client can tell "fully embedded" from "writes landed, embeddings
    /// missing".
    ///
    /// # Errors
    ///
    /// Propagates storage errors from the pending-backlog scan.
    pub fn semantic_status_detailed(&self) -> Result<Vec<SemanticTargetStatus>> {
        let node_targets = self.semantic_status();
        let rel_targets = self
            .rel_semantic
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .list()
            .to_vec();
        let mut out = Vec::with_capacity(node_targets.len() + rel_targets.len());
        for index in node_targets {
            out.push(self.status_for("node", index)?);
        }
        for index in rel_targets {
            out.push(self.status_for("relationship", index)?);
        }
        Ok(out)
    }

    /// Build the health row for one target of the given kind (`"node"` /
    /// `"relationship"`), scanning the matching backlog (#263 / #266).
    fn status_for(&self, kind: &'static str, index: SemanticIndex) -> Result<SemanticTargetStatus> {
        // Only Auto targets are drevo-managed, so only they have a backlog.
        let pending = if matches!(index.mode, IndexMode::Auto) {
            match kind {
                "relationship" => self.semantic_pending_count_rel(
                    &index.label,
                    &index.text_property,
                    &index.embedding_property,
                )?,
                _ => self.semantic_pending_count(
                    &index.label,
                    &index.text_property,
                    &index.embedding_property,
                )?,
            }
        } else {
            0
        };
        let (failed, last_error) =
            self.embed_failure_stat(kind, &index.label, &index.embedding_property);
        Ok(SemanticTargetStatus {
            target_kind: kind,
            index,
            pending,
            failed,
            last_error,
            degraded: pending > 0,
        })
    }

    /// Count nodes of `label` that carry a non-empty `text_property` but are
    /// still missing `embedding_property` — the live auto-embed backlog (#263).
    ///
    /// # Errors
    ///
    /// Propagates storage errors from the node scan.
    fn semantic_pending_count(
        &self,
        label: &str,
        text_property: &str,
        embedding_property: &str,
    ) -> Result<usize> {
        let mut pending = 0;
        for node in self.collect_all_nodes()? {
            let matches_label = node.kind == label
                || matches!(
                    node.properties.0.get(SECONDARY_LABELS_KEY),
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
        Ok(pending)
    }

    /// Relationship mirror of [`Self::semantic_pending_count`] (#266): edges of
    /// `rel_type` with a non-empty `text_property` but no `embedding_property`.
    ///
    /// # Errors
    ///
    /// Propagates storage errors from the edge scan.
    fn semantic_pending_count_rel(
        &self,
        rel_type: &str,
        text_property: &str,
        embedding_property: &str,
    ) -> Result<usize> {
        let mut pending = 0;
        for edge in self.collect_all_edges()? {
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
        Ok(pending)
    }

    /// Record a swallowed auto-embed failure for a target (#263), keyed by
    /// `kind` (`"node"` / `"relationship"`, #266) so a node label and a
    /// relationship type of the same name never collide. Called from the
    /// fail-open paths (write-path hooks and reindex).
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
    /// (#263). Returns `(0, None)` when nothing has failed — and, on a build
    /// without the `http` feature, always (there is no embedder to fail).
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

    /// Install the server-side text embedder used by `drevo.semantic.query`
    /// (#251 slice 3). Set-once: the first call installs the embedder and
    /// returns `true`; any later call is a no-op and returns `false`.
    ///
    /// Wired at server startup from `EmbeddingsConfig::from_env` (see
    /// `crate::server::run`). Takes `&self` — the handle is already `Arc`-shared
    /// by the catalog by the time the server configures it, so a set-once
    /// [`std::sync::OnceLock`] is the right primitive.
    #[cfg(feature = "http")]
    pub fn set_embedder(
        &self,
        embedder: std::sync::Arc<dyn crate::embeddings::TextEmbedder>,
    ) -> bool {
        self.embedder.set(embedder).is_ok()
    }

    /// Embed `text` into a query vector with the installed embedder (#251
    /// slice 3). Backs `drevo.semantic.query`: the executor embeds the query
    /// text here, then runs the same brute-force cosine scan as
    /// `drevo.vector.query`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::embeddings::EmbeddingsError::NotConfigured`] when no
    /// embedder has been installed (the common "server started without
    /// `DREVO_EMBEDDINGS_UPSTREAM`" case), or the embedder's own error when the
    /// upstream call or response parsing fails.
    #[cfg(feature = "http")]
    pub fn embed_text(
        &self,
        text: &str,
    ) -> std::result::Result<Vec<f32>, crate::embeddings::EmbeddingsError> {
        self.embedder
            .get()
            .ok_or(crate::embeddings::EmbeddingsError::NotConfigured)?
            .embed_query(text)
    }

    /// Capability introspection for the server-side embedder (#267).
    ///
    /// Reports whether an embedder is actually installed, and — when it is — the
    /// configured model id, upstream endpoint, and vector dimension, so a client
    /// can verify a server-side embedder is compatible with vectors it may write
    /// itself. Never exposes the upstream API key. The dimension is discovered
    /// by a one-off probe (embedding a short string) and cached; a failing probe
    /// leaves it `None` without erroring.
    ///
    /// Backs the `drevo.semantic.info` procedure. Without the `http` feature (or
    /// with no embedder installed) it reports `present = false` and all-`None`.
    pub fn embedder_info(&self) -> EmbedderCapability {
        #[cfg(feature = "http")]
        {
            let Some(embedder) = self.embedder.get() else {
                return EmbedderCapability::absent();
            };
            EmbedderCapability {
                present: true,
                model: embedder.model(),
                upstream: embedder.upstream(),
                dimension: self.embedder_dimension_probe(),
            }
        }
        #[cfg(not(feature = "http"))]
        {
            EmbedderCapability::absent()
        }
    }

    /// The embedding vector dimension, from cache or a one-off probe (#267).
    /// Swallows a probe failure (returns `None`) so `info` never fails.
    #[cfg(feature = "http")]
    fn embedder_dimension_probe(&self) -> Option<usize> {
        {
            let cached = self
                .embedder_dimension
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(dim) = *cached {
                return Some(dim);
            }
        }
        // Probe with a short, non-empty string (empty input is rejected upstream).
        let dim = self.embed_text("dimension probe").ok().map(|v| v.len());
        if let Some(dim) = dim {
            *self
                .embedder_dimension
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(dim);
        }
        dim
    }

    /// #251 slice 4 — apply server-side auto-embedding to a node's properties
    /// just before it is persisted.
    ///
    /// For every registered [`IndexMode::Auto`] target whose label matches this
    /// node (primary `kind` or a `_labels` secondary label), embed the text in
    /// the target's `text_property` and write the resulting vector into its
    /// `embedding_property`. This is what makes ingest "just work": a client
    /// `CREATE`s `(:Doc {text: …})` and the embedding appears without a
    /// separate `/v1/embeddings` round-trip, so `drevo.semantic.query` /
    /// `drevo.vector.query` can retrieve it immediately (issue #251 acceptance
    /// bullet: "on ingest/update, drevo embeds the configured properties
    /// server-side and keeps the vector index in sync").
    ///
    /// A deliberate double no-op keeps the common path untouched: it returns
    /// immediately when no embedder is installed (every non-server context —
    /// tests, CLI, an in-memory backend without `set_embedder`) and when no
    /// Auto target matches. `old` is the pre-patch property map on update; a
    /// target whose source text is unchanged (and whose embedding is already
    /// present) is skipped, so an unrelated update does not re-hit the upstream.
    /// An upstream failure is logged and swallowed — a transient embedder
    /// outage must never fail a write. The embedding call runs before any
    /// storage transaction is opened, so no lock is held across the network I/O.
    fn apply_auto_embeddings(
        &self,
        kind: &str,
        properties: &mut Properties,
        old: Option<&Properties>,
    ) {
        #[cfg(feature = "http")]
        {
            // Fast exit: no server-side embedder → nothing to do (the common
            // case for tests, the CLI, and any handle the server never wired).
            if self.embedder.get().is_none() {
                return;
            }
            let targets: Vec<(String, String, String)> = {
                let registry = self.semantic.lock().unwrap_or_else(|e| e.into_inner());
                registry
                    .list()
                    .iter()
                    .filter(|t| matches!(t.mode, IndexMode::Auto))
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
            if let Some(serde_json::Value::Array(arr)) = properties.0.get(SECONDARY_LABELS_KEY) {
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
                    continue; // no text to embed on this node
                };
                if text.is_empty() {
                    continue;
                }
                // On update, skip when the source text is unchanged and the
                // embedding already exists — avoid a redundant upstream call.
                if let Some(old) = old {
                    let unchanged = old.0.get(&text_prop).and_then(serde_json::Value::as_str)
                        == Some(text.as_str());
                    if unchanged && properties.0.contains_key(&emb_prop) {
                        continue;
                    }
                }
                match self.embed_text(&text) {
                    Ok(vector) => {
                        let arr = serde_json::Value::Array(
                            vector.into_iter().map(|f| serde_json::json!(f)).collect(),
                        );
                        properties.0.insert(emb_prop, arr);
                    }
                    Err(error) => {
                        tracing::warn!(label = %label, %error, "auto-embed failed (ignored)");
                        self.record_embed_failure("node", &label, &emb_prop, &error.to_string());
                    }
                }
            }
        }
        #[cfg(not(feature = "http"))]
        {
            let _ = (kind, properties, old);
        }
    }

    /// #266 — relationship mirror of [`Self::apply_auto_embeddings`]: apply
    /// server-side auto-embedding to an **edge's** properties before it is
    /// persisted. `rel_type` is the edge's `kind`; matching Auto-mode targets in
    /// the relationship registry embed `text_property` into `embedding_property`.
    /// Same double no-op and fail-open discipline as the node path.
    fn apply_auto_embeddings_edge(
        &self,
        rel_type: &str,
        properties: &mut Properties,
        old: Option<&Properties>,
    ) {
        #[cfg(feature = "http")]
        {
            if self.embedder.get().is_none() {
                return;
            }
            let targets: Vec<(String, String, String)> = {
                let registry = self.rel_semantic.lock().unwrap_or_else(|e| e.into_inner());
                registry
                    .list()
                    .iter()
                    .filter(|t| matches!(t.mode, IndexMode::Auto) && t.label == rel_type)
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
                    continue; // no text to embed on this edge
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
                match self.embed_text(&text) {
                    Ok(vector) => {
                        let arr = serde_json::Value::Array(
                            vector.into_iter().map(|f| serde_json::json!(f)).collect(),
                        );
                        properties.0.insert(emb_prop, arr);
                    }
                    Err(error) => {
                        tracing::warn!(rel_type = %rel, %error, "auto-embed (rel) failed (ignored)");
                        self.record_embed_failure(
                            "relationship",
                            &rel,
                            &emb_prop,
                            &error.to_string(),
                        );
                    }
                }
            }
        }
        #[cfg(not(feature = "http"))]
        {
            let _ = (rel_type, properties, old);
        }
    }

    /// #262 — backfill embeddings for **already-present** nodes of a semantic
    /// target, in resumable batches.
    ///
    /// Auto-embedding (#251 slice 4) only fires on create/update, so nodes that
    /// existed before a rule was registered stay un-embedded and invisible to
    /// `drevo.semantic.query`. This walks the nodes of `label`, embeds
    /// `text_property` into `embedding_property` for up to `batch_size` of those
    /// that still lack it, and reports counts so a client can loop until
    /// `remaining == 0`.
    ///
    /// The caller (the `drevo.semantic.reindex` procedure) resolves and
    /// validates the registered target first and passes its `text_property` /
    /// `embedding_property` here, so this method itself is registry-agnostic.
    ///
    /// Idempotent and resumable: a node that already carries `embedding_property`
    /// is skipped, so re-running is cheap and safe. A node whose text changed is
    /// re-embedded by the write path, not here. Each embed + persist is its own
    /// storage transaction, and the embedding call happens before that
    /// transaction opens — so, like the write-path hook, no lock is held across
    /// the network I/O. Nodes without a non-empty `text_property` are skipped; a
    /// failed embed is left for a later pass (counted in `remaining`).
    ///
    /// A no-op returning zeros when no embedder is installed.
    ///
    /// # Errors
    ///
    /// Propagates storage errors from scanning or persisting nodes.
    #[cfg(feature = "http")]
    pub fn semantic_reindex(
        &self,
        label: &str,
        text_property: &str,
        embedding_property: &str,
        batch_size: usize,
    ) -> Result<SemanticReindexReport> {
        let mut report = SemanticReindexReport::default();
        let embedder_ready = self.embedder.get().is_some();
        let mut budget = batch_size;

        for node in self.collect_all_nodes()? {
            // Match the node's primary kind plus any secondary `_labels`.
            let matches_label = node.kind == label
                || matches!(
                    node.properties.0.get(SECONDARY_LABELS_KEY),
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
                // No text to embed, or the embedding is already present.
                report.skipped += 1;
                continue;
            };

            // A candidate needing embedding.
            if budget == 0 || !embedder_ready {
                report.remaining += 1;
                continue;
            }
            match self.embed_text(&text) {
                Ok(vector) => {
                    let mut props = node.properties.clone();
                    let arr = serde_json::Value::Array(
                        vector.into_iter().map(|f| serde_json::json!(f)).collect(),
                    );
                    props.0.insert(embedding_property.to_string(), arr);
                    // Persist via the normal update path (keeps every index in
                    // sync). The auto-embed hook sees the text unchanged and the
                    // embedding already set, so it does not re-embed.
                    let patch = NodePatch {
                        properties: Some(props),
                        ..Default::default()
                    };
                    self.update_node(node.id, patch)?;
                    report.embedded += 1;
                    budget -= 1;
                }
                Err(error) => {
                    // Swallow like the write path; leave it for a later pass.
                    tracing::warn!(label, error = %error, "reindex embed failed (ignored)");
                    self.record_embed_failure(
                        "node",
                        label,
                        embedding_property,
                        &error.to_string(),
                    );
                    report.remaining += 1;
                }
            }
        }
        Ok(report)
    }

    /// #266 — relationship mirror of [`Self::semantic_reindex`]: backfill
    /// embeddings for already-present **edges** of `rel_type`, in resumable
    /// batches. Same idempotent / resumable / fail-open semantics as the node
    /// path; persists via [`Self::update_edge`] so every index stays in sync.
    ///
    /// # Errors
    ///
    /// Propagates storage errors from scanning or persisting edges.
    #[cfg(feature = "http")]
    pub fn semantic_reindex_rel(
        &self,
        rel_type: &str,
        text_property: &str,
        embedding_property: &str,
        batch_size: usize,
    ) -> Result<SemanticReindexReport> {
        let mut report = SemanticReindexReport::default();
        let embedder_ready = self.embedder.get().is_some();
        let mut budget = batch_size;

        for edge in self.collect_all_edges()? {
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

            if budget == 0 || !embedder_ready {
                report.remaining += 1;
                continue;
            }
            match self.embed_text(&text) {
                Ok(vector) => {
                    let mut props = edge.properties.clone();
                    let arr = serde_json::Value::Array(
                        vector.into_iter().map(|f| serde_json::json!(f)).collect(),
                    );
                    props.0.insert(embedding_property.to_string(), arr);
                    let patch = EdgePatch {
                        properties: Some(props),
                        ..Default::default()
                    };
                    self.update_edge(edge.id, patch)?;
                    report.embedded += 1;
                    budget -= 1;
                }
                Err(error) => {
                    tracing::warn!(rel_type, error = %error, "reindex (rel) embed failed (ignored)");
                    self.record_embed_failure(
                        "relationship",
                        rel_type,
                        embedding_property,
                        &error.to_string(),
                    );
                    report.remaining += 1;
                }
            }
        }
        Ok(report)
    }

    /// Physical on-disk size of the backend file in bytes, or `None` for the
    /// ephemeral in-memory backend (#253 slice 1).
    ///
    /// An O(1) file stat — cheap enough to sample on every metrics scrape,
    /// unlike the full [`Self::bloat_report`] scan.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::Storage`] if the backend size probe fails.
    pub fn file_bytes(&self) -> Result<Option<u64>> {
        Ok(self.backend.size_bytes()?)
    }

    /// Measure storage bloat (#253 slice 1): the physical file footprint
    /// versus the irreducible logical graph data, plus their ratio.
    ///
    /// Reads `file_bytes` from the backend's `size_bytes` (an O(1) file stat;
    /// `None` for the ephemeral in-memory backend), `stored_bytes` from the
    /// backend's `content_bytes` (the summed key+value length of every stored
    /// row — records *and* all secondary indexes), and `logical_bytes` by
    /// scanning just the `node:` + `edge:` record rows. `index_bytes` is the
    /// difference. `bloat_ratio = file_bytes / stored_bytes` surfaces how much
    /// of the file is reclaimable copy-on-write high-water-mark bloat that only
    /// [`Self::compact`] (or `drevo shrink`) returns to the OS — dividing by the
    /// full stored footprint (not just records) is what keeps a text-heavy,
    /// FTS-indexed graph from reading as bloated when it is merely index-rich.
    ///
    /// Cost: one full-keyspace content scan (streamed on disk) plus a scan of
    /// the `node:` and `edge:` prefixes for the record split. This is an
    /// **on-demand** operator/automation call (an alert source, a pre-compaction
    /// check), not a hot path; do not call it per request.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::Storage`] if the backend scan or size probe fails.
    pub fn bloat_report(&self) -> Result<BloatReport> {
        let file_bytes = self.backend.size_bytes()?;
        let stored_bytes = self.backend.content_bytes()?;

        let mut logical_bytes: u64 = 0;
        let mut node_count: u64 = 0;
        for (key, value) in self.backend.scan_prefix(PREFIX_NODE)? {
            if key.len() != PREFIX_NODE.len() + 8 {
                continue; // skip anything that isn't a bare node record row
            }
            logical_bytes += (key.len() + value.len()) as u64;
            node_count += 1;
        }
        let mut edge_count: u64 = 0;
        for (key, value) in self.backend.scan_prefix(PREFIX_EDGE)? {
            if key.len() != PREFIX_EDGE.len() + 8 {
                continue;
            }
            logical_bytes += (key.len() + value.len()) as u64;
            edge_count += 1;
        }
        // Records are a subset of everything stored, so this never underflows;
        // saturating_sub is belt-and-braces against a backend miscount.
        let index_bytes = stored_bytes.saturating_sub(logical_bytes);

        let bloat_ratio = match file_bytes {
            Some(fb) if stored_bytes > 0 => Some(fb as f64 / stored_bytes as f64),
            _ => None,
        };

        Ok(BloatReport {
            file_bytes,
            stored_bytes,
            logical_bytes,
            index_bytes,
            node_count,
            edge_count,
            bloat_ratio,
        })
    }

    /// Per-keyspace storage breakdown (#275): rows + content bytes for each
    /// logical prefix, sorted by descending row count so the dominant keyspace
    /// is first. This is the evidence for the FTS-overhead investigation — it
    /// surfaces that the `fts` keyspace holds far more rows than any other on a
    /// text-heavy graph (one row per `(trigram, node)`), which is what drives
    /// the physical file well above its content size.
    ///
    /// On-demand only (it scans every keyspace); do not call it per request.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::Storage`] if a backend scan fails.
    pub fn keyspace_stats(&self) -> Result<Vec<KeyspaceStat>> {
        // Every disjoint keyspace prefix. These mirror the module-level
        // `PREFIX_*` consts (kept as literals here to avoid widening their
        // visibility across modules); they are stable on-disk wire prefixes.
        // Disjointness holds because each ends in `:` and no label is a `:`
        // -delimited prefix of another (e.g. `node:` vs `node_uuid:` differ at
        // the 5th byte `:` vs `_`).
        const PREFIXES: &[(&str, &[u8])] = &[
            ("node", b"node:"),
            ("node_uuid", b"node_uuid:"),
            ("node_title", b"node_title:"),
            ("node_kind", b"node_kind:"),
            ("edge", b"edge:"),
            ("edge_uuid", b"edge_uuid:"),
            ("edge_kind", b"edge_kind:"),
            ("out", b"out:"),
            ("in", b"in:"),
            ("updated", b"updated:"),
            ("prop", b"prop:"),
            ("fts", b"fts:"),
            ("ftslen", b"ftslen:"),
            ("efts", b"efts:"),
            ("eftslen", b"eftslen:"),
            ("vec", b"vec:"),
        ];
        let mut out = Vec::with_capacity(PREFIXES.len());
        for (label, prefix) in PREFIXES {
            let mut entries: u64 = 0;
            let mut content_bytes: u64 = 0;
            for (key, value) in self.backend.scan_prefix(prefix)? {
                entries += 1;
                content_bytes += (key.len() + value.len()) as u64;
            }
            out.push(KeyspaceStat {
                prefix: label,
                entries,
                content_bytes,
            });
        }
        out.sort_by(|a, b| b.entries.cmp(&a.entries).then(a.prefix.cmp(b.prefix)));
        Ok(out)
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
            semantic: Mutex::new(SemanticIndexRegistry::new()),
            #[cfg(feature = "http")]
            embedder: std::sync::OnceLock::new(),
            #[cfg(feature = "http")]
            embed_failures: Mutex::new(std::collections::HashMap::new()),
            #[cfg(feature = "http")]
            embedder_dimension: Mutex::new(None),
            rel_semantic: Mutex::new(SemanticIndexRegistry::new()),
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

    /// Compact **iff** the `policy` is enabled and this database is bloated
    /// past its thresholds (#253 slice 2).
    ///
    /// Returns `Ok(Some(report))` when a compaction ran, or `Ok(None)` when the
    /// policy is disabled, the backend has no on-disk footprint (in-memory), the
    /// file is under `policy.min_bytes`, or the bloat ratio is under
    /// `policy.min_ratio`. Any compaction error is propagated.
    ///
    /// This needs the same **exclusive** access as [`Self::compact`] (it calls
    /// it), so it is only safe to invoke while this handle is the sole owner of
    /// the backend — which is exactly the case inside [`Self::open`], before the
    /// handle is shared behind an `Arc`. That is where the opt-in automatic
    /// trigger lives; embedders can also call this directly at any quiescent
    /// point they control.
    pub fn maybe_auto_compact(
        &mut self,
        policy: &AutoCompactPolicy,
    ) -> Result<Option<CompactReport>> {
        if !policy.enabled {
            return Ok(None);
        }
        let report = self.bloat_report()?;
        // No physical footprint (in-memory) or nothing measurable → nothing to do.
        let Some(file_bytes) = report.file_bytes else {
            return Ok(None);
        };
        if file_bytes < policy.min_bytes {
            return Ok(None);
        }
        match report.bloat_ratio {
            Some(ratio) if ratio >= policy.min_ratio => Ok(Some(self.compact()?)),
            _ => Ok(None),
        }
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

    /// Every storage write needed to insert a **verbatim** node (preserving its
    /// id / uuid / timestamps and rebuilding every secondary index), returned as
    /// a `(key, value)` batch instead of applied one at a time.
    ///
    /// This is the import fast path: [`crate::dump`]'s `apply_dump` collects the
    /// entries for **all** imported nodes and commits them in a single
    /// [`StorageBackend::put_batch`] transaction — one fsync instead of one per
    /// trigram / index-entry. On a text-heavy graph that turns a restore / shrink
    /// from tens of minutes (hundreds of thousands of per-record commits) into
    /// seconds.
    ///
    /// # Errors
    ///
    /// [`DrevoError::DuplicateTitle`] if a *different* id already owns this
    /// node's title; storage errors from the property-index scan.
    pub(crate) fn node_raw_entries(&self, node: &Node) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        // Title uniqueness: only fail when a different id owns this title;
        // exact-content matches were filtered upstream in `dump::apply_dump`.
        let title_key = node_title_key(&node.title);
        if let Some(existing) = self.backend.get(&title_key)? {
            if u64_from_bytes(&existing) != node.id {
                return Err(DrevoError::DuplicateTitle(node.title.clone()));
            }
        }

        let mut writes: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (node_key(node.id), serialize_node(node)?),
            (node_uuid_key(&node.uuid), node.id.to_le_bytes().to_vec()),
            (title_key, node.id.to_le_bytes().to_vec()),
            (node_kind_key(&node.kind, node.id), Vec::new()),
        ];
        writes.extend(fts_index::node_index_entries_with_props(
            node.id,
            &node.title,
            &node.body,
            &node.properties,
        ));
        writes.extend(property_index::node_index_entries(
            node.id,
            &node.properties,
        )?);
        writes.push((updated_key(node.updated_at, node.id), Vec::new()));
        Ok(writes)
    }

    /// Storage writes to insert a **verbatim** [`Edge`] (id / uuid / timestamp
    /// preserved, adjacency + kind indexes rebuilt) — the edge counterpart of
    /// [`Self::node_raw_entries`], used by the import fast path.
    pub(crate) fn edge_raw_entries(&self, edge: &Edge) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let writes: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (edge_key(edge.id), serialize_edge(edge)?),
            (edge_uuid_key(&edge.uuid), edge.id.to_le_bytes().to_vec()),
            (
                out_edge_key(edge.from_id, &edge.kind, edge.id),
                adjacency_value(edge.to_id, &edge.kind),
            ),
            (
                in_edge_key(edge.to_id, &edge.kind, edge.id),
                adjacency_value(edge.from_id, &edge.kind),
            ),
            (edge_kind_key(&edge.kind, edge.id), Vec::new()),
        ];
        Ok(writes)
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
        fts_index::index_node_with_props(
            &*self.backend,
            node.id,
            &node.title,
            &node.body,
            &node.properties,
        )?;
        property_index::index_node(&*self.backend, node.id, &node.properties)?;
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
        self.backend.put(
            &out_edge_key(edge.from_id, &edge.kind, edge.id),
            &adjacency_value(edge.to_id, &edge.kind),
        )?;
        self.backend.put(
            &in_edge_key(edge.to_id, &edge.kind, edge.id),
            &adjacency_value(edge.from_id, &edge.kind),
        )?;
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
        fts_index::deindex_node_with_props(
            &*self.backend,
            current.id,
            &current.title,
            &current.body,
            &current.properties,
        )?;
        property_index::deindex_node(&*self.backend, current.id, &current.properties)?;
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
            .delete(&out_edge_key(current.from_id, &current.kind, current.id))?;
        self.backend
            .delete(&in_edge_key(current.to_id, &current.kind, current.id))?;
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
        fts_index::deindex_node_with_props(
            &*self.backend,
            id,
            &node.title,
            &node.body,
            &node.properties,
        )?;
        property_index::deindex_node(&*self.backend, id, &node.properties)?;
        self.backend.delete(&updated_key(node.updated_at, id))?;
        Ok(())
    }

    /// Remove an edge and all its indexes without journaling.
    fn purge_edge_no_journal(&self, id: u64) -> Result<()> {
        let edge = self.get_edge(id)?.ok_or(DrevoError::EdgeNotFound(id))?;
        self.backend.delete(&edge_key(id))?;
        self.backend.delete(&edge_uuid_key(&edge.uuid))?;
        self.backend
            .delete(&out_edge_key(edge.from_id, &edge.kind, id))?;
        self.backend
            .delete(&in_edge_key(edge.to_id, &edge.kind, id))?;
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
        let mut node = new_node.into_node(id);
        // #251 slice 4: server-side auto-embedding on ingest. No-op unless an
        // embedder is installed and an Auto-mode target matches this node.
        self.apply_auto_embeddings(&node.kind, &mut node.properties, None);

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
        fts_index::index_node_with_props(
            &*self.backend,
            id,
            &node.title,
            &node.body,
            &node.properties,
        )?;

        // Property index (Phase 14 task 00088)
        property_index::index_node(&*self.backend, id, &node.properties)?;

        // Updated-at index (newest-first ordering)
        self.backend.put(&updated_key(node.updated_at, id), &[])?;

        self.record_undo(UndoOp::CreatedNode(id));
        Ok(node)
    }

    /// Batch-create many nodes in a **single** storage transaction.
    ///
    /// Functionally equivalent to calling [`Self::create_node`] for each
    /// input, but folds every node's record + secondary-index writes
    /// (uuid / title / kind / FTS / property / updated-at) into one
    /// [`StorageBackend::put_batch`] — a single redb `begin_write`/`commit`.
    /// So N nodes cost **one** fsync instead of N, which is what makes a
    /// bulk import (e.g. `tools/neo4j-to-drevo`) fast instead of fsync-bound.
    ///
    /// Title uniqueness is enforced both against the store and within the
    /// batch; the first collision fails the whole call before anything is
    /// written (the id counter may still have advanced, exactly as the
    /// per-node path leaves gaps on a mid-sequence error).
    ///
    /// # Errors
    ///
    /// - [`DrevoError::DuplicateTitle`] if any title already exists or repeats
    ///   within the batch.
    pub fn create_nodes(&self, new_nodes: Vec<NewNode>) -> Result<Vec<Node>> {
        let mut writes: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut nodes: Vec<Node> = Vec::with_capacity(new_nodes.len());
        let mut batch_titles: std::collections::HashSet<String> = std::collections::HashSet::new();

        for new_node in new_nodes {
            let title_key = node_title_key(&new_node.title);
            if batch_titles.contains(&new_node.title) || self.backend.get(&title_key)?.is_some() {
                return Err(DrevoError::DuplicateTitle(new_node.title));
            }
            batch_titles.insert(new_node.title.clone());

            let id = self.alloc_node_id();
            let mut node = new_node.into_node(id);
            // #251 slice 4: auto-embed each node on bulk ingest too (no-op
            // unless an embedder is installed and an Auto-mode target matches).
            self.apply_auto_embeddings(&node.kind, &mut node.properties, None);

            writes.push((node_key(id), serialize_node(&node)?));
            writes.push((node_uuid_key(&node.uuid), id.to_le_bytes().to_vec()));
            writes.push((title_key, id.to_le_bytes().to_vec()));
            writes.push((node_kind_key(&node.kind, id), Vec::new()));
            writes.extend(fts_index::node_index_entries_with_props(
                id,
                &node.title,
                &node.body,
                &node.properties,
            ));
            writes.extend(property_index::node_index_entries(id, &node.properties)?);
            writes.push((updated_key(node.updated_at, id), Vec::new()));

            nodes.push(node);
        }

        self.backend.put_batch(&writes)?;
        for node in &nodes {
            self.record_undo(UndoOp::CreatedNode(node.id));
        }
        Ok(nodes)
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
        let old_properties = node.properties.clone();
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
        // #251 slice 4: re-embed when the source text changed (no-op unless an
        // embedder is installed and an Auto-mode target matches). Passing the
        // pre-image lets it skip when the text is unchanged.
        self.apply_auto_embeddings(&node.kind, &mut node.properties, Some(&old_properties));

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

        // Update FTS index if title, body, or any indexed property changed
        // (#227): the index now covers string properties, so a property-only
        // change must re-index too, and the old postings must be removed using
        // the OLD properties or stale property trigrams would leak.
        if node.title != old_title || node.body != old_body || node.properties.0 != old_properties.0
        {
            fts_index::deindex_node_with_props(
                &*self.backend,
                id,
                &old_title,
                &old_body,
                &old_properties,
            )?;
            fts_index::index_node_with_props(
                &*self.backend,
                id,
                &node.title,
                &node.body,
                &node.properties,
            )?;
        }

        // Update property index if the properties map changed (Phase 14
        // task 00088). Comparing the whole map covers added, removed, and
        // changed keys in one shot.
        if node.properties != old_properties {
            property_index::deindex_node(&*self.backend, id, &old_properties)?;
            property_index::index_node(&*self.backend, id, &node.properties)?;
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
        fts_index::deindex_node_with_props(
            &*self.backend,
            id,
            &node.title,
            &node.body,
            &node.properties,
        )?;

        // Remove property index
        property_index::deindex_node(&*self.backend, id, &node.properties)?;

        // Remove updated-at index
        self.backend.delete(&updated_key(node.updated_at, id))?;

        // Remove any persisted embedding sidecar (Phase 12 task `00078`).
        // Node ids are never reused, so a stray embedding would only waste
        // space rather than mis-associate, but cleaning it up keeps the
        // store free of orphans. This deletion is not yet undo-logged —
        // embedding persistence becomes transaction-aware with MVCC.
        vector_store::delete(&*self.backend, id)?;

        self.record_undo(UndoOp::DeletedNode(node));
        Ok(())
    }

    // ---------------------------------------------------------------
    // Vector embeddings (Phase 12 task `00078`)
    // ---------------------------------------------------------------

    /// Persist an embedding for a node, overwriting any prior value.
    ///
    /// Embeddings live in a dedicated durable store (`vec:{id}` keys),
    /// separate from the JSON `properties` the `00077` `similar(...)`
    /// predicate reads — this is the typed, compact home for vectors that
    /// survives a reopen and feeds [`build_vector_index`](Self::build_vector_index).
    ///
    /// The target node must exist; deleting the node later removes its
    /// embedding automatically.
    ///
    /// # Errors
    ///
    /// - [`DrevoError::NodeNotFound`] if `node_id` does not refer to an
    ///   existing node.
    /// - [`DrevoError::Encode`] / [`DrevoError::Storage`] on a backend or
    ///   serialization failure.
    pub fn set_embedding(&self, node_id: u64, embedding: Vector) -> Result<()> {
        if self.get_node(node_id)?.is_none() {
            return Err(DrevoError::NodeNotFound(node_id));
        }
        vector_store::put(&*self.backend, node_id, &embedding)
    }

    /// Persist embeddings for many nodes in a single batched write.
    ///
    /// On the redb backend the whole slice commits in one transaction
    /// (one `fsync`), making this the throughput path for bulk embedding
    /// ingest — a per-node [`set_embedding`](Self::set_embedding) loop
    /// would open one write transaction per row. Every target node is
    /// validated to exist *before* the write, so the batch is all-or-nothing.
    ///
    /// # Errors
    ///
    /// - [`DrevoError::NodeNotFound`] if any id does not refer to an
    ///   existing node — no embedding is written in that case.
    /// - [`DrevoError::Encode`] / [`DrevoError::Storage`] on a backend or
    ///   serialization failure.
    pub fn set_embeddings_batch(&self, embeddings: &[(u64, Vector)]) -> Result<()> {
        for (node_id, _) in embeddings {
            if self.get_node(*node_id)?.is_none() {
                return Err(DrevoError::NodeNotFound(*node_id));
            }
        }
        vector_store::put_batch(&*self.backend, embeddings)
    }

    /// Fetch the embedding stored for a node, or `None` if it has none.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::Decode`] if the stored bytes are corrupt, or
    /// [`DrevoError::Storage`] on backend failure.
    pub fn get_embedding(&self, node_id: u64) -> Result<Option<Vector>> {
        vector_store::get(&*self.backend, node_id)
    }

    /// Remove the embedding stored for a node. A node with no embedding is
    /// a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::Storage`] on backend failure.
    pub fn delete_embedding(&self, node_id: u64) -> Result<()> {
        vector_store::delete(&*self.backend, node_id)
    }

    /// Count the embeddings currently persisted.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::Storage`] on backend failure.
    pub fn embedding_count(&self) -> Result<usize> {
        vector_store::count(&*self.backend)
    }

    /// Rebuild an in-memory HNSW index (`00076`) from every persisted
    /// embedding.
    ///
    /// The HNSW proximity graph is in-memory only; after a reopen this
    /// replays the durable vectors into a fresh [`HnswIndex`] keyed by
    /// node id — the approximate-nearest-neighbor acceleration path that
    /// the brute-force `00077` `similar(...)` predicate provides the
    /// correctness baseline for.
    ///
    /// # Errors
    ///
    /// - [`DrevoError::Vector`] if a persisted embedding cannot be
    ///   inserted (e.g. its dimension disagrees with the first vector).
    /// - [`DrevoError::Storage`] on backend failure.
    pub fn build_vector_index(&self, config: HnswConfig) -> Result<HnswIndex> {
        vector_store::build_hnsw(&*self.backend, config)
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
        let mut edge = new_edge.into_edge(id);
        // #266: relationship-side auto-embedding on ingest. No-op unless an
        // embedder is installed and an Auto-mode rel target matches this kind.
        self.apply_auto_embeddings_edge(&edge.kind, &mut edge.properties, None);

        // Store edge data
        let data = serialize_edge(&edge)?;
        self.backend.put(&edge_key(id), &data)?;

        // UUID index
        self.backend
            .put(&edge_uuid_key(&edge.uuid), &id.to_le_bytes())?;

        // Outgoing adjacency: out:{from_id}:{kind}:{edge_id} -> (to_id, kind)
        // (#243 slice 2 folds the kind into the key).
        self.backend.put(
            &out_edge_key(edge.from_id, &edge.kind, id),
            &adjacency_value(edge.to_id, &edge.kind),
        )?;

        // Incoming adjacency: in:{to_id}:{kind}:{edge_id} -> (from_id, kind).
        self.backend.put(
            &in_edge_key(edge.to_id, &edge.kind, id),
            &adjacency_value(edge.from_id, &edge.kind),
        )?;

        // Edge kind index
        self.backend.put(&edge_kind_key(&edge.kind, id), &[])?;

        // FTS index the edge's string properties (#227-B) so relationship text
        // (e.g. `name` / `fact`) is BM25-searchable via fts.searchRelationships.
        edge_index::index_edge(&*self.backend, id, &edge.properties)?;

        self.record_undo(UndoOp::CreatedEdge(id));
        Ok(edge)
    }

    /// Batch-create many edges in a **single** storage transaction.
    ///
    /// The edge sibling of [`Self::create_nodes`]: folds every edge's record
    /// plus its uuid / adjacency (`out:`/`in:`) / kind index writes into one
    /// [`StorageBackend::put_batch`]. Each edge's endpoints must already
    /// exist (create the nodes first, e.g. via [`Self::create_nodes`]); the
    /// first invalid edge fails the whole call before any write.
    ///
    /// # Errors
    ///
    /// - [`DrevoError::InvalidWeight`] if any weight is non-finite.
    /// - [`DrevoError::NodeNotFound`] if any endpoint does not exist.
    pub fn create_edges(&self, new_edges: Vec<NewEdge>) -> Result<Vec<Edge>> {
        let mut writes: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut edges: Vec<Edge> = Vec::with_capacity(new_edges.len());

        for new_edge in new_edges {
            if !new_edge.weight.is_finite() {
                return Err(DrevoError::InvalidWeight(new_edge.weight));
            }
            if self.get_node(new_edge.from_id)?.is_none() {
                return Err(DrevoError::NodeNotFound(new_edge.from_id));
            }
            if self.get_node(new_edge.to_id)?.is_none() {
                return Err(DrevoError::NodeNotFound(new_edge.to_id));
            }

            let id = self.alloc_edge_id();
            let mut edge = new_edge.into_edge(id);
            // #266: auto-embed each edge on bulk ingest too (no-op unless an
            // embedder is installed and an Auto-mode rel target matches).
            self.apply_auto_embeddings_edge(&edge.kind, &mut edge.properties, None);

            writes.push((edge_key(id), serialize_edge(&edge)?));
            writes.push((edge_uuid_key(&edge.uuid), id.to_le_bytes().to_vec()));
            writes.push((
                out_edge_key(edge.from_id, &edge.kind, id),
                adjacency_value(edge.to_id, &edge.kind),
            ));
            writes.push((
                in_edge_key(edge.to_id, &edge.kind, id),
                adjacency_value(edge.from_id, &edge.kind),
            ));
            writes.push((edge_kind_key(&edge.kind, id), Vec::new()));
            // FTS postings for the edge's string properties (#227-B).
            writes.extend(edge_index::edge_index_entries(id, &edge.properties));

            edges.push(edge);
        }

        self.backend.put_batch(&writes)?;
        for edge in &edges {
            self.record_undo(UndoOp::CreatedEdge(edge.id));
        }
        Ok(edges)
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
        let old_properties = edge.properties.clone();

        edge.apply_patch(patch);
        // #266: re-embed when the source text changed (no-op unless an embedder
        // is installed and an Auto-mode rel target matches). Pass the pre-image
        // so an unchanged text is skipped.
        self.apply_auto_embeddings_edge(&edge.kind, &mut edge.properties, Some(&old_properties));

        let data = serialize_edge(&edge)?;
        self.backend.put(&edge_key(id), &data)?;

        // Update edge_kind index if kind changed
        if edge.kind != old_kind {
            self.backend.delete(&edge_kind_key(&old_kind, id))?;
            self.backend.put(&edge_kind_key(&edge.kind, id), &[])?;
            // The kind is part of the v2 adjacency KEY (#243 slice 2), so a
            // kind change MOVES both adjacency entries: delete the old-kind
            // keys, then write the new-kind keys. Endpoints are unchanged.
            self.backend
                .delete(&out_edge_key(edge.from_id, &old_kind, id))?;
            self.backend
                .delete(&in_edge_key(edge.to_id, &old_kind, id))?;
            self.backend.put(
                &out_edge_key(edge.from_id, &edge.kind, id),
                &adjacency_value(edge.to_id, &edge.kind),
            )?;
            self.backend.put(
                &in_edge_key(edge.to_id, &edge.kind, id),
                &adjacency_value(edge.from_id, &edge.kind),
            )?;
        }

        // Re-index the edge's FTS text when a string property changed (#227-B):
        // de-index by the OLD properties so stale trigrams can't leak.
        if edge.properties.0 != old_properties.0 {
            edge_index::deindex_edge(&*self.backend, id, &old_properties)?;
            edge_index::index_edge(&*self.backend, id, &edge.properties)?;
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
        self.backend
            .delete(&out_edge_key(edge.from_id, &edge.kind, id))?;

        // Remove incoming adjacency entry
        self.backend
            .delete(&in_edge_key(edge.to_id, &edge.kind, id))?;

        // Remove edge kind index
        self.backend.delete(&edge_kind_key(&edge.kind, id))?;

        // Remove FTS postings (#227-B).
        edge_index::deindex_edge(&*self.backend, id, &edge.properties)?;

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

    /// Return every node whose `key` property equals `value`, resolved
    /// through the persistent property index (Phase 14 task `00088`).
    ///
    /// This is the `O(matches)` fast path for equality predicates such as
    /// the Cypher pattern `MATCH (n {status: "open"})`: it scans only the
    /// index entries for the requested `(key, value)` pair instead of
    /// every node in the graph. Matching is on canonical-JSON byte
    /// equality (see [`crate::property_index`]). Results are ordered by
    /// node id.
    ///
    /// # Arguments
    ///
    /// * `key` — the property name to match (e.g. `"status"`)
    /// * `value` — the exact JSON value the property must equal
    pub fn nodes_by_property(&self, key: &str, value: &serde_json::Value) -> Result<Vec<Node>> {
        let ids = property_index::node_ids_for_value(&*self.backend, key, value)?;
        let mut nodes = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(node) = self.get_node(id)? {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    /// Count the nodes whose `key` property equals `value` without
    /// materializing them — a selectivity hint for the cost-based planner
    /// (Phase 14), backed by the same persistent property index as
    /// [`Self::nodes_by_property`].
    pub fn count_nodes_by_property(&self, key: &str, value: &serde_json::Value) -> Result<usize> {
        Ok(property_index::node_ids_for_value(&*self.backend, key, value)?.len())
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

    /// Full-text search ranked by Okapi BM25.
    ///
    /// Equivalent to [`Drevo::search_fts_ranked`] with
    /// [`FtsRanking::default`] (BM25, `k1 = 1.2`, `b = 0.75`). Extracts
    /// trigrams from the query, finds candidate nodes via posting-list
    /// intersection (AND semantics), scores each candidate, and returns up
    /// to `limit` results sorted by descending score, then by ascending
    /// node id for stability.
    ///
    /// Returns an empty list if the query produces no trigrams or no nodes
    /// match.
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<ScoredNode>> {
        self.search_fts_ranked(query, limit, FtsRanking::default())
    }

    /// Full-text search with a selectable [`FtsRanking`] strategy.
    ///
    /// The public [`Drevo::search_fts`] entry point delegates here with
    /// BM25; pass [`FtsRanking::TfIdf`] to fall back to the legacy
    /// deterministic, length-insensitive scorer (useful for golden-ranking
    /// baselines).
    ///
    /// **BM25 (default).** Per query trigram `qᵢ`:
    ///
    /// ```text
    /// score += idf(qᵢ) · tf·(k1 + 1) / (tf + k1·(1 − b + b·|d|/avgdl))
    /// ```
    ///
    /// where `tf` is the raw occurrence count of `qᵢ` in the document
    /// (counting repetition — see [`crate::fts::raw_trigrams`]), `|d|` is
    /// the document length (total trigram tokens), `avgdl` is the average
    /// document length across the corpus, and `idf` is the smoothed
    /// BM25 IDF (see [`crate::fts::index`]). The `k1` term saturates
    /// runaway term frequencies; the `b` term down-weights long documents.
    /// Corpus statistics (`N`, `avgdl`) are derived from the persisted
    /// per-document lengths maintained on every insert/update/delete.
    ///
    /// **TF-IDF (legacy).** Binary trigram presence normalized by the
    /// node's trigram cardinality, weighted by smoothed IDF
    /// `ln(1 + N/df)`. Score is the sum of `tf · idf` over query trigrams.
    ///
    /// # Performance
    ///
    /// `audit/AUDIT-fts.md` documents a measured ~800 ms vs 50 ms-target
    /// gap on broad single-token queries against ~10k nodes. The
    /// bottleneck is the per-candidate `extract_trigrams` call combined
    /// with the corpus-wide scan to count `N`/`avgdl`. Further mitigations
    /// (cached posting-list lengths, inverted-index compaction) remain a
    /// tracked follow-up.
    pub fn search_fts_ranked(
        &self,
        query: &str,
        limit: usize,
        ranking: FtsRanking,
    ) -> Result<Vec<ScoredNode>> {
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

        let mut scored = match ranking {
            FtsRanking::Bm25 { k1, b } => {
                self.score_bm25(&query_trigrams, &candidate_ids, k1, b)?
            }
            FtsRanking::TfIdf => self.score_tfidf(&query_trigrams, &candidate_ids)?,
        };

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

    /// Full-text search over **relationships** (#227-B): the top-`limit` edges
    /// whose string properties best match `query`, ranked by Okapi BM25 — the
    /// edge companion of [`Self::search_fts`], exposed to Cypher as
    /// `CALL fts.searchRelationships(query, k)`.
    ///
    /// # Errors
    ///
    /// Returns [`DrevoError::Storage`] on backend failure.
    pub fn search_fts_relationships(&self, query: &str, limit: usize) -> Result<Vec<ScoredEdge>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let query_trigrams = extract_trigrams(query, "");
        if query_trigrams.is_empty() {
            return Ok(Vec::new());
        }
        let candidate_ids = edge_index::intersect_trigrams(&*self.backend, &query_trigrams)?;
        if candidate_ids.is_empty() {
            return Ok(Vec::new());
        }

        let stats = edge_index::corpus_stats(&*self.backend)?;
        let avgdl = stats.avgdl();
        let (k1, b) = (1.2_f32, 0.75_f32);

        let mut dfs: Vec<u64> = Vec::with_capacity(query_trigrams.len());
        for trigram in &query_trigrams {
            dfs.push(edge_index::posting_list_len(&*self.backend, trigram)? as u64);
        }
        let n = dfs.iter().copied().max().unwrap_or(0).max(stats.doc_count);
        let idf_values: Vec<f32> = dfs.iter().map(|&df| fts_index::bm25_idf(n, df)).collect();

        let mut scored: Vec<ScoredEdge> = Vec::with_capacity(candidate_ids.len());
        for edge_id in &candidate_ids {
            let edge = match self.get_edge(*edge_id)? {
                Some(e) => e,
                None => continue,
            };
            let raw = edge_index::edge_raw_trigrams(&edge.properties);
            if raw.is_empty() {
                continue;
            }
            let doc_len = edge_index::doc_length(&*self.backend, *edge_id)?
                .map(|l| l as f32)
                .unwrap_or(raw.len() as f32);
            let norm = if avgdl > 0.0 {
                1.0 - b + b * (doc_len / avgdl)
            } else {
                1.0
            };
            let mut score: f32 = 0.0;
            for (i, qt) in query_trigrams.iter().enumerate() {
                let tf = raw.iter().filter(|t| *t == qt).count() as f32;
                if tf == 0.0 {
                    continue;
                }
                score += idf_values[i] * (tf * (k1 + 1.0)) / (tf + k1 * norm);
            }
            if score > 0.0 {
                scored.push(ScoredEdge { edge, score });
            }
        }
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.edge.id.cmp(&b.edge.id))
        });
        scored.truncate(limit);
        Ok(scored)
    }

    /// Score candidates with Okapi BM25 (term-frequency saturation +
    /// document-length normalization). See [`Drevo::search_fts_ranked`].
    fn score_bm25(
        &self,
        query_trigrams: &[String],
        candidate_ids: &[u64],
        k1: f32,
        b: f32,
    ) -> Result<Vec<ScoredNode>> {
        let stats = fts_index::corpus_stats(&*self.backend)?;
        let avgdl = stats.avgdl();

        // Document frequency per query trigram.
        let mut dfs: Vec<u64> = Vec::with_capacity(query_trigrams.len());
        for trigram in query_trigrams {
            dfs.push(fts_index::posting_list_len(&*self.backend, trigram)? as u64);
        }

        // `N` for the IDF. By construction every posting-bearing node also
        // has a persisted length, so `doc_count >= df` for a consistent
        // index. The `max` only matters for a legacy index written before
        // length stats existed (postings but no `ftslen:`), where it keeps
        // IDF non-negative instead of silently dropping every result.
        let n = dfs.iter().copied().max().unwrap_or(0).max(stats.doc_count);

        // Precompute the BM25 IDF for each query trigram.
        let idf_values: Vec<f32> = dfs.iter().map(|&df| fts_index::bm25_idf(n, df)).collect();

        let mut scored: Vec<ScoredNode> = Vec::with_capacity(candidate_ids.len());
        for node_id in candidate_ids {
            let node = match self.get_node(*node_id)? {
                Some(n) => n,
                None => continue,
            };

            // Raw (non-deduplicated) trigrams give per-term frequency.
            let raw = fts_index::node_raw_trigrams(&node.title, &node.body, &node.properties);
            if raw.is_empty() {
                continue;
            }
            // Document length `|d|`: prefer the persisted stat (the same
            // value `avgdl` is averaged from) and fall back to the recomputed
            // count for a legacy index that predates length persistence.
            let doc_len = fts_index::doc_length(&*self.backend, *node_id)?
                .map(|l| l as f32)
                .unwrap_or(raw.len() as f32);
            // Length-normalization factor; falls back to no normalization
            // when the corpus average is unavailable.
            let norm = if avgdl > 0.0 {
                1.0 - b + b * (doc_len / avgdl)
            } else {
                1.0
            };

            let mut score: f32 = 0.0;
            for (i, qt) in query_trigrams.iter().enumerate() {
                let tf = raw.iter().filter(|t| *t == qt).count() as f32;
                if tf == 0.0 {
                    continue;
                }
                score += idf_values[i] * (tf * (k1 + 1.0)) / (tf + k1 * norm);
            }

            if score > 0.0 {
                scored.push(ScoredNode { node, score });
            }
        }
        Ok(scored)
    }

    /// Score candidates with the legacy TF-IDF scorer. Retained for
    /// deterministic baselines. See [`Drevo::search_fts_ranked`].
    fn score_tfidf(
        &self,
        query_trigrams: &[String],
        candidate_ids: &[u64],
    ) -> Result<Vec<ScoredNode>> {
        // Total number of indexed nodes (approximate: count node: prefix entries)
        let all_nodes = self.backend.scan_prefix(PREFIX_NODE)?;
        let total_nodes = all_nodes.len() as f32;

        // Precompute IDF for each query trigram
        let mut idf_values: Vec<f32> = Vec::with_capacity(query_trigrams.len());
        for trigram in query_trigrams {
            let df = fts_index::posting_list_len(&*self.backend, trigram)? as f32;
            // Smoothed IDF: ln(1 + N / df) — avoids zero when df == N.
            let idf = if df > 0.0 {
                (total_nodes / df).ln_1p()
            } else {
                0.0
            };
            idf_values.push(idf);
        }

        let mut scored: Vec<ScoredNode> = Vec::with_capacity(candidate_ids.len());
        for node_id in candidate_ids {
            let node = match self.get_node(*node_id)? {
                Some(n) => n,
                None => continue,
            };

            // Extract the node's own (deduplicated) trigrams to compute TF.
            let node_trigrams = fts_index::node_trigrams(&node.title, &node.body, &node.properties);
            let node_trigram_count = node_trigrams.len() as f32;
            if node_trigram_count == 0.0 {
                continue;
            }

            let mut score: f32 = 0.0;
            for (i, qt) in query_trigrams.iter().enumerate() {
                // Trigrams are deduplicated, so tf is 0 or 1.
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
        Ok(scored)
    }

    // ---------------------------------------------------------------
    // Keyword faceting (task 00133)
    // ---------------------------------------------------------------

    /// Group every node of `kind` by the keywords extracted from one of its
    /// text fields, optionally collapsing near-duplicate keywords.
    ///
    /// For each node the top-`k` salient keywords of `property` are
    /// extracted (the `keywords()` extractor), then collapsed into facets per
    /// the chosen `collapse` axis (see [`FacetCollapse`]). This is the
    /// single-call form of the cross-cutting "group my graph by extracted
    /// keyword" query, returning `[{facet, members, count}]` sorted by
    /// descending document count (then label).
    ///
    /// `property` selects the source text: the reserved names `"title"` and
    /// `"body"` read the node's title/body; any other name reads that
    /// `properties` key (a string value verbatim, any other JSON value via
    /// its `to_string`). A node missing/empty in that field contributes
    /// nothing rather than erroring, so a heterogeneous `kind` does not
    /// abort (mirrors the `keywords()` NULL discipline, task `00132`).
    ///
    /// # Arguments
    ///
    /// * `kind` — node classification to scan.
    /// * `property` — source text field (`"title"`, `"body"`, or a property
    ///   key).
    /// * `k` — keywords extracted per node before collapsing.
    /// * `collapse` — the similarity axis (none / lexical / semantic).
    pub fn facets(
        &self,
        kind: &str,
        property: &str,
        k: usize,
        collapse: &FacetCollapse<'_>,
    ) -> Result<Vec<Facet>> {
        let nodes = self.list_nodes_by_kind(kind, usize::MAX, 0)?;
        let mut per_doc: Vec<(u64, Vec<String>)> = Vec::with_capacity(nodes.len());
        for node in nodes {
            let Some(text) = node_property_text(&node, property) else {
                continue;
            };
            let keywords = crate::fts::keywords::extract_keywords(&*self.backend, &text, k, false)?;
            if !keywords.is_empty() {
                per_doc.push((node.id, keywords));
            }
        }
        Ok(build_facets(&per_doc, collapse))
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

    /// Compute PageRank centrality over the whole graph — Phase 15 task
    /// `00098`.
    ///
    /// Materialises the entire node + edge set into an in-memory
    /// [`crate::algorithms::AdjacencyList`] snapshot and runs weighted power
    /// iteration ([`crate::algorithms::pagerank`]). Edge weights are read from
    /// [`crate::model::Edge::weight`] and interpreted as non-negative link
    /// strengths (negatives are clamped to `0.0`). Directed: rank flows along
    /// outgoing edges; dangling nodes redistribute their rank uniformly.
    ///
    /// The `config` is validated at construction
    /// ([`crate::algorithms::PageRankConfig::new`]); this call itself only
    /// fails if a storage scan fails. An empty graph yields an empty result.
    pub fn pagerank(
        &self,
        config: &crate::algorithms::PageRankConfig,
    ) -> Result<crate::algorithms::PageRankResult> {
        let graph = self.adjacency_snapshot()?;
        Ok(crate::algorithms::pagerank(&graph, config))
    }

    /// Detect communities over the whole graph using the Louvain method —
    /// Phase 15 task `00098`.
    ///
    /// Materialises the entire node + edge set into an in-memory
    /// [`crate::algorithms::AdjacencyList`] snapshot and runs multi-level
    /// modularity optimisation ([`crate::algorithms::louvain`]). The directed
    /// graph is projected to undirected first (reciprocal edges sum; self-loops
    /// are kept); edge weights are interpreted as non-negative.
    ///
    /// The `config` is validated at construction
    /// ([`crate::algorithms::LouvainConfig::new`]); this call itself only fails
    /// if a storage scan fails. An empty graph yields an empty result.
    pub fn louvain_communities(
        &self,
        config: &crate::algorithms::LouvainConfig,
    ) -> Result<crate::algorithms::LouvainResult> {
        let graph = self.adjacency_snapshot()?;
        Ok(crate::algorithms::louvain(&graph, config))
    }

    /// Build an in-memory [`crate::algorithms::AdjacencyList`] snapshot of the
    /// entire graph for the global algorithms ([`Self::pagerank`] /
    /// [`Self::louvain_communities`]). Nodes are ordered by ascending ID so the
    /// algorithms' results are deterministic.
    fn adjacency_snapshot(&self) -> Result<crate::algorithms::AdjacencyList> {
        let node_ids: Vec<u64> = self
            .collect_all_nodes()?
            .into_iter()
            .map(|n| n.id)
            .collect();
        let edges = self
            .collect_all_edges()?
            .into_iter()
            .map(|e| (e.from_id, e.to_id, e.weight));
        Ok(crate::algorithms::AdjacencyList::from_parts(
            node_ids, edges,
        ))
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
    /// Semantically equivalent to [`Self::bfs`] with `max_depth=1`, but reads
    /// the neighbor ids from the denormalized adjacency index via
    /// [`Self::neighbor_ids`] (one prefix scan, no `get_edge` per neighbor on
    /// a #243-era database) and then loads each distinct neighbor node once.
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
        let ids = self.neighbor_ids(node_id, direction, kind)?;
        let mut nodes = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(node) = self.get_node(id)? {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    /// Return the **distinct** node ids adjacent to `node_id` in `direction`,
    /// optionally restricted to edges of `kind`.
    ///
    /// Unlike [`Self::neighbors`], this returns bare node ids and — on any
    /// database written since #243 — reads them **straight from the adjacency
    /// index**: one `out:`/`in:` prefix scan, zero `get_edge` point lookups,
    /// so the cost stays proportional to the fan-out even on supernodes.
    /// (Legacy adjacency entries written before #243 carry an empty value and
    /// fall back to one `get_edge` each; upgrade them once with
    /// [`Self::backfill_adjacency_values`].)
    ///
    /// Ordering follows a breadth-first visit: outgoing entries before
    /// incoming (for [`Direction::Both`]), each neighbor reported once at
    /// first sight, and `node_id` itself excluded — so a self-loop contributes
    /// no neighbor.
    pub fn neighbor_ids(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<u64>> {
        // Push the kind filter into the scan so a kind-restricted fan-out over
        // a supernode reads only the matching sub-prefix (#243 slice 2).
        let targets = self.adjacency_targets(node_id, direction, kind)?;
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        seen.insert(node_id);
        let mut ids = Vec::new();
        for target in targets {
            if seen.insert(target.neighbor_id) {
                ids.push(target.neighbor_id);
            }
        }
        Ok(ids)
    }

    /// Upgrade legacy empty adjacency values (pre-#243) to the denormalized
    /// `(neighbor_id, kind)` payload, so later [`Self::neighbor_ids`] and
    /// kind-filtered traversals skip the `get_edge` fallback.
    ///
    /// Returns the number of adjacency entries rewritten. Idempotent: entries
    /// that already carry a value are left untouched, so it is safe to run
    /// repeatedly — e.g. once after opening a database created by an older
    /// drevo. An adjacency entry whose edge record is missing (a dangling
    /// entry — an integrity violation, not a legacy state) is skipped.
    pub fn backfill_adjacency_values(&self) -> Result<u64> {
        let mut upgraded = 0u64;
        for (prefix, incoming) in [(PREFIX_OUT, false), (PREFIX_IN, true)] {
            for (key, value) in self.backend.scan_prefix(prefix)? {
                if !value.is_empty() {
                    continue; // already denormalized
                }
                // {prefix}{node_id_8}:{edge_id_8} — the edge id is the last 8.
                if key.len() < 8 {
                    continue;
                }
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&key[key.len() - 8..]);
                let edge_id = u64::from_le_bytes(arr);
                if let Some(edge) = self.get_edge(edge_id)? {
                    let neighbor_id = if incoming { edge.from_id } else { edge.to_id };
                    self.backend
                        .put(&key, &adjacency_value(neighbor_id, &edge.kind))?;
                    upgraded += 1;
                }
            }
        }
        Ok(upgraded)
    }

    /// Read one bounded page of a node's **outgoing** adjacency (#243 slice 3).
    ///
    /// Returns at most `limit` [`AdjacencyEntry`]s (edge id + neighbor id +
    /// kind) in ascending key order, plus an opaque `next` cursor. Pass
    /// `after = None` for the first page and `after = page.next.as_deref()` for
    /// each subsequent page; iteration ends when `next` is `None`.
    ///
    /// Unlike [`Self::edges_of`], this walks the adjacency index in
    /// **bounded-memory chunks** — the backend stops scanning once `limit`
    /// entries are collected — so a supernode with millions of out-edges can be
    /// consumed a page at a time instead of materialising the whole set. On a
    /// denormalized database (post-#243) it also loads **no** edge records;
    /// legacy empty-value entries fall back to one `get_edge` each (bounded by
    /// `limit`).
    pub fn outgoing_adjacency_page(
        &self,
        node_id: u64,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<AdjacencyPage> {
        self.adjacency_page(node_id, false, after, limit)
    }

    /// Read one bounded page of a node's **incoming** adjacency (#243 slice 3).
    ///
    /// The `in:`-prefix sibling of [`Self::outgoing_adjacency_page`]; the
    /// `neighbor_id` of each entry is the edge's `from_id`. Same cursor
    /// protocol and bounded-memory guarantees.
    pub fn incoming_adjacency_page(
        &self,
        node_id: u64,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<AdjacencyPage> {
        self.adjacency_page(node_id, true, after, limit)
    }

    /// Shared bounded-page reader for [`Self::outgoing_adjacency_page`] /
    /// [`Self::incoming_adjacency_page`]. `incoming` selects the `in:` prefix
    /// (neighbor = `from_id`) versus `out:` (neighbor = `to_id`).
    fn adjacency_page(
        &self,
        node_id: u64,
        incoming: bool,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<AdjacencyPage> {
        let prefix = if incoming {
            in_prefix(node_id)
        } else {
            out_prefix(node_id)
        };
        // Fetch one extra entry as a lookahead: if it exists there is another
        // page, and `next` is the cursor at the `limit`-th key; otherwise this
        // is the final page and `next` is `None`. This avoids the trailing
        // empty page that a plain "full page -> cursor" rule needs when the
        // edge count divides evenly by `limit`.
        let raw = self
            .backend
            .scan_prefix_limited(&prefix, after, limit.saturating_add(1))?;
        let has_more = raw.len() > limit;
        let kept = if has_more { &raw[..limit] } else { &raw[..] };
        let next = has_more
            .then(|| kept.last().map(|(k, _)| k.clone()))
            .flatten();
        let base_prefix = if incoming { PREFIX_IN } else { PREFIX_OUT };
        let mut entries = Vec::with_capacity(kept.len());
        for (key, value) in kept {
            let edge_id = edge_id_from_adjacency_key(key, base_prefix);
            match decode_adjacency_value(value) {
                Some((neighbor_id, kind)) => entries.push(AdjacencyEntry {
                    edge_id,
                    neighbor_id,
                    kind: kind.to_string(),
                }),
                None => {
                    // Legacy empty value (pre-#243) — recover from the edge.
                    if let Some(edge) = self.get_edge(edge_id)? {
                        let neighbor_id = if incoming { edge.from_id } else { edge.to_id };
                        entries.push(AdjacencyEntry {
                            edge_id,
                            neighbor_id,
                            kind: edge.kind,
                        });
                    }
                }
            }
        }
        Ok(AdjacencyPage { entries, next })
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
        for (key, value) in &out_entries {
            // out:{from_id_8}:{kind}:{edge_id_8} (v2) — the shortest valid key
            // is the empty-kind case; node id is the first 8, edge id the last.
            let min_len = PREFIX_OUT.len() + 8 + 1 + 1 + 8;
            if key.len() < min_len {
                violations.push(format!(
                    "adjacency key has unexpected length: out: key len = {}",
                    key.len()
                ));
                continue;
            }
            let from_id = u64_from_adjacency_key_first_id(key, PREFIX_OUT);
            let edge_id = edge_id_from_adjacency_key(key, PREFIX_OUT);
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
                    // #243 — a denormalized value must match the edge's
                    // to_id / kind (an empty value is a legacy entry, allowed).
                    if let Some((neighbor_id, kind)) = decode_adjacency_value(value) {
                        if neighbor_id != e.to_id || kind != e.kind {
                            violations.push(format!(
                                "out adjacency value mismatch: edge_id={edge_id}, \
                                 value=({neighbor_id}, {kind:?}), \
                                 edge=(to_id={}, kind={:?})",
                                e.to_id, e.kind
                            ));
                        }
                    }
                    // Invariant #1 — the corresponding in: entry MUST exist.
                    let in_key = in_edge_key(e.to_id, &e.kind, e.id);
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
        for (key, value) in &in_entries {
            let min_len = PREFIX_IN.len() + 8 + 1 + 1 + 8;
            if key.len() < min_len {
                violations.push(format!(
                    "adjacency key has unexpected length: in: key len = {}",
                    key.len()
                ));
                continue;
            }
            let to_id = u64_from_adjacency_key_first_id(key, PREFIX_IN);
            let edge_id = edge_id_from_adjacency_key(key, PREFIX_IN);
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
                    // #243 — a denormalized value must match the edge's
                    // from_id / kind (an empty value is a legacy entry).
                    if let Some((neighbor_id, kind)) = decode_adjacency_value(value) {
                        if neighbor_id != e.from_id || kind != e.kind {
                            violations.push(format!(
                                "in adjacency value mismatch: edge_id={edge_id}, \
                                 value=({neighbor_id}, {kind:?}), \
                                 edge=(from_id={}, kind={:?})",
                                e.from_id, e.kind
                            ));
                        }
                    }
                    // Invariant #1 — mirror direction.
                    let out_key = out_edge_key(e.from_id, &e.kind, e.id);
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
                .get(&out_edge_key(edge.from_id, &edge.kind, edge.id))?
                .is_none()
            {
                violations.push(format!(
                    "edge {} missing its out: adjacency entry (from_id={})",
                    edge.id, edge.from_id
                ));
            }
            if self
                .backend
                .get(&in_edge_key(edge.to_id, &edge.kind, edge.id))?
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

    /// Read a node's denormalized adjacency targets in `direction` — the
    /// `(edge_id, neighbor_id, kind)` triples recovered from the adjacency
    /// **value** without a full `get_edge` on #243-era entries. Legacy
    /// empty-value entries fall back to one `get_edge` each. For
    /// [`Direction::Both`] the outgoing targets precede the incoming ones,
    /// matching the edge order of [`Self::edges_of`].
    fn adjacency_targets(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<AdjTarget>> {
        match direction {
            Direction::Outgoing => self.adjacency_targets_prefixed(node_id, false, kind),
            Direction::Incoming => self.adjacency_targets_prefixed(node_id, true, kind),
            Direction::Both => {
                let mut targets = self.adjacency_targets_prefixed(node_id, false, kind)?;
                targets.extend(self.adjacency_targets_prefixed(node_id, true, kind)?);
                Ok(targets)
            }
        }
    }

    /// One-direction half of [`Self::adjacency_targets`]. `incoming` selects
    /// the `in:` prefix (neighbor = `from_id`) versus the `out:` prefix
    /// (neighbor = `to_id`), which also drives the legacy `get_edge` fallback.
    ///
    /// When `kind` is `Some`, the scan is narrowed to the kind-scoped
    /// sub-prefix `{out|in}:{node}:{kind}:` (#243 slice 2), so a kind-filtered
    /// fan-out over a supernode costs `O(matches)` — the raw scan touches only
    /// the edges of that kind instead of the node's full degree. The decoded
    /// kind is still re-checked against `kind`, which discards the rare false
    /// positive when one kind is a byte-prefix of another (a `kind` containing
    /// `:`).
    fn adjacency_targets_prefixed(
        &self,
        node_id: u64,
        incoming: bool,
        kind: Option<&str>,
    ) -> Result<Vec<AdjTarget>> {
        let prefix = match kind {
            Some(k) if incoming => in_kind_prefix(node_id, k),
            Some(k) => out_kind_prefix(node_id, k),
            None if incoming => in_prefix(node_id),
            None => out_prefix(node_id),
        };
        let entries = self.backend.scan_prefix(&prefix)?;
        let mut targets = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            let edge_id =
                edge_id_from_adjacency_key(&key, if incoming { PREFIX_IN } else { PREFIX_OUT });
            let target = match decode_adjacency_value(&value) {
                Some((neighbor_id, k)) => AdjTarget {
                    neighbor_id,
                    kind: k.to_string(),
                },
                None => {
                    // Legacy empty value (pre-#243) — recover from the edge.
                    match self.get_edge(edge_id)? {
                        Some(edge) => AdjTarget {
                            neighbor_id: if incoming { edge.from_id } else { edge.to_id },
                            kind: edge.kind,
                        },
                        None => continue,
                    }
                }
            };
            // On the kind-scoped fast path, drop false positives from a longer
            // kind that shares this byte prefix (only possible when `kind`
            // itself contains `:`).
            if let Some(filter) = kind {
                if target.kind != filter {
                    continue;
                }
            }
            targets.push(target);
        }
        Ok(targets)
    }

    /// Collect outgoing edges for a node by scanning the `out:` prefix.
    fn outgoing_edges(&self, node_id: u64) -> Result<Vec<Edge>> {
        let prefix = out_prefix(node_id);
        let entries = self.backend.scan_prefix(&prefix)?;
        let mut edges = Vec::with_capacity(entries.len());
        for (key, _) in entries {
            let edge_id = edge_id_from_adjacency_key(&key, PREFIX_OUT);
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
            let edge_id = edge_id_from_adjacency_key(&key, PREFIX_IN);
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
                // v2: {prefix}{node_id_8}:{kind}:{edge_id_8}; the edge id is the
                // last 8 bytes in both v1 and v2. The v1 layout is the shortest
                // valid key, so use it as a lower bound.
                if key.len() < prefix.len() + 8 + 1 + 8 {
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

/// Build an outgoing adjacency key: `out:{from_id}:{kind}:{edge_id}` (v2,
/// #243 slice 2 — the edge `kind` is folded into the key so a kind-filtered
/// fan-out can sub-prefix scan `out:{from_id}:{kind}:` in `O(matches)` instead
/// of scanning every out-edge of a supernode).
///
/// The `edge_id` is always the **last 8 bytes** and the node id the first 8
/// after the prefix, so [`edge_id_from_adjacency_key`] and
/// [`u64_from_adjacency_key_first_id`] parse both this layout and the legacy
/// v1 `out:{from_id}:{edge_id}` without knowing which they hold.
fn out_edge_key(from_id: u64, kind: &str, edge_id: u64) -> Vec<u8> {
    adjacency_key(PREFIX_OUT, from_id, kind, edge_id)
}

/// Build an incoming adjacency key: `in:{to_id}:{kind}:{edge_id}` (v2, #243
/// slice 2). See [`out_edge_key`] for the layout rationale.
fn in_edge_key(to_id: u64, kind: &str, edge_id: u64) -> Vec<u8> {
    adjacency_key(PREFIX_IN, to_id, kind, edge_id)
}

/// Shared builder for the v2 kind-in-key adjacency layout
/// `{prefix}{node_id_8}:{kind}:{edge_id_8}`.
fn adjacency_key(prefix: &[u8], node_id: u64, kind: &str, edge_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(prefix.len() + 8 + 1 + kind.len() + 1 + 8);
    key.extend_from_slice(prefix);
    key.extend_from_slice(&node_id.to_le_bytes());
    key.push(b':');
    key.extend_from_slice(kind.as_bytes());
    key.push(b':');
    key.extend_from_slice(&edge_id.to_le_bytes());
    key
}

/// Build the **legacy v1** outgoing adjacency key `out:{from_id}:{edge_id}`.
///
/// Retained only for the format migration ([`Drevo::migrate_adjacency`]),
/// which must delete the exact pre-v2 keys, and for the byte-format tests.
fn out_edge_key_v1(from_id: u64, edge_id: u64) -> Vec<u8> {
    adjacency_key_v1(PREFIX_OUT, from_id, edge_id)
}

/// Build the **legacy v1** incoming adjacency key `in:{to_id}:{edge_id}`.
fn in_edge_key_v1(to_id: u64, edge_id: u64) -> Vec<u8> {
    adjacency_key_v1(PREFIX_IN, to_id, edge_id)
}

/// Shared builder for the v1 adjacency layout `{prefix}{node_id_8}:{edge_id_8}`.
fn adjacency_key_v1(prefix: &[u8], node_id: u64, edge_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(prefix.len() + 8 + 1 + 8);
    key.extend_from_slice(prefix);
    key.extend_from_slice(&node_id.to_le_bytes());
    key.push(b':');
    key.extend_from_slice(&edge_id.to_le_bytes());
    key
}

/// Build the scan prefix for **all** outgoing edges of a node:
/// `out:{node_id}:`. Matches both v1 and v2 keys (the kind segment sits after
/// this prefix), so full fan-out and the migration scan are layout-agnostic.
fn out_prefix(node_id: u64) -> Vec<u8> {
    let mut key = PREFIX_OUT.to_vec();
    key.extend_from_slice(&node_id.to_le_bytes());
    key.push(b':');
    key
}

/// Build the scan prefix for **all** incoming edges of a node: `in:{node_id}:`.
fn in_prefix(node_id: u64) -> Vec<u8> {
    let mut key = PREFIX_IN.to_vec();
    key.extend_from_slice(&node_id.to_le_bytes());
    key.push(b':');
    key
}

/// Build the kind-scoped scan prefix for outgoing edges of a node of a given
/// `kind`: `out:{node_id}:{kind}:` (v2 fast path, #243 slice 2).
///
/// Only meaningful on a v2 (kind-in-key) database. A `kind` containing `:`
/// can admit false positives (a longer kind sharing this byte prefix), so
/// callers MUST still confirm the decoded value's kind equals `kind`; it never
/// yields false negatives.
fn out_kind_prefix(node_id: u64, kind: &str) -> Vec<u8> {
    adjacency_kind_prefix(PREFIX_OUT, node_id, kind)
}

/// Build the kind-scoped scan prefix for incoming edges of a node of a given
/// `kind`: `in:{node_id}:{kind}:` (v2 fast path, #243 slice 2).
fn in_kind_prefix(node_id: u64, kind: &str) -> Vec<u8> {
    adjacency_kind_prefix(PREFIX_IN, node_id, kind)
}

/// Shared builder for the v2 kind-scoped scan prefix
/// `{prefix}{node_id_8}:{kind}:`.
fn adjacency_kind_prefix(prefix: &[u8], node_id: u64, kind: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(prefix.len() + 8 + 1 + kind.len() + 1);
    key.extend_from_slice(prefix);
    key.extend_from_slice(&node_id.to_le_bytes());
    key.push(b':');
    key.extend_from_slice(kind.as_bytes());
    key.push(b':');
    key
}

/// Classify an adjacency key as legacy **v1** (`{prefix}{node_8}:{edge_8}`)
/// versus **v2** (`{prefix}{node_8}:{kind}:{edge_8}`, #243 slice 2).
///
/// A v1 key has exactly 8 bytes after the `{prefix}{node_8}:` header (the
/// edge id); a v2 key has the `{kind}:` segment in between, so ≥ 9 bytes
/// remain (an empty kind still contributes the extra `:` delimiter). Used by
/// the open-time migration gate and never on the read hot path.
fn adjacency_key_is_v1(key: &[u8], prefix: &[u8]) -> bool {
    let header = prefix.len() + 8 + 1; // {prefix}{node_8}:
    key.len() == header + 8
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

/// Extract the edge ID from an adjacency key.
///
/// The edge id is always encoded as the **last 8 bytes** of the key, in both
/// the v1 (`{prefix}{node_8}:{edge_8}`) and v2 (`{prefix}{node_8}:{kind}:
/// {edge_8}`, #243 slice 2) layouts, so this parse is layout-agnostic and the
/// `prefix` argument is only used as a lower bound to reject malformed keys.
/// Panic-free per `drevo-rust` §"Error Handling".
fn edge_id_from_adjacency_key(key: &[u8], prefix: &[u8]) -> u64 {
    // Smallest valid key is v1: {prefix}{node_8}:{edge_8}.
    if key.len() < prefix.len() + 8 + 1 + 8 {
        return 0;
    }
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&key[key.len() - 8..]);
    u64::from_le_bytes(arr)
}

/// A denormalized adjacency target: the neighbor node id and the edge kind,
/// recovered from the adjacency **value** (or a `get_edge` fallback for legacy
/// entries) without loading the full edge record (#243 slice 1).
struct AdjTarget {
    neighbor_id: u64,
    kind: String,
}

/// One adjacency entry from a bounded page (#243 slice 3): the edge id, the
/// neighbor node id, and the edge kind — recovered from the adjacency value
/// (no `get_edge` on a denormalized database).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdjacencyEntry {
    /// The connecting edge's id.
    pub edge_id: u64,
    /// The node at the *other* end of the edge (its `to_id` for an outgoing
    /// page, its `from_id` for an incoming page).
    pub neighbor_id: u64,
    /// The edge's kind.
    pub kind: String,
}

/// A bounded page of a node's adjacency (#243 slice 3), as returned by
/// [`Drevo::outgoing_adjacency_page`] / [`Drevo::incoming_adjacency_page`].
///
/// `entries` holds up to the requested `limit` items in ascending key order.
/// `next` is an **opaque** cursor to pass as `after` for the following page,
/// or `None` when this page exhausted the node's edges (it was not full).
/// The entries are per-edge and **not** de-duplicated across parallel edges —
/// this is an edge-level iterator, not the distinct-neighbor set that
/// [`Drevo::neighbor_ids`] returns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdjacencyPage {
    /// The page's adjacency entries (at most `limit`).
    pub entries: Vec<AdjacencyEntry>,
    /// Opaque cursor for the next page, or `None` at the end.
    pub next: Option<Vec<u8>>,
}

/// Encode the denormalized adjacency **value** (#243 slice 1): the *other*
/// endpoint's node id (8-byte little-endian) followed by the edge `kind` as
/// UTF-8.
///
/// Storing the neighbor id + kind directly in the value lets
/// [`Drevo::neighbor_ids`] and kind-filtered fan-out answer "who is adjacent
/// to X" straight from the `out:`/`in:` prefix scan, with **zero** `get_edge`
/// point lookups — the property that keeps supernode traversal cheap.
/// Databases written before #243 store an empty value; readers fall back to
/// `get_edge` for those (see [`decode_adjacency_value`]) and can be upgraded
/// in place via [`Drevo::backfill_adjacency_values`].
///
/// A migrated value is always ≥ 8 bytes (even for an empty `kind`), so it is
/// unambiguously distinct from a legacy empty value.
fn adjacency_value(neighbor_id: u64, kind: &str) -> Vec<u8> {
    let mut value = Vec::with_capacity(8 + kind.len());
    value.extend_from_slice(&neighbor_id.to_le_bytes());
    value.extend_from_slice(kind.as_bytes());
    value
}

/// Decode an adjacency value written by [`adjacency_value`].
///
/// Returns `Some((neighbor_id, kind))` for a denormalized value, or `None`
/// for a legacy empty value (pre-#243), a too-short value, or a non-UTF-8
/// `kind` tail — in which case the caller recovers the data with a `get_edge`
/// fallback. Panic-free per `drevo-rust` §"Error Handling".
fn decode_adjacency_value(value: &[u8]) -> Option<(u64, &str)> {
    if value.len() < 8 {
        return None;
    }
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&value[..8]);
    let neighbor_id = u64::from_le_bytes(arr);
    let kind = std::str::from_utf8(&value[8..]).ok()?;
    Some((neighbor_id, kind))
}

/// Build a node kind index key: `node_kind:{kind}:{node_id}`.
fn node_kind_key(kind: &str, node_id: u64) -> Vec<u8> {
    let mut key = PREFIX_NODE_KIND.to_vec();
    key.extend_from_slice(kind.as_bytes());
    key.push(b':');
    key.extend_from_slice(&node_id.to_le_bytes());
    key
}

/// Resolve the faceting source text for a node and a `property` name.
///
/// `"title"` / `"body"` map to the node's title/body; any other name reads
/// that `properties` key (a JSON string verbatim, any other value via its
/// `to_string`). Returns `None` when the field is absent or empty so the
/// node simply contributes no keywords (see [`Drevo::facets`]).
fn node_property_text(node: &Node, property: &str) -> Option<String> {
    let text = match property {
        "title" => node.title.clone(),
        "body" => node.body.clone(),
        other => match node.properties.get(other)? {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => return None,
            value => value.to_string(),
        },
    };
    (!text.trim().is_empty()).then_some(text)
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
        // v2 (#243 slice 2): out:{from_id_8}:{kind}:{edge_id_8}.
        let key = out_edge_key(1, "likes", 5);
        assert!(key.starts_with(PREFIX_OUT));
        let rest = &key[PREFIX_OUT.len()..];
        assert_eq!(&rest[..8], &1u64.to_le_bytes());
        assert_eq!(rest[8], b':');
        assert_eq!(&rest[9..9 + "likes".len()], b"likes");
        assert_eq!(rest[9 + "likes".len()], b':');
        assert_eq!(&rest[rest.len() - 8..], &5u64.to_le_bytes());
    }

    #[test]
    fn in_edge_key_format() {
        let key = in_edge_key(2, "knows", 10);
        assert!(key.starts_with(PREFIX_IN));
        let rest = &key[PREFIX_IN.len()..];
        assert_eq!(&rest[..8], &2u64.to_le_bytes());
        assert_eq!(rest[8], b':');
        assert_eq!(&rest[9..9 + "knows".len()], b"knows");
        assert_eq!(&rest[rest.len() - 8..], &10u64.to_le_bytes());
    }

    #[test]
    fn out_prefix_matches_all_kinds_and_kind_prefix_is_narrower() {
        let all = out_prefix(3);
        let likes = out_edge_key(3, "likes", 99);
        let hates = out_edge_key(3, "hates", 7);
        // The bare node prefix matches every kind's key.
        assert!(likes.starts_with(&all));
        assert!(hates.starts_with(&all));
        // The kind-scoped prefix matches only its own kind.
        let likes_prefix = out_kind_prefix(3, "likes");
        assert!(likes.starts_with(&likes_prefix));
        assert!(!hates.starts_with(&likes_prefix));
    }

    #[test]
    fn adjacency_key_classifier_distinguishes_v1_and_v2() {
        let v1 = out_edge_key_v1(1, 42);
        let v2 = out_edge_key(1, "likes", 42);
        let v2_empty_kind = out_edge_key(1, "", 42);
        assert!(adjacency_key_is_v1(&v1, PREFIX_OUT));
        assert!(!adjacency_key_is_v1(&v2, PREFIX_OUT));
        // Even an empty-kind v2 key carries the extra ':' delimiter, so it is
        // never mistaken for v1.
        assert!(!adjacency_key_is_v1(&v2_empty_kind, PREFIX_OUT));
    }

    #[test]
    fn edge_id_from_adjacency_key_valid_for_both_layouts() {
        // The edge id is the last 8 bytes in both layouts.
        assert_eq!(
            edge_id_from_adjacency_key(&out_edge_key(1, "likes", 42), PREFIX_OUT),
            42
        );
        assert_eq!(
            edge_id_from_adjacency_key(&out_edge_key_v1(1, 42), PREFIX_OUT),
            42
        );
        assert_eq!(
            edge_id_from_adjacency_key(&out_edge_key(1, "", 7), PREFIX_OUT),
            7
        );
    }

    #[test]
    fn edge_id_from_adjacency_key_invalid_returns_zero() {
        let prefix = b"out:";
        let key = b"out:short";
        assert_eq!(edge_id_from_adjacency_key(key, prefix), 0);
    }

    #[cfg(feature = "redb-backend")]
    #[test]
    fn adjacency_format_major_matches_on_disk_format_major() {
        // The adjacency layout version and the redb on-disk format version are
        // bumped together (#243 slice 2), so a v2 adjacency file is exactly the
        // set of files an old build refuses via the on-disk major check.
        assert_eq!(ADJ_FORMAT_MAJOR, crate::storage::redb::FORMAT_MAJOR);
    }

    // --- Bloat report (#253 slice 1) ---

    #[test]
    fn bloat_report_in_memory_has_no_file_bytes_but_counts_logical() {
        use crate::model::{NewEdge, NewNode, Properties};
        let db = Drevo::open_in_memory().unwrap();
        let a = db
            .create_node(NewNode {
                kind: "n".into(),
                title: "a".into(),
                body: "hello".into(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap()
            .id;
        let b = db
            .create_node(NewNode {
                kind: "n".into(),
                title: "b".into(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap()
            .id;
        db.create_edge(NewEdge {
            from_id: a,
            to_id: b,
            kind: "knows".into(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .unwrap();

        let report = db.bloat_report().unwrap();
        // Ephemeral backend → no physical footprint, so no ratio.
        assert_eq!(report.file_bytes, None);
        assert_eq!(report.bloat_ratio, None);
        assert_eq!(report.node_count, 2);
        assert_eq!(report.edge_count, 1);
        assert!(
            report.logical_bytes > 0,
            "logical bytes should sum the node/edge records"
        );
        // stored_bytes counts records + every index, so it strictly exceeds the
        // records-only logical_bytes, and the split is exact.
        assert!(
            report.stored_bytes > report.logical_bytes,
            "stored ({}) must exceed records-only logical ({}) — indexes exist",
            report.stored_bytes,
            report.logical_bytes
        );
        assert_eq!(
            report.index_bytes,
            report.stored_bytes - report.logical_bytes
        );
        assert!(report.index_bytes > 0);
    }

    #[test]
    fn bloat_report_empty_db_has_zero_logical_and_no_ratio() {
        let db = Drevo::open_in_memory().unwrap();
        let report = db.bloat_report().unwrap();
        assert_eq!(report.node_count, 0);
        assert_eq!(report.edge_count, 0);
        assert_eq!(report.logical_bytes, 0);
        assert_eq!(report.bloat_ratio, None);
    }

    #[cfg(feature = "redb-backend")]
    #[test]
    fn bloat_report_on_disk_measures_file_and_ratio() {
        use crate::model::{NewNode, Properties};
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("bloat.redb");
        let db = Drevo::open(&path).unwrap();
        for i in 0..10 {
            db.create_node(NewNode {
                kind: "n".into(),
                title: format!("t{i}"),
                body: "x".repeat(64),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        }
        db.close().unwrap();

        let db = Drevo::open(&path).unwrap();
        let report = db.bloat_report().unwrap();
        assert!(report.file_bytes.is_some(), "disk backend measures itself");
        assert_eq!(report.node_count, 10);
        assert!(report.logical_bytes > 0);
        // stored_bytes = records + indexes ≥ records-only logical_bytes.
        assert!(report.stored_bytes >= report.logical_bytes);
        assert_eq!(
            report.index_bytes,
            report.stored_bytes - report.logical_bytes
        );
        // The honest ratio divides the file by the FULL stored footprint, so it
        // is defined and ≥ 1 (the physical file always covers its own data).
        let ratio = report.bloat_ratio.expect("ratio defined on disk");
        assert!(ratio >= 1.0, "physical must be ≥ stored, got {ratio}");
    }

    /// Regression guard for the honest ratio: on a text-heavy graph the FTS
    /// trigram index dominates the record rows, so `index_bytes` exceeds
    /// `logical_bytes`. The old `file / logical_bytes` ratio double-counted that
    /// index as "bloat"; the new `file / stored_bytes` ratio must not.
    #[cfg(feature = "redb-backend")]
    #[test]
    fn bloat_report_ratio_excludes_fts_index_footprint() {
        use crate::model::{NewNode, Properties};
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fts_bloat.redb");
        let db = Drevo::open(&path).unwrap();
        // Batch-create (one commit) so this stays fast on the slow CI runner.
        let nodes: Vec<NewNode> = (0..50)
            .map(|i| NewNode {
                kind: "n".into(),
                title: format!("t{i}"),
                body: format!(
                    "node {i} anxious deadlines mentoring graph vectors embeddings \
                     semantic search relationships knowledge base entity {i} lorem \
                     ipsum dolor sit amet consectetur adipiscing elit sed eiusmod"
                ),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .collect();
        db.create_nodes(nodes).unwrap();
        let report = db.bloat_report().unwrap();
        // FTS trigrams over the bodies make the index footprint dominate.
        assert!(
            report.index_bytes > report.logical_bytes,
            "index ({}) should dwarf records ({}) for text-heavy data",
            report.index_bytes,
            report.logical_bytes
        );
        let honest = report.bloat_ratio.expect("disk ratio defined");
        // The misleading old metric (file / logical) would read strictly larger,
        // because it divides by a strictly smaller denominator.
        let misleading = report.file_bytes.unwrap() as f64 / report.logical_bytes as f64;
        assert!(
            misleading > honest,
            "old metric ({misleading}) must over-report vs honest ({honest})"
        );
    }

    #[test]
    fn keyspace_stats_shows_fts_dominates_row_count() {
        use crate::model::{NewNode, Properties};
        let db = Drevo::open_in_memory().unwrap();
        // Text-heavy nodes → one FTS row per (trigram, node), so the `fts`
        // keyspace holds far more rows than the `node` records.
        let nodes: Vec<NewNode> = (0..40)
            .map(|i| NewNode {
                kind: "n".into(),
                title: format!("t{i}"),
                body: format!(
                    "node {i} anxious deadlines mentoring graph vectors embeddings \
                     semantic search relationships knowledge base entity {i} lorem \
                     ipsum dolor sit amet consectetur adipiscing elit"
                ),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .collect();
        db.create_nodes(nodes).unwrap();

        let stats = db.keyspace_stats().unwrap();
        // Sorted by descending row count → the FTS keyspace is first.
        assert_eq!(
            stats[0].prefix, "fts",
            "fts should be the largest keyspace by row count, got {stats:?}"
        );
        let fts = stats.iter().find(|s| s.prefix == "fts").unwrap();
        let node = stats.iter().find(|s| s.prefix == "node").unwrap();
        assert_eq!(node.entries, 40, "one row per node record");
        // The blowup driver: FTS rows outnumber node records by a large factor.
        assert!(
            fts.entries > node.entries * 10,
            "fts rows ({}) should dwarf node rows ({})",
            fts.entries,
            node.entries
        );
    }

    // --- Auto-compaction policy (#253 slice 2) ---

    #[test]
    fn auto_compact_policy_defaults_are_off_and_conservative() {
        let p = AutoCompactPolicy::default();
        assert!(!p.enabled);
        assert_eq!(p.min_ratio, 2.0);
        assert_eq!(p.min_bytes, 10 * 1024 * 1024);
    }

    #[test]
    fn auto_compact_policy_from_env_parses_all_knobs() {
        let env = |k: &str| -> Option<String> {
            match k {
                "DREVO_AUTO_COMPACT" => Some("On".to_string()),
                "DREVO_AUTO_COMPACT_RATIO" => Some("3.5".to_string()),
                "DREVO_AUTO_COMPACT_MIN_BYTES" => Some("1048576".to_string()),
                _ => None,
            }
        };
        let p = AutoCompactPolicy::from_env(env);
        assert!(p.enabled);
        assert_eq!(p.min_ratio, 3.5);
        assert_eq!(p.min_bytes, 1_048_576);
    }

    #[test]
    fn auto_compact_policy_from_env_disabled_and_bad_values_fall_back() {
        // Absent → disabled with defaults.
        let p = AutoCompactPolicy::from_env(|_| None);
        assert!(!p.enabled);
        assert_eq!(p.min_ratio, 2.0);
        // Present-but-falsey stays disabled; unparseable numbers fall back.
        let env = |k: &str| -> Option<String> {
            match k {
                "DREVO_AUTO_COMPACT" => Some("no".to_string()),
                "DREVO_AUTO_COMPACT_RATIO" => Some("not-a-number".to_string()),
                "DREVO_AUTO_COMPACT_MIN_BYTES" => Some("-5".to_string()),
                _ => None,
            }
        };
        let p = AutoCompactPolicy::from_env(env);
        assert!(!p.enabled);
        assert_eq!(p.min_ratio, 2.0);
        assert_eq!(p.min_bytes, 10 * 1024 * 1024);
    }

    #[test]
    fn maybe_auto_compact_disabled_policy_is_noop() {
        let mut db = Drevo::open_in_memory().unwrap();
        let policy = AutoCompactPolicy {
            enabled: false,
            min_ratio: 1.0,
            min_bytes: 0,
        };
        assert!(db.maybe_auto_compact(&policy).unwrap().is_none());
    }

    #[test]
    fn maybe_auto_compact_in_memory_never_compacts() {
        use crate::model::{NewNode, Properties};
        let mut db = Drevo::open_in_memory().unwrap();
        db.create_node(NewNode {
            kind: "n".into(),
            title: "a".into(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();
        // Enabled with the lowest possible thresholds, but the in-memory backend
        // has no file_bytes → nothing to reclaim.
        let policy = AutoCompactPolicy {
            enabled: true,
            min_ratio: 1.0,
            min_bytes: 0,
        };
        assert!(db.maybe_auto_compact(&policy).unwrap().is_none());
    }

    #[cfg(feature = "redb-backend")]
    #[test]
    fn maybe_auto_compact_respects_thresholds_and_runs_when_enabled() {
        use crate::model::{NewNode, Properties};
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ac.redb");
        {
            let db = Drevo::open(&path).unwrap();
            for i in 0..20 {
                db.create_node(NewNode {
                    kind: "n".into(),
                    title: format!("t{i}"),
                    body: "x".repeat(128),
                    body_html: String::new(),
                    properties: Properties::default(),
                })
                .unwrap();
            }
            db.close().unwrap();
        }

        // Disabled → no-op even though the file exists.
        let mut db = Drevo::open(&path).unwrap();
        assert!(db
            .maybe_auto_compact(&AutoCompactPolicy {
                enabled: true,
                min_ratio: 1.0,
                min_bytes: u64::MAX, // file is below this floor → skip
            })
            .unwrap()
            .is_none());
        // Ratio floor above the real ratio → skip.
        assert!(db
            .maybe_auto_compact(&AutoCompactPolicy {
                enabled: true,
                min_ratio: 1e9,
                min_bytes: 0,
            })
            .unwrap()
            .is_none());
        // Enabled, thresholds satisfied (any on-disk file has ratio ≥ 1) → a
        // compaction runs and reports before/after byte counts.
        let report = db
            .maybe_auto_compact(&AutoCompactPolicy {
                enabled: true,
                min_ratio: 1.0,
                min_bytes: 0,
            })
            .unwrap()
            .expect("compaction should have run");
        assert!(report.bytes_before.is_some());
        assert!(report.bytes_after.is_some());
        // Data is intact after the reclaim.
        assert_eq!(db.bloat_report().unwrap().node_count, 20);
    }

    // --- Semantic-index registry persistence (#251) ---

    #[cfg(feature = "redb-backend")]
    #[test]
    fn semantic_registry_survives_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sem.redb");
        {
            let db = Drevo::open(&path).unwrap();
            db.semantic_register("Entity", "summary", "embedding", IndexMode::Auto, None)
                .unwrap();
            db.close().unwrap();
        }
        // A fresh open must restore the registered target from redb.
        let db = Drevo::open(&path).unwrap();
        let status = db.semantic_status();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].label, "Entity");
        assert_eq!(status[0].text_property, "summary");
        assert_eq!(status[0].embedding_property, "embedding");
        assert_eq!(status[0].mode, IndexMode::Auto);
    }

    #[test]
    fn semantic_registry_empty_by_default() {
        let db = Drevo::open_in_memory().unwrap();
        assert!(db.semantic_status().is_empty());
    }

    // --- Adjacency value codec (#243 slice 1) ---

    #[test]
    fn adjacency_value_roundtrips() {
        let v = adjacency_value(0xDEAD_BEEF, "knows");
        assert_eq!(v.len(), 8 + "knows".len());
        assert_eq!(decode_adjacency_value(&v), Some((0xDEAD_BEEF, "knows")));
    }

    #[test]
    fn adjacency_value_empty_kind_is_still_denormalized() {
        // An edge with an empty kind still yields an 8-byte value — distinct
        // from a legacy empty value, so it decodes rather than falling back.
        let v = adjacency_value(42, "");
        assert_eq!(v.len(), 8);
        assert_eq!(decode_adjacency_value(&v), Some((42, "")));
    }

    #[test]
    fn decode_legacy_and_malformed_values_return_none() {
        assert_eq!(
            decode_adjacency_value(&[]),
            None,
            "legacy empty -> fallback"
        );
        assert_eq!(decode_adjacency_value(&[1, 2, 3]), None, "too short");
        // 8 id bytes + an invalid UTF-8 kind tail -> None (panic-free).
        let mut bad = 7u64.to_le_bytes().to_vec();
        bad.push(0xFF);
        assert_eq!(decode_adjacency_value(&bad), None);
    }

    // --- neighbor_ids reads from the value, not `get_edge` (#243) ---

    /// Wraps a `MemoryBackend` and counts `get` calls that target an `edge:`
    /// record. The counter is an `Arc<AtomicU64>` so the test can hold its own
    /// clone and read it without downcasting the boxed trait object.
    struct CountingBackend {
        inner: MemoryBackend,
        edge_gets: std::sync::Arc<AtomicU64>,
    }

    impl StorageBackend for CountingBackend {
        fn get(&self, key: &[u8]) -> crate::storage::error::Result<Option<Vec<u8>>> {
            // Count only `edge:{id}` record reads, not edge_uuid:/edge_kind:.
            if key.len() == PREFIX_EDGE.len() + 8 && key.starts_with(PREFIX_EDGE) {
                self.edge_gets.fetch_add(1, Ordering::Relaxed);
            }
            self.inner.get(key)
        }
        fn put(&self, key: &[u8], value: &[u8]) -> crate::storage::error::Result<()> {
            self.inner.put(key, value)
        }
        fn put_batch(&self, items: &[(Vec<u8>, Vec<u8>)]) -> crate::storage::error::Result<()> {
            self.inner.put_batch(items)
        }
        fn delete(&self, key: &[u8]) -> crate::storage::error::Result<()> {
            self.inner.delete(key)
        }
        fn scan_prefix(
            &self,
            prefix: &[u8],
        ) -> crate::storage::error::Result<Vec<(Vec<u8>, Vec<u8>)>> {
            self.inner.scan_prefix(prefix)
        }
        fn flush(&self) -> crate::storage::error::Result<()> {
            self.inner.flush()
        }
    }

    #[test]
    fn neighbor_ids_does_zero_edge_loads_but_edges_of_loads_all() {
        use crate::model::{Direction, NewEdge, NewNode, Properties};

        let edge_gets = std::sync::Arc::new(AtomicU64::new(0));
        let db = Drevo {
            backend: Box::new(CountingBackend {
                inner: MemoryBackend::new(),
                edge_gets: std::sync::Arc::clone(&edge_gets),
            }),
            next_node_id: AtomicU64::new(1),
            next_edge_id: AtomicU64::new(1),
            counter_drift_repaired: AtomicBool::new(false),
            tx_state: Mutex::new(TxState::Idle),
            semantic: Mutex::new(SemanticIndexRegistry::new()),
            #[cfg(feature = "http")]
            embedder: std::sync::OnceLock::new(),
            #[cfg(feature = "http")]
            embed_failures: Mutex::new(std::collections::HashMap::new()),
            #[cfg(feature = "http")]
            embedder_dimension: Mutex::new(None),
            rel_semantic: Mutex::new(SemanticIndexRegistry::new()),
        };

        let mk = |title: &str| {
            db.create_node(NewNode {
                kind: "n".into(),
                title: title.into(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap()
            .id
        };
        let a = mk("a");
        let targets: Vec<u64> = (0..10).map(|i| mk(&format!("t{i}"))).collect();
        for (i, &to) in targets.iter().enumerate() {
            db.create_edge(NewEdge {
                from_id: a,
                to_id: to,
                kind: if i % 2 == 0 { "even" } else { "odd" }.into(),
                weight: 1.0,
                properties: Properties::default(),
            })
            .unwrap();
        }

        // neighbor_ids recovers every neighbor from the adjacency value —
        // zero `edge:` record reads — and applies the kind filter in memory.
        let before = edge_gets.load(Ordering::Relaxed);
        let ids = db.neighbor_ids(a, Direction::Outgoing, None).unwrap();
        let evens = db
            .neighbor_ids(a, Direction::Outgoing, Some("even"))
            .unwrap();
        let after = edge_gets.load(Ordering::Relaxed);
        assert_eq!(ids.len(), 10, "all distinct fan-out neighbors surfaced");
        assert_eq!(
            evens.len(),
            5,
            "kind filter applied straight from the value"
        );
        assert_eq!(
            after - before,
            0,
            "neighbor_ids must not load any edge record on a denormalized db"
        );

        // Sanity: the full-edge path *does* load every edge, so the zero above
        // reflects a real reduction, not an unmeasured path.
        let before = edge_gets.load(Ordering::Relaxed);
        let edges = db.edges_of(a, Direction::Outgoing).unwrap();
        let after = edge_gets.load(Ordering::Relaxed);
        assert_eq!(edges.len(), 10);
        assert_eq!(after - before, 10, "edges_of loads one edge per neighbor");
    }

    // --- backfill upgrades legacy empty adjacency values (#243) ---

    #[test]
    fn backfill_upgrades_legacy_empty_values() {
        use crate::model::{Direction, NewEdge, NewNode, Properties};

        let db = Drevo::open_in_memory().unwrap();
        let mk = |title: &str| {
            db.create_node(NewNode {
                kind: "n".into(),
                title: title.into(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap()
            .id
        };
        let a = mk("a");
        let b = mk("b");
        let e = db
            .create_edge(NewEdge {
                from_id: a,
                to_id: b,
                kind: "knows".into(),
                weight: 1.0,
                properties: Properties::default(),
            })
            .unwrap();

        // Simulate a pre-#243-slice-1 empty adjacency value on the v2 key,
        // the way an older drevo stored them (value empty, key kind-in-key).
        db.backend
            .put(&out_edge_key(a, "knows", e.id), &[])
            .unwrap();
        db.backend.put(&in_edge_key(b, "knows", e.id), &[]).unwrap();

        // Reads still work via the get_edge fallback...
        assert_eq!(
            db.neighbor_ids(a, Direction::Outgoing, None).unwrap(),
            vec![b]
        );

        // ...and backfill upgrades both entries (out + in) exactly once.
        assert_eq!(db.backfill_adjacency_values().unwrap(), 2);
        assert_eq!(db.backfill_adjacency_values().unwrap(), 0, "idempotent");

        // The values are now denormalized and match the edge.
        assert_eq!(
            decode_adjacency_value(
                &db.backend
                    .get(&out_edge_key(a, "knows", e.id))
                    .unwrap()
                    .unwrap()
            ),
            Some((b, "knows"))
        );
        assert_eq!(
            decode_adjacency_value(
                &db.backend
                    .get(&in_edge_key(b, "knows", e.id))
                    .unwrap()
                    .unwrap()
            ),
            Some((a, "knows"))
        );
        assert!(db.verify_invariants().unwrap().is_empty());
    }

    // --- bounded adjacency page: legacy fallback (#243 slice 3) ---

    #[test]
    fn adjacency_page_falls_back_to_get_edge_on_legacy_empty_value() {
        use crate::model::{NewEdge, NewNode, Properties};

        let db = Drevo::open_in_memory().unwrap();
        let mk = |title: &str| {
            db.create_node(NewNode {
                kind: "n".into(),
                title: title.into(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap()
            .id
        };
        let a = mk("a");
        let b = mk("b");
        let e = db
            .create_edge(NewEdge {
                from_id: a,
                to_id: b,
                kind: "knows".into(),
                weight: 1.0,
                properties: Properties::default(),
            })
            .unwrap();

        // Simulate a pre-#243 empty adjacency value; the page must still
        // recover (neighbor_id, kind) via the get_edge fallback.
        db.backend
            .put(&out_edge_key(a, "knows", e.id), &[])
            .unwrap();
        let page = db.outgoing_adjacency_page(a, None, 10).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].neighbor_id, b);
        assert_eq!(page.entries[0].kind, "knows");
        assert_eq!(page.entries[0].edge_id, e.id);
        assert!(page.next.is_none());
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

    // --- BM25 ranking (task 00131) ---

    #[test]
    fn bm25_term_frequency_saturates() {
        // BM25's k1 term means the 10th occurrence of a term must not
        // count 10x. With k1=1.2 the marginal gain per extra occurrence
        // shrinks, so the score from tf=10 is far less than 10x the tf=1
        // score after IDF/length effects cancel (both docs equal length-ish).
        let db = Drevo::open_in_memory().unwrap();
        // Doc 1: one occurrence; Doc 2: many occurrences, padded so the
        // length-normalization term is comparable.
        db.create_node(test_node("note", "rust once", "alpha beta gamma delta"))
            .unwrap();
        db.create_node(test_node(
            "note",
            "rust rust rust rust rust rust rust rust rust rust",
            "",
        ))
        .unwrap();
        let results = db.search_fts("rust", 10).unwrap();
        assert_eq!(results.len(), 2);
        let many = results.iter().find(|r| r.node.id == 2).unwrap();
        let once = results.iter().find(|r| r.node.id == 1).unwrap();
        // More occurrences still score higher, but with diminishing return.
        assert!(many.score > once.score);
        assert!(
            many.score < once.score * 10.0,
            "k1 saturation should keep tf=10 well under 10x the tf=1 score \
             (many={}, once={})",
            many.score,
            once.score
        );
    }

    #[test]
    fn bm25_length_normalization_prefers_shorter_doc() {
        // Two docs each contain "rust" once, but one is much longer. With
        // b=0.75 the shorter (more focused) document should rank higher.
        let db = Drevo::open_in_memory().unwrap();
        // Distinct titles (Drevo enforces unique titles), same query term once each.
        db.create_node(test_node("note", "rust north", "")).unwrap();
        db.create_node(test_node(
            "note",
            "rust south",
            "this is a very long body about many unrelated subjects such as \
             cooking gardening astronomy philosophy economics and history",
        ))
        .unwrap();
        let results = db.search_fts("rust", 10).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].node.id, 1,
            "the shorter document should rank first under BM25 length norm"
        );
    }

    #[test]
    fn bm25_rare_term_outranks_common_term() {
        // IDF: a term appearing in few docs is more salient.
        let db = Drevo::open_in_memory().unwrap();
        for i in 0..12 {
            db.create_node(test_node("note", &format!("common report {}", i), ""))
                .unwrap();
        }
        db.create_node(test_node("note", "common quasar sighting", ""))
            .unwrap();
        let results = db.search_fts("common quasar", 10).unwrap();
        assert!(!results.is_empty());
        assert_eq!(
            results[0].node.title, "common quasar sighting",
            "the doc with the rare term must rank first"
        );
    }

    #[test]
    fn search_fts_ranked_tfidf_flag_preserves_legacy_scorer() {
        // The deterministic-baseline flag must still return matches.
        let db = Drevo::open_in_memory().unwrap();
        db.create_node(test_node("note", "Rust programming", ""))
            .unwrap();
        let results = db.search_fts_ranked("rust", 10, FtsRanking::TfIdf).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].score > 0.0);
    }

    #[test]
    fn search_fts_default_is_bm25() {
        // search_fts and search_fts_ranked(.., Bm25) must agree.
        let db = Drevo::open_in_memory().unwrap();
        db.create_node(test_node("note", "Rust programming", "rust systems"))
            .unwrap();
        db.create_node(test_node("note", "Python scripting", ""))
            .unwrap();
        let a = db.search_fts("rust", 10).unwrap();
        let b = db
            .search_fts_ranked("rust", 10, FtsRanking::default())
            .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn search_fts_doc_length_stats_maintained_on_delete() {
        // Deleting a node must drop its length stat (cascade-on-delete) so
        // avgdl reflects only live documents.
        let db = Drevo::open_in_memory().unwrap();
        let n = db
            .create_node(test_node("note", "rust programming", ""))
            .unwrap();
        let before = fts_index::corpus_stats(&*db.backend).unwrap();
        assert_eq!(before.doc_count, 1);
        db.delete_node(n.id).unwrap();
        let after = fts_index::corpus_stats(&*db.backend).unwrap();
        assert_eq!(after.doc_count, 0);
        assert_eq!(after.total_len, 0);
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
