# Load / throughput harness

> First slice of [#241]. A repeatable driver that runs a **mixed read/write
> workload** over the public `Drevo` API across a **concurrency sweep** and
> reports **p50/p95/p99** latency and **throughput**. It is a measurement tool,
> not a `cargo test` — it runs on demand and is kept off `ci-fast`.

## Running it

```text
cargo run --release --example load_harness
```

Knobs (environment variables):

| Var | Default | Meaning |
|---|---|---|
| `NODES` | `5000` | seed nodes the workload reads/links against |
| `OPS` | `2000` | operations **per thread** |
| `READ_PCT` | `80` | percent of ops that are reads (rest are edge writes) |
| `BACKEND` | `memory` | `memory` (in-memory) or `redb` (on-disk) — see below |

```text
NODES=20000 OPS=5000 READ_PCT=50 cargo run --release --example load_harness
BACKEND=redb NODES=1000 OPS=500 cargo run --release --example load_harness
```

The thread sweep is fixed at `[1, 2, 4, 8, 16]`. A **fresh graph is seeded per
sweep point** so points are comparable (writes don't accumulate as the thread
count grows). stdout is a JSON array of sweep points (one per thread count);
stderr is a compact human table.

**`BACKEND=redb`** runs the identical sweep against the on-disk redb backend
instead of the ephemeral in-memory one. Every `create_edge` then commits with an
fsync, so writes are far slower and the single-writer ceiling shows in real
wall-clock — this is where the concurrency curve reflects redb's actual
copy-on-write cost rather than just lock contention. Because each write fsyncs,
**use small `NODES`/`OPS`** (the default 5000×2000 would take a long time on
disk). It is an on-demand measurement, never run in CI.

Read op = `get_node` + 1-hop `neighbors` (`Outgoing`). Write op = `create_edge`
between two seed nodes. The op mix and per-thread access stream are deterministic
(exactly `READ_PCT` reads per 100 ops), so runs are reproducible.

## What each sweep point contains

```json
{
  "threads": 8, "ops_per_thread": 2000, "read_pct": 80,
  "total_ops": 16000, "errors": 0, "wall_ms": 40,
  "throughput_ops_sec": 395417.0,
  "reads":  { "count": 12800, "min_us": 0, "max_us": 852, "mean_us": 24,
              "p50_us": 3, "p95_us": 60, "p99_us": 95 },
  "writes": { "count": 3200,  "min_us": 3, "max_us": 563, "mean_us": 52,
              "p50_us": 36, "p95_us": 152, "p99_us": 196 }
}
```

## Baseline (main, in-memory backend)

Captured with the defaults (`NODES=5000 OPS=2000 READ_PCT=80`). **Absolute
numbers are hardware-specific** — regenerate locally with the command above; the
signal is the *shape* across the sweep, not the raw figures.

| threads | ops/sec | read p50 µs | read p99 µs | write p50 µs | write p99 µs |
|--:|--:|--:|--:|--:|--:|
| 1 | 636 816 | 0 | 1 | 3 | 6 |
| 2 | 608 620 | 1 | 6 | 5 | 31 |
| 4 | 522 872 | 1 | 46 | 8 | 129 |
| 8 | 395 417 | 3 | 95 | 36 | 196 |
| 16 | 393 496 | 6 | 179 | 82 | 441 |

### Reading the baseline

- **The single-writer ceiling is visible.** Throughput *falls* as concurrency
  rises (636k → 393k ops/sec) rather than scaling up: the write path serializes,
  so adding threads adds contention, not throughput. This is the redb / in-memory
  single-writer model showing through.
- **Write tail latency grows with contention** — write p99 climbs 6 µs → 441 µs
  across the sweep, while reads stay comparatively cheap (they don't hold the
  writer). This is the quantitative companion to the adjacency-layout analysis in
  [`adjacency-key-schema.md`](adjacency-key-schema.md) and [#243].

## Churn → compact → recovery (`churn_compact` example)

The second harness measures the degradation the [#240] adjacency-layout
investigation predicts — a COW B-tree file holds its high-water mark and
scatters live pages across the freelist under churn — and how much
`Drevo::compact()` recovers. Unlike the in-memory sweep above, it runs against
the **redb (on-disk) backend** so the file footprint and compaction are real.

```text
cargo run --release --example churn_compact
NODES=20000 EDGES=40000 CHURN=40000 PROBE=20000 cargo run --release --example churn_compact
```

| Var | Default | Meaning |
|---|---|---|
| `NODES` | `10000` | seed nodes |
| `EDGES` | `20000` | seed edges (so `neighbors` returns something) |
| `CHURN` | `20000` | churn rounds — grow-then-shrink: insert this many edges (rewriting node bodies), then delete them all |
| `PROBE` | `10000` | read-probe ops per phase (`get_node` + 1-hop `neighbors`) |

It runs a single-threaded read probe in three phases — **steady** → (churn) →
**degraded** → (`compact()`) → **recovered** — and prints JSON with each phase's
throughput / p50–p99 / on-disk `file_bytes`, plus the `CompactReport`
(`bytes_before` / `bytes_after` / `bytes_reclaimed`).

> **On-disk writes are fsync-bound and slow on the shared self-hosted runner**
> (each redb transaction fsyncs; ~thousands of churn writes take minutes). The
> full-scale run is a local/on-demand measurement. The flow itself is covered at
> small scale by the `#[ignore]`d `redb_three_phase_churn_compact_recovers` in
> `tests/compaction_tests.rs`, which runs in `slow-tests.yml` (never on the PR
> gate). The compaction *reclaim* contract is separately locked by
> `redb_compact_reclaims_after_heavy_churn`.
>
> **Reclaim only manifests at scale.** redb pre-allocates a file region and
> recycles freed pages from its freelist, so a small run stays inside that region
> and reports `bytes_reclaimed: 0` with a flat `file_bytes` across phases — the
> grow-then-shrink churn has to push the file past the pre-allocated region
> before `compact()` returns space to the OS. Use a large `CHURN` (and enough
> `NODES`/`EDGES`) to see non-zero reclaim; the existing
> `redb_compact_reclaims_after_heavy_churn` documents the same caveat.

## HTTP load path (`http_load` example)

The sweeps above call the in-process API directly. `http_load` instead starts
the drevo HTTP API on an ephemeral localhost port and drives it with a minimal,
dependency-free HTTP/1.1 client, so the numbers include the **full request
path** — TCP connect, HTTP framing, axum routing, JSON (de)serialisation.

```text
cargo run --release --example http_load
NODES=5000 OPS=2000 READ_PCT=80 cargo run --release --example http_load
```

| Var | Default | Meaning |
|---|---|---|
| `NODES` | `2000` | seed nodes |
| `OPS` | `500` | requests **per thread** |
| `READ_PCT` | `80` | percent reads (`GET /nodes/{id}`); the rest are writes (`POST /edges`) |

Same fixed thread sweep `[1, 2, 4, 8, 16]`; stdout is a JSON array (per-thread
`requests_per_sec` + read/write p50–p99), stderr a table.

### Sample (in-memory backend, localhost)

Illustrative, hardware-specific — regenerate with the command above.

| threads | req/sec | read p99 µs | write p99 µs |
|--:|--:|--:|--:|
| 1 | 8 305 | 250 | 262 |
| 4 | 18 602 | 340 | 329 |
| 16 | 20 406 | 1 082 | 1 382 |

Unlike the in-process write ceiling, HTTP **RPS scales up** with concurrency
(8.3k → 20k) before plateauing: reads dominate (80 %) and axum/tokio serve them
across a worker pool, while the serialized writes are only a fifth of the load.
Latency still climbs under contention (read p99 250 µs → ~1 ms).

The over-the-wire flow is covered on the normal PR gate by
`http_load_end_to_end_small_scale` in `tests/http_load_tests.rs` (localhost +
in-memory is fast); the status-line parser has its own unit test. The full RPS
sweep is on-demand.

---

All four planned slices of [#241] have landed: the in-memory concurrency sweep,
the redb backend variant, the churn → compact → recovery harness, and this HTTP
load path.

[#240]: https://github.com/ice1x/drevo/issues/240
[#241]: https://github.com/ice1x/drevo/issues/241
[#243]: https://github.com/ice1x/drevo/issues/243
