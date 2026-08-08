# FTS storage benchmark (#275)

Measures the on-disk-size vs. write-throughput trade-off of the FTS index across
drevo versions, on the **real agent-memory KG** (2596 nodes / 3755 edges,
`~/drevo_backups/*.graphml`). Harness: [`bench.py`](bench.py) — version-independent
(stable drevo-py API only), so the same script runs against any build.

## Why this exists

The #275 posting-list rewrite optimized storage. This benchmark exists to prove
it did **not** silently regress write throughput — and it caught one that it
had (see "posting-lists" below), which the batched-writes fix then removed.

## Versions

| label | git | FTS layout |
|---|---|---|
| `per-pair` | `e47e9268` (pre-#278) | one empty row per `(trigram, node)` |
| `posting-lists` | `c25e5d51` (#278+#279) | `fts:{trigram}:` → packed `[id]`, but written **one `put` per trigram** |
| `posting-lists-batched` | this branch | same layout, all posting writes folded into **one `put_batch`** |

## Results (real KG, macOS, slow-fsync host)

| metric | per-pair | posting-lists | posting-lists-batched |
|---|---:|---:|---:|
| **file size** | 514.5 MiB | **257.5 MiB** | **257.5 MiB** |
| **import (disk)** | 15.6 s | 175.3 s ⚠️ | **6.1 s** |
| import throughput | 166 nodes/s | 14.8 nodes/s | **426 nodes/s** |
| batch write (in-mem) | 9 834 n/s | 17 210 n/s | **18 022 n/s** |
| incr write, 1-by-1 (in-mem) | **9 960 n/s** | 3 723 n/s | 3 890 n/s |
| search median | 412 ms | 445 ms | 430 ms |

## What the numbers say

- **Storage: halved.** 514.5 → 257.5 MiB on a fresh import; the posting-list
  layout collapses ~1.6M `(trigram,node)` rows to ~one-per-trigram. (On the
  live 412 MB file — already more compact than a fresh per-pair rebuild — a
  `shrink` lands at 258 MiB, i.e. **−37%**.)

- **A real regression, found and fixed.** `posting-lists` regressed disk import
  **11×** (15.6 → 175.3 s): `index_nodes_grouped` issued one `put` — hence one
  redb commit/fsync — **per trigram**, thousands on a bulk import, versus the
  old path's single `put_batch`. In-memory benchmarks hid it (no fsync); only
  the disk import exposed it. **`posting-lists-batched`** folds every updated
  posting list into one `put_batch`: import drops to **6.1 s — 2.5× faster than
  the old per-pair format** (fewer rows + one commit), file size unchanged.

- **Batch writes: faster.** `create_nodes` / import in-memory throughput is up
  (~18 k vs ~9.8 k nodes/s) — one grouped RMW per trigram beats one write per
  `(trigram,node)` pair.

- **The one honest cost: single-node, one-at-a-time writes** are ~2.5× slower
  in-memory (3 890 vs 9 960 n/s). This is the fundamental posting-list
  read-modify-write cost (each insert touches a growing list; worst case here,
  all nodes share vocabulary). It is an **in-memory micro-benchmark worst case**:
  on disk a single `create_node` is dominated by fsync, not this; and real bulk
  paths (`create_nodes`, import) are *faster*. Net: no regression on any
  realistic path.

- **Search: unchanged** (~430 ms median; a single `get` per trigram replaced a
  prefix scan — neutral-to-slightly-better at this scale).

## Reproduce

```bash
# build a version's drevo-py into the env, then:
python bench/fts_storage/bench.py \
    --label <name> \
    --graphml ~/drevo_backups/<kg>.graphml \
    --workdir /tmp/drevo_bench
```

Numbers are host-relative (this machine has slow redb fsync); the **ratios**
between versions are the signal, not the absolute seconds.
