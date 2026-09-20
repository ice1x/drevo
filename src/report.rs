//! Engine-independent storage & semantic **report** DTOs.
//!
//! These small serde-friendly value types describe the *result* of a graph
//! operation — a compaction, a bloat scan, a per-keyspace breakdown, a semantic
//! reindex pass, or a semantic-target health snapshot — independently of which
//! engine produced them. They live here (rather than in `crate::db`) so they
//! survive the retirement of the legacy KV engine (epic #444): the durable
//! native serving layer (`crate::native_service` / `crate::native_api`) and the
//! Cypher executor return and consume them directly.

use crate::semantic_index::SemanticIndex;

/// Outcome of one `Drevo::semantic_reindex` backfill pass (#262).
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

/// Structured report produced by `Drevo::compact` (Phase 9 task `00054`).
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
/// `Drevo::compact` (or the `drevo shrink` CLI) reclaims it. The ratio is
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
/// copy-on-write high-water-mark slack that `Drevo::compact` / `drevo shrink`
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
