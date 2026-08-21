# RFC: native graph core (`drevo-core`)

> Status: **draft / planning**. No code yet — this is the umbrella design for
> replacing the KV-encoded graph with a native, in-memory-first, ACID graph
> engine, extracted into a standalone Rust crate that drevo depends on.
> Tracking issue: _TBD_.

## 1. Motivation

drevo today is a **graph-on-KV** database: every node, edge, adjacency entry and
index is a serialized byte blob keyed by a string prefix (`node:`,
`out:{from}:{kind}:{edge_id}`, …) in a single redb table (see
[adjacency-key-schema.md](adjacency-key-schema.md)). A 1-hop expansion costs a
B-tree descent + range scan + key/value decode **per hop**. This is exactly the
architecture that native graph engines (Neo4j, Memgraph) beat with **index-free
adjacency**: a vertex holds direct references to its edges, so a hop is a memory
read, not an index lookup.

**Goal:** build a native property-graph engine in Rust that matches and then
surpasses Memgraph/Neo4j on traversal throughput while keeping drevo's
differentiators (embeddable, no GC, small footprint, WASM, built-in FTS +
vectors + auto-embedding), and extract it into a reusable crate `drevo-core`.

**Hard requirements (explicit):**

- **ACID from phase 1**, not bolted on later. Durability is part of the core.
- **Scalability** as a first-class concern — vertically (parallel runtime,
  concurrent writers) and horizontally (WAL-shipping read replicas → Raft HA).

## 2. What we take from Neo4j — and where we go further

| Neo4j core benefit | Neo4j reality | drevo today | native core target | Phase |
|---|---|---|---|---|
| Index-free adjacency | linked-list fixed-size records, cache-hostile on supernodes | KV prefix scan (the tax we remove) | arena + kind-sorted adjacency vectors: expand = binary-search + slice, cache-friendly | 2 |
| ACID + write-ahead log | ✅ but **default isolation is read-committed on locks** | undo-journal, per-connection tx (#298) | **MVCC snapshot isolation by default** + WAL — stricter and lock-free reads | 3–4 |
| Constraints / schema | ✅ unique / exists / node-key | title-uniqueness only | UNIQUE(label,prop) / EXISTS / node-key validated at commit via write-set (no locks) | 3 |
| Own page cache | ✅ (disk-era design) | redb COW B-tree | memory-first; reads never touch a page-fault path | 2/4 |
| Cost-based planner + stats | ✅; **parallel runtime is Enterprise-only** | planner substrate written (Phase 14) but not wired | native per-kind counters + degree histograms → wire the planner | 5–6 |
| Parallel query runtime | 💰 Enterprise | none | **morsel-driven parallelism**, free over an MVCC snapshot | 8 |
| Indexes (range/composite/text/vector) | ✅ + Lucene FTS | property index, BM25 FTS, HNSW vectors, **auto-embedding** (Neo4j has none) | keep on a change-feed; add composite index | 5 |
| Replication / HA | 💰 causal cluster | substrate (00095/00097) | WAL-shipping replicas → Raft (openraft) | 9 |
| No GC pauses, small image, embeddable, WASM | ❌ JVM (~GB heap, slow start) | ✅ (118 MB image, <1s start, lib + FFI + PyO3 + WASM) | **invariant — must not regress** | all |

Where we are *already* more modern than Neo4j: no JVM/GC, embeddable as a
library (not a server), Bolt-compatible (all Neo4j drivers work), built-in vector
+ FTS + auto-embedding, and a 118 MB image vs ~GB.

## 3. Key architectural decision — raise the seam from KV to graph

The current abstraction boundary is `StorageBackend` (`get/put/delete/scan_prefix`
over opaque bytes). A native engine is **not** a KV store, so it cannot implement
`StorageBackend` without re-imposing byte-key encoding — the exact tax we remove.

The new seam is one level up: a **`GraphEngine`** trait expressed in graph terms
(`create_node`, `neighbors(id, dir, kind, cursor)`, `edges_of`, `scan_kind`,
`begin/commit/rollback(tx)`, statistics), which today's `db.rs`+redb becomes the
first implementation of (`KvEngine`, behaviour-identical). This gives a
strangler-fig migration: two engines behind a flag, run against the **same** test
corpus (differential testing), flip the default when the native engine wins.

```
now:    Cypher executor → Drevo (key encoding) → StorageBackend (redb / memory)
target: Cypher executor → trait GraphEngine ─┬→ KvEngine   (current db.rs + redb, legacy)
                                             └→ drevo-core  (native graph, new crate)
```

## 4. Isolation model (ACID "I")

MVCC gives us a knob Neo4j cannot offer cheaply: the same version chain that
yields read-committed also yields snapshot isolation — the only difference is
*when the snapshot is taken* (per-transaction vs per-statement). So:

- **Default = snapshot isolation.** One query = one consistent slice of the
  graph. This matters specifically for AI/retrieval workloads: a multi-hop
  traversal under read-committed can observe *half* of another agent's in-flight
  mutation — a path that never existed — and silently feed a non-existent
  subgraph into an LLM context.
- **`READ COMMITTED` is a per-transaction opt-in** (re-snapshot each statement)
  for "freshness over a consistent slice" workloads (e.g. append-heavy KG
  enrichment). Weak isolation is a *mode chosen on top of MVCC*, never the
  architectural ceiling it is for a lock-based engine.
- Cost of SI: a long reader pins the GC horizon → version-chain growth. Mitigated
  by the horizon computation from #293 (`min(reader xmins, in-progress, next)`);
  the weaker mode is the escape valve for long analytical scans.

drevo's existing `src/mvcc/` (Phase 13: `VersionedStore`, snapshots, xmin/xmax
versioning, conflict detection, GC) is the substrate — currently **not wired into
`Drevo`** — and becomes the foundation, not a from-scratch build.

## 5. Scalability

### Vertical (single machine) — priority

- **Parallel runtime (Phase 8):** morsel-driven parallelism (DuckDB/HyPer style)
  — a query is split into morsels pulled by a worker pool. The MVCC snapshot makes
  read parallelism lock-free. Neo4j gates this behind Enterprise; we get it free
  over the snapshot.
- **Concurrent writers:** MVCC + write-write conflict detection (already written,
  task 00083) → true multi-threaded writes with `TransientError` retries, whose
  Bolt semantics already ship (#298). Neo4j serialises writers on locks.

### Horizontal (multi-node) — staged, each rung self-contained

1. **Read replicas via WAL-shipping (Phase 9a):** the phase-4 WAL is already a
   replication log; a follower replays it and serves read snapshots. Covers ~90%
   of real read-scaling needs. Substrate exists (`src/replication/`, CDC 00097).
2. **HA / auto-failover on Raft (Phase 9b):** `openraft`, WAL entries as the Raft
   log — Neo4j causal-cluster equivalent without an Enterprise licence.
3. **Sharding — explicit non-goal.** Graph min-cut is NP-hard; Neo4j Fabric is
   query federation, not transparent sharding. Deferred until proven necessary.

## 6. Phase plan (each phase = its own issues/PRs, strict TDD, merges green)

| Phase | Deliverable | Independently valuable? |
|---|---|---|
| **0** | Benchmark scoreboard: traversal/BFS/shortestPath/write-tps/recovery vs drevo-kv **and Memgraph** (docker, same Bolt client, on a **copy of real data** + synthetic supernode). Output `docs/native-core-baseline.md`. | Yes — defines "surpass" |
| **1** | Extract `trait GraphEngine`; today's code becomes `KvEngine`, behaviour-identical, whole existing test corpus is the guard. **Highest-risk phase** (mechanical refactor of 7k-line `db.rs`). | Yes — cleaner architecture |
| **2** | `drevo-core` crate: arena/slot vertices+edges, kind-sorted adjacency, interned labels. Single-threaded, in-memory. Differential-tested vs `KvEngine`. | Proves the traversal win |
| **3** | **ACID I+A+C:** MVCC snapshot isolation (port `src/mvcc/`), isolation knob, constraint engine (unique/exists/node-key) at commit. Adversarial concurrency tests (#298/#293 method). | Correctness core |
| **4** | **ACID D:** WAL + periodic snapshot + crash recovery. `kill -9` under load → reopen → invariants hold. | Durability |
| **5** | Native label/property indexes; FTS/vector/semantic fed via change-feed; add composite index. | Feature parity |
| **6** | Re-point Cypher executor / traversal / planner at native ops; `DREVO_ENGINE=native\|kv`, CI runs the full corpus on **both**; redb→native migration via existing GraphML cycle (#56/#57). | The latency win lands |
| **7** | Benchmark gate vs Memgraph on real data; flip default to native; publish `drevo-core` as a standalone crate drevo depends on. | The goal |
| **8** | Morsel-driven parallel runtime (parallel expand/scan, then PageRank over a CSR snapshot). | Vertical scale |
| **9** | WAL-shipping read replicas → Raft HA. | Horizontal scale |

Horizon: ~20–24 PRs. Any phase can be the stopping point — the KV engine stays
the default until phase 7, so nothing is broken mid-flight.

## 7. Risks

1. **In-memory ⇒ RAM-bound**, like Memgraph. drevo's real graph (~hundreds of
   MiB) fits comfortably, but "larger than RAM" stops being free (redb gave it).
   Accepted trade-off; KV engine remains the bigger-than-RAM option; an
   mmap/paged tier is a later possibility, not a phase-1 requirement.
2. **WASM invariant.** `wasm32` runs on `MemoryBackend` today. The native engine
   must stay WASM-buildable (no WAL file in the browser is actually simpler);
   verify from phase 2.
3. **Phase 1 is the danger.** Refactoring `db.rs` without behaviour change —
   mitigated by purely mechanical PRs and the existing test corpus as the guard.
4. **Scope.** This is a multi-quarter program, not one PR. Phase 0 (trait +
   scoreboard) is valuable even if the effort stops there.

## 8. Non-goals

- Transparent multi-node sharding (see §5).
- Replacing FTS/vector/semantic subsystems (they stay; only re-fed).
- Removing the KV engine (kept as the bigger-than-RAM / archival / WASM-simple
  option and as the differential-test oracle).
