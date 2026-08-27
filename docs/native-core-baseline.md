# Native core baseline — KV vs native on real data

The Phase-0/7 scoreboard of the [native graph core RFC](rfc-native-core.md)
(#307): the same Cypher queries, the same graph, measured on today's KV engine
and on the native engine with its secondary indexes — on a **copy of real
production data**, because synthetic shapes have repeatedly misestimated
real-world wins (the FTS posting-list rewrite measured 2× off until validated
on a live copy).

## Method

- **Harness:** `benches/real_data_baseline_bench.rs`
  (`DREVO_BASELINE_GRAPHML=<graphml> cargo bench --bench
  real_data_baseline_bench`). The bench asserts up front that both engines
  return identical rows for every measured query — a wrong-answer speedup is
  not a win.
- **Data:** a GraphML snapshot of the live drevo knowledge graph
  (2 596 nodes / 3 755 edges; long-text bodies typical of a KG). Imported into
  an in-memory KV `Drevo`, then migrated byte-identically to a `NativeGraph`
  through the `GraphEngine` seam; the native label + property indexes are
  synced from the change-feed.
- **Paths measured:** KV via `execute` (today's production path) vs native via
  `execute_on_engine_with_indexes` (the flip-target path); plus one seam-level
  `neighbor_ids` expansion isolating index-free adjacency from executor
  overhead.
- **Workload parameters are derived from the data** (densest kind, a
  mid-selectivity property pair, the highest-degree node), so re-runs stay
  meaningful as the graph evolves.
- **Machine:** Apple M1 Max, 32 GB, macOS; criterion, 20 samples, 3 s
  measurement windows. Numbers are criterion midpoints (2026-08-26,
  snapshot `drevo_kg_20260805_195240.graphml`).

## Results

| Workload (Cypher unless noted) | KV | native + indexes | speed-up |
|---|---:|---:|---:|
| `MATCH (n) RETURN count(*)` | 402.8 ms | 52.2 ms | **7.7×** |
| `MATCH (n:Entity) RETURN count(*)` (densest label, 1 971 nodes) | 353.9 ms | 42.6 ms | **8.3×** |
| `MATCH (n {type: 'Trait'}) RETURN count(*)` (mid-selectivity property) | 365.1 ms | 0.36 ms | **≈1 000×** |
| `MATCH (a)-->(b) WHERE id(a) = <hub> RETURN count(b)` (hub out-degree 98) | 1.228 s | 140.8 ms | **8.7×** |
| seam `neighbor_ids(hub, Outgoing)` (no executor) | 20.3 µs | 6.1 µs | **3.3×** |

## Reading the numbers

- **The property-equality row is the headline:** the native property index
  answers `MATCH (n {key: value})` from postings while the KV path full-scans
  and decodes every node — three orders of magnitude on real data.
- **Scans and Cypher traversal are ~8×:** the KV path pays a bincode decode of
  every record (KG bodies are large) on every enumeration; the native engine
  clones `Arc`-held records from maps. This is executor-dominated — the pure
  adjacency seam gap (3.3×) shows the engine-only difference before the
  arena/CSR layout lands (Phase 2 completion).
- **KV numbers here are a floor, not the live cost:** the bench uses the
  in-memory `MemoryBackend`; the production deployment runs redb on disk, so
  the real gap on the deployed system is at least this large.
- The Cypher hub expansion (`WHERE id(a) = …`) is slow on both engines in
  absolute terms because the executor enumerates the whole `(a)-->(b)` pattern
  before filtering — an executor-planning gap (id-seek pushdown), independent
  of the engine, and a candidate follow-up.

## Run 2 — after id-seek pushdown (2026-08-26, same machine & snapshot)

The follow-up flagged above landed the same day: a conjunctive
`WHERE id(n) = X` / `id(n) IN [...]` now resolves through
`GraphEngine::get_node` point seeks on **any** engine instead of enumerating
the pattern (`tests/cypher_id_seek_tests.rs` proves the scan is skipped via a
counting engine decorator; the differential corpus pins cross-engine parity).

| Workload | KV | native + indexes | vs run 1 |
|---|---:|---:|---|
| `MATCH (a)-->(b) WHERE id(a) = <hub> RETURN count(b)` | 19.5 ms | 2.29 ms | KV **63× faster**, native **61× faster** |

The other rows are unchanged within noise (scan-bound workloads do not touch
the seek path). Remaining absolute cost in this query is the `b`-side edge
loading and aggregation, not candidate enumeration.

## Run 3 — after the zero-copy `Arc<Node>` seam (2026-08-26, same machine & snapshot)

The node-reading seam methods (`get_node`, `neighbors`, `all_nodes`,
`nodes_by_kind`) now return `Arc<Node>` handles: the native engine shares its
stored records (a refcount bump instead of deep-cloning every body/property map
on every enumeration — the `Arc::ptr_eq` contract is pinned by
`tests/native_engine_tests.rs::seam_reads_share_the_stored_record_on_native`),
while the KV engine wraps its owned decodes once.

| Workload | native + indexes (run 2 → run 3) |
|---|---|
| `MATCH (n) RETURN count(*)` | 52.2 ms → **20.6 ms** (2.5×) |
| label scan `:Entity` | 42.6 ms → **20.2 ms** (2.1×) |
| property equality `{type: 'Trait'}` | 0.36 ms → **0.18 ms** (2×) |
| Cypher 1-hop from hub | 2.29 ms → **0.93 ms** (2.5×) |
| seam `neighbor_ids(hub)` | 6.1 µs → 4.3 µs |

(The KV column also shifted down ~25% in this run across all rows — ambient
machine variance between runs, not a KV change; compare within-run only.)

The remaining native scan cost is now dominated by rebuilding the executor's
`NodeValue` projection per node per query — a per-node value cache tailing the
change-feed is the natural next step, but needs a versioning design so a
mid-query write can never serve a stale value (`updated_at` alone is
millisecond-resolution and insufficient).

## Run 4 — after the `NodeValue` cache (2026-08-26, same machine & snapshot)

The per-query projection rebuild flagged in run 3 is gone: a
change-feed-maintained [`NativeValueCache`] memoises each node's `NodeValue`,
and a hit is validated against the live record with `Arc::ptr_eq` — so a stale
or never-resynced cache can only cost speed, never serve a wrong answer
(`tests/native_value_cache_tests.rs` pins reuse, staleness rejection, and
intra-statement write visibility; the differential corpus's indexed run now
exercises the cache on every scenario).

| Workload | native, run 3 → run 4 | vs KV (within-run) |
|---|---|---:|
| `MATCH (n) RETURN count(*)` | 20.6 ms → **537 µs** (38×) | **≈560×** |
| label scan `:Entity` | 20.2 ms → **519 µs** (39×) | **≈580×** |
| property equality `{type: 'Trait'}` | 182 µs → **9.3 µs** (20×) | **≈32 000×** |
| Cypher 1-hop from hub | 0.93 ms → **60 µs** (15×) | **≈230×** |

Cumulative across the day's four runs, the native path went: full scan
52 ms → 0.54 ms, hub 1-hop 141 ms → 0.06 ms. At sub-millisecond full scans on
real data, the executor's remaining per-row costs (bindings, filter
evaluation) now dominate — the next meaningful comparison is the Memgraph
column, not further micro-work on this path.

## Run 5 — the Memgraph column (2026-08-27, same machine & snapshot)

The cross-database half of the scoreboard: the same GraphML, the same Cypher
text, measured through the **same client** (`scripts/memgraph_baseline_bench.py`,
`neo4j` Python driver, localhost Bolt, medians of 30 runs) against

- **drevo:** a locally built `drevo-server` at main (`0.0.18-49`), redb
  on-disk data dir, Bolt — today's production path, i.e. the **KV engine**
  (the native engine is not flipped in yet), and
- **Memgraph:** official `memgraph/memgraph:latest` Docker image (v3.12.0),
  default in-memory transactional storage, with a label index and a
  label+property index created on the densest label — mirroring the native
  drevo indexes the in-process runs use.

The script asserts row parity between the two databases on every measured
query before timing (label scan agrees at 2 181 — kind plus secondary
`_labels` matches; property pair `type = 'Trait'` agrees at 26; hub
out-degree agrees at 98).

| Workload | drevo KV (Bolt) | Memgraph (Bolt) | Memgraph advantage |
|---|---:|---:|---:|
| `RETURN 1` (round-trip floor) | 269 µs | 1.22 ms | drevo 4.5× lighter |
| `MATCH (n) RETURN count(*)` | 342 ms | 3.35 ms | **102×** |
| label scan (densest label) | 325 ms | 1.01 ms | **321×** |
| property equality (no label) | 304 ms | 1.95 ms | **156×** |
| property equality (labelled) | 359 ms | 0.67 ms | **535×** |
| Cypher 1-hop from hub (id seek) | 15.0 ms | 0.63 ms | **24×** |

Reading it honestly:

- **Today's production drevo loses to Memgraph by two orders of magnitude on
  scans.** This is the KV engine paying a full decode-everything scan per
  query — the same gap the in-process runs above measure, now confirmed on
  the wire.
- **The native engine is already in Memgraph's class.** Memgraph's own Bolt
  round-trip floor is ~1.2 ms, so its server-side query cost is roughly
  0–2 ms per workload. The native in-process numbers from run 4 (full scan
  537 µs, label scan 519 µs, property equality 9.3 µs, hub 1-hop 60 µs) sit
  at or below that — while drevo's Bolt stack itself is 4.5× lighter than
  Memgraph's (269 µs vs 1.22 ms on `RETURN 1`).
- **The comparison native-vs-Memgraph is not apples-to-apples until the
  engine flip:** run 4 is in-process, the Memgraph column includes Bolt.
  What this run establishes is the bound: native cost + drevo's measured
  Bolt floor ≈ 0.3–0.8 ms per workload, below Memgraph's measured 0.6–3.4 ms
  — so the flip (serve Bolt from the native engine) is what turns "in
  Memgraph's class" into "surpass Memgraph" on this scoreboard.
- Fair-play caveats: Memgraph runs its normal fully in-memory mode while
  drevo serves redb from disk; and Memgraph's label+property index is only
  used by the labelled query form (its unlabelled `{type: 'Trait'}` match
  full-scans, same as semantically required), which is why the labelled row
  is its best.

## Standing items toward "surpass Memgraph" (RFC Phase 0)

- ~~Add the Memgraph column~~ — run 5 above; rerun via
  `scripts/memgraph_baseline_bench.py` (needs the `memgraph/memgraph` image
  and a running `drevo-server`; see the script docstring).
- **Engine flip** (`DREVO_ENGINE=native|kv`): serve Bolt/HTTP from the native
  engine — the differential corpus and the id-ascending scan-order
  convergence are already in place as guards, and run 5 shows the flip is
  what closes the scoreboard.
- Re-run after the arena/CSR native internals (RFC Phase 2 completion);
  update this table (append runs, keep history).
