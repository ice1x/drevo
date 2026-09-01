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

## Run 6 — the engine flip closes the scoreboard (2026-08-28, same machine & snapshot)

The same cross-database harness as run 5 (`scripts/memgraph_baseline_bench.py`,
same client, same queries, row parity asserted), but drevo now runs with
**`DREVO_ENGINE=native`** — the read mirror serving read-only Cypher from the
native engine over the same Bolt wire (slices A/B of Phase 6). This is the
apples-to-apples comparison run 5 said was missing.

| Workload | drevo native (Bolt) | Memgraph (Bolt) | vs run 5's KV column |
|---|---:|---:|---:|
| `RETURN 1` (round-trip floor) | 232 µs | 693 µs | — |
| `MATCH (n) RETURN count(*)` | 1.08 ms | **0.89 ms** | 317× faster |
| label scan (densest label) | 935 µs | **789 µs** | 348× faster |
| property equality (no label) | **276 µs** | 2.18 ms | 1 100× faster |
| property equality (labelled) | **544 µs** | 672 µs | 660× faster |
| Cypher 1-hop from hub (id seek) | **322 µs** | 665 µs | 47× faster |

(Memgraph's own numbers moved vs run 5 — its round-trip floor halved — so
compare within-run, as always.)

Reading it:

- **drevo wins four of six rows** through the identical client: both
  property-equality forms (7.9× on the unlabelled one, where drevo's native
  property index answers a query Memgraph must full-scan by semantics), the
  hub 1-hop (2.1×), and the round-trip floor (3× lighter).
- **Memgraph keeps a ~1.2× edge on the two bare count-scans** (0.89 ms vs
  1.08 ms; 0.79 ms vs 0.94 ms). At this depth the remaining drevo cost is
  the executor's per-row binding/aggregation work plus the HashMap-based
  native layout — exactly what the arena/CSR internals (RFC Phase 2
  completion) target. "Догнать" is done; the last 20% of "перегнать" on
  full scans is Phase 2's job.
- The production default stays `DREVO_ENGINE=kv`; the flip is opt-in until
  the mirror has soaked. Writes always execute on KV either way — the flip
  can change read latency, never durability or answers.

## Run 7 — count pushdown: the sweep (2026-08-28, same machine & snapshot)

Run 6 left Memgraph a ~1.2× edge on the two bare count-scans, with the
remaining drevo cost identified as executor per-row work spent producing a
single integer. The count pushdown removes that work entirely: a bare
`MATCH (n[:Label]) RETURN count(*)` is now answered from cardinalities —
`GraphEngine::count_nodes` on any engine, the kind bucket + label index for
the labelled form — with the detector conservative enough that any guarded
shape (`WHERE`, properties, `DISTINCT`, relationships…) keeps the ordinary
scan (`tests/cypher_count_pushdown_tests.rs`; corpus scenario pins
cross-engine parity). Same harness as runs 5–6, drevo with
`DREVO_ENGINE=native`:

| Workload | drevo native (Bolt) | Memgraph (Bolt) | drevo advantage |
|---|---:|---:|---:|
| `RETURN 1` (round-trip floor) | 225 µs | 649 µs | 2.9× |
| `MATCH (n) RETURN count(*)` | **234 µs** | 624 µs | **2.7×** |
| label scan (densest label) | **285 µs** | 662 µs | **2.3×** |
| property equality (no label) | **223 µs** | 1.30 ms | **5.8×** |
| property equality (labelled) | **376 µs** | 536 µs | **1.4×** |
| Cypher 1-hop from hub (id seek) | **283 µs** | 489 µs | **1.7×** |

**drevo wins every row.** The count rows collapsed to the Bolt floor
(234 µs vs the 225 µs `RETURN 1`) — the query cost is now the wire, not the
engine. On this scoreboard, on this real-data snapshot, "догнать и
перегнать Memgraph" is done: 1.4–5.8× ahead through the identical client on
every measured workload.

Honesty notes: this is one 2.6 k-node production KG snapshot, not a
benchmark suite — bigger graphs, deeper traversals, and write throughput
remain unmeasured; Memgraph runs its stock configuration; and the count
rows now measure a cardinality lookup, which is precisely the point of a
pushdown but says nothing further about scan speed (the arena/CSR work
remains worthwhile for the scan-shaped queries the detector must not
touch).

## Run 8 — the durable serving path (2026-09-01, same machine & snapshot)

Runs 6–7 measured `DREVO_ENGINE=native` — the in-memory read *mirror* beside
a redb store of record. This run measures `DREVO_ENGINE=native-durable`: the
**WAL-backed native engine as the store of record**, no redb at all
(`NativeService` — crash-recovery log, the full index stack synced off the
change-feed, runtime compaction, registered transactions). The graph was
restored into a fresh zero-redb server through `POST /import/graphml` (the
71 MB backup landed in **0.65 s**, one fsynced WAL batch — the live-migration
path end to end), then queried over Bolt with the same client as runs 5–7.

| Workload | drevo native-durable (Bolt) | Memgraph (Bolt) | drevo advantage |
|---|---:|---:|---:|
| `RETURN 1` (round-trip floor) | 259 µs | 612 µs | 2.4× |
| `MATCH (n) RETURN count(*)` | **240 µs** | 693 µs | **2.9×** |
| label scan (densest label) | **304 µs** | 683 µs | **2.2×** |
| property equality (no label) | **244 µs** | 1.66 ms | **6.8×** |
| property equality (labelled) | **478 µs** | 588 µs | **1.2×** |
| Cypher 1-hop from hub (id seek) | **317 µs** | 596 µs | **1.9×** |

**Durability is free on reads.** The WAL-backed engine matches the in-memory
mirror of run 7 within noise (count 240 µs vs 234 µs; hub 317 µs vs 283 µs)
and still wins every row against Memgraph, 1.2–6.8×. The whole durability
track — crash-recovery WAL, the in-statement index-staleness gate, runtime
compaction, the change-feed-fed index stack, registered transactions — added
no measurable read cost: reads serve the same in-memory native structures
either way; the WAL is on the write path only. This closes the evidence loop
on Phase 4/7: the store that persists to disk performs like the one that
does not.

## Standing items toward "surpass Memgraph" (RFC Phase 0)

- ~~Add the Memgraph column~~ — run 5 above; rerun via
  `scripts/memgraph_baseline_bench.py` (needs the `memgraph/memgraph` image
  and a running `drevo-server`; see the script docstring).
- ~~Engine flip (`DREVO_ENGINE=native|kv`)~~ — shipped (Phase 6 slices A/B)
  and measured in run 6: four of six rows now beat Memgraph through the
  same Bolt client.
- ~~The count-scan gap~~ — closed by the count pushdown (run 7); every
  scoreboard row is now drevo's.
- Arena/CSR native internals (RFC Phase 2 completion) — still worthwhile
  for the *scan-shaped* queries the count detector must not touch
  (filtered scans, projections, aggregations over properties); re-run this
  scoreboard after it lands (append runs, keep history). The **pre-refactor
  anchor** it must beat is recorded below.

## Phase 2 pre-refactor anchor — arena/CSR before/after (2026-09-01)

RFC Phase 2 replaces the native engine's `HashMap` vertices/edges plus the
denormalised `Vec<AdjEntry>` adjacency with an arena/slot + CSR
representation. Before that refactor lands, this is the in-process native
baseline it must not regress — and should improve on the traversal rows —
captured on the same real-data snapshot as runs 1–8 (2 596 nodes, 3 755
edges; densest label `Entity` = 1 971 nodes; mid-selectivity property
`type = 'Trait'`; hub node out-degree 98). Apple M1 Max, criterion
midpoints, native in-process (no Bolt) with the label + property +
value-cache indexes synced.

| Workload | native (pre-Phase-2) | KV (redb) | note |
|---|---:|---:|---|
| `count(*)` all nodes | **192 ns** | 10.4 µs | cardinality pushdown |
| label scan count (`Entity`, 1 971) | **57.4 µs** | 303 ms | scan-shaped — Phase 2 target |
| property equality count (`type='Trait'`) | **9.26 µs** | 302 ms | scan-shaped — Phase 2 target |
| 1-hop from hub, Cypher | **60.4 µs** | 14.3 ms | executor + adjacency |
| 1-hop from hub, seam (`neighbor_ids`) | **4.35 µs** | 13.9 µs | adjacency only |
| **2-hop from hub, seam (frontier expand)** | **36.7 µs** | 99.0 µs | **arena/CSR anchor** |

The 2-hop seam row is the sensitive measurement and the reason it was added:
98 first-hop nodes, each expanded through its own adjacency slice (~8× the
1-hop cost), so it exposes the per-edge iteration a CSR rewrite reshapes
rather than a single lookup a HashMap already serves well. Reproduce
exactly with

```sh
DREVO_BASELINE_GRAPHML=$HOME/drevo_backups/drevo_kg_20260805_195240.graphml \
    cargo bench --bench real_data_baseline_bench
```

This is a documentary anchor, not an enforced CI gate: criterion timings
are machine-specific, and gating on them is the trap that produced the
multi-hour CI stall (bench-on-push, #76/#77). Phase 2's before/after is a
manual re-run of this exact command on the same snapshot and machine. The
part that *is* mechanically enforced is the bench's built-in parity
assertion — both engines must return identical rows (including the new
2-hop count) before any timing is taken — so a wrong-answer speedup can
never be recorded as a win.
