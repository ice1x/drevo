# Native engine — load & concurrency

The [native-core baseline](native-core-baseline.md) is a single-threaded
micro-latency scoreboard: it says nothing about **concurrency**, **deep
traversal**, or **write throughput**. This page fills those gaps with
`examples/native_load.rs`, which drives a *shared* engine from a thread sweep
and reports throughput + tail latency (p50/p95/p99) per workload, for the
`native-durable` engine (real on-disk WAL) against the KV `Drevo`, both loaded
from the same real-data GraphML snapshot through the identical `GraphEngine`
seam.

Reproduce:

```text
DREVO_BASELINE_GRAPHML=$HOME/drevo_backups/<snapshot>.graphml \
    cargo run --release --example native_load
# tunables: THREADS=1,2,4,8,16  OPS=2000  HOPS=3  WRITE_OPS=500
```

## Results (Apple M1 Max, 2 596-node / 3 755-edge snapshot)

Throughput in ops/sec; latency in µs. `native` = `native-durable`.

| Workload | Threads | KV ops/s | native ops/s | KV p99 | native p99 |
|---|---:|---:|---:|---:|---:|
| point_read | 1 | 8.7 k | **6.24 M** | 243 | **0** |
| point_read | 8 | 56 k | **3.14 M** | 618 | **6** |
| one_hop | 1 | 254 k | **734 k** | 13 | **4** |
| one_hop | 8 | 988 k | **2.34 M** | 21 | **7** |
| three_hop_bfs | 1 | 27 k | **77 k** | 157 | **49** |
| three_hop_bfs | 4 | 68 k | **113 k** | 243 | **159** |
| three_hop_bfs | 8 | 57 k | 64 k | 672 | 655 |
| **write_edge autocommit** | 1 | **4.5 k** | 187 | **337** | 8 875 |
| **write_edge autocommit** | 8 | **8.1 k** | 136 | 12 396 | **737 205** |
| **write_edge tx-batched** | 1 | — | **172 563** | — | — |

## What it shows — honestly

**Reads: native wins decisively and scales.** Point reads are ~100–700×
faster (a RAM hashmap lookup vs a redb page walk) and even the 3-hop
breadth-first traversal — where the index-free adjacency thesis is supposed
to pay off — is ~2–3× ahead at low concurrency. This is the real,
non-latent win the durability flip banked.

**Deep traversal narrows under high concurrency.** At 8 threads the 3-hop
advantage collapses to ~1.1× (64 k vs 57 k). The BFS op allocates a fresh
`HashSet`/frontier per call, so at high thread counts the bottleneck moves
from the engine to the allocator, not the adjacency structure — a harness
artefact worth knowing before reading too much into the top row.

**Writes are the real weakness, and load testing is the only thing that
showed it.** The KV engine sustains 4.5–8 k single-edge inserts/sec;
`native-durable` autocommit does **~130–190/sec** — *30–60× slower* — with a
p99 that degrades from ~9 ms at one thread to **~0.7 s at eight**. Cause: the
durable store does a full `fsync` **per acknowledged write**, and concurrent
writers serialize behind the single WAL writer, so tail latency explodes
under write contention. Reads are untouched (the WAL is off the read path);
this is purely the write path.

## Production implication + the fix (measured)

The live `native-durable` deployment is excellent for a read-heavy graph
(which this KG is) but the autocommit write path is **not** suited to
high-concurrency write bursts as-is. The fix already exists in the engine —
**batch writes into a transaction** (`NativeTx`), which commits a whole batch
with a *single* `fsync` — and the harness now **measures** it rather than
asserting it: the same durable edge inserts run at

* **autocommit: ~174 inserts/sec** (one `fsync` each), versus
* **tx-batched: ~172 600 inserts/sec** (one `fsync` at commit) —

a **~1 000× speedup**, which also puts native's batched write throughput
~20× *above* the KV engine's ~8 k/sec. So the fix is real, not just plausible.
Write-heavy callers should still wrap bulk inserts in a transaction rather
than firing autocommit statements — that is the ~1 000× path.

### Group commit for autocommit (landed, measured)

Autocommit itself no longer fsyncs once per edge under load: concurrent
durable writers enqueue and **one elected leader flushes the whole pending
batch with a single fsync** (`GroupCommit` in `drevo-core`; a batch of N
writes counts as one, exposed as `NativeGraph::wal_fsync_count`). Measured
effect on the concurrent autocommit workload (8 threads):

* **p99 tail: ~737 ms → ~27 ms (~27× better)** — the catastrophic tail above
  is gone, which is the point for many concurrent MCP writers;
* **throughput: ~136 → ~595 inserts/sec (~4×)**, and it now *scales* with
  threads instead of staying flat.

It does **not** reach the tx-batched or KV numbers, because autocommit is now
bounded by the **inner write lock** (each statement's in-memory apply is
serialized), not by fsync. Coalescing is proven deterministically by
`group_commit_coalesces_concurrent_fsyncs` (N concurrent writes ⇒ fsyncs ≪ N,
all recover); an uncontended write still fsyncs once
(`sequential_durable_writes_fsync_once_each`), so durability is unchanged.
Lifting the inner-write-lock serialization is the remaining follow-up if
autocommit write throughput must rise further; bulk writers should batch.

The pieces this harness depends on — the BFS reach, KV/native traversal
parity, error-free concurrent reads, durable writes surviving a WAL reopen,
group-commit fsync coalescing, and single-fsync transactions — are guarded on
the normal PR gate by `tests/native_load_tests.rs`.
