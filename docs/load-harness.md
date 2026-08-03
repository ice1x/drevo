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

```text
NODES=20000 OPS=5000 READ_PCT=50 cargo run --release --example load_harness
```

The thread sweep is fixed at `[1, 2, 4, 8, 16]`. A **fresh in-memory graph is
seeded per sweep point** so points are comparable (writes don't accumulate as
the thread count grows). stdout is a JSON array of sweep points (one per thread
count); stderr is a compact human table.

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

## Deliberately deferred (follow-up PRs)

Kept out of this first slice to keep it reviewable:

- **churn → `compact()` → recovery curve** — measure steady-state, apply heavy
  delete/insert/update churn, re-measure (degraded), compact, re-measure. This is
  the degradation the [#240] investigation predicts.
- **redb (on-disk) backend variant** — the numbers above are the in-memory
  backend; the on-disk single-writer path has its own fsync/COW costs.
- **HTTP load path** — driving `POST /import` and the HTTP API for RPS/SLA figures.

[#240]: https://github.com/ice1x/drevo/issues/240
[#241]: https://github.com/ice1x/drevo/issues/241
[#243]: https://github.com/ice1x/drevo/issues/243
