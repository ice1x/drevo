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

## Standing items toward "surpass Memgraph" (RFC Phase 0)

- Add the Memgraph column: same GraphML, same queries over Bolt, dockerised —
  the cross-database half of this scoreboard.
- Re-run after the arena/CSR native internals (RFC Phase 2 completion) and
  after id-seek pushdown; update this table (append runs, keep history).
