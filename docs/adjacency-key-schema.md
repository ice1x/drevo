# Adjacency key schema (redb / KV backend)

> Investigation for [#240]. Documents how
> drevo encodes graph data as keys over its KV backend, and assesses the layout for
> traversal cost, type-filtering, supernode behavior, and churn. Grounded in
> `src/db.rs` (key builders around L2714–2856, expansion at `outgoing_edges`
> L2413). No code change — measurement and follow-up planning only.

## Storage substrate

The redb backend (`src/storage/redb.rs`) exposes **two** physical tables:

- `data` — `&[u8] → &[u8]`, holds every logical record and index entry
- `meta` — `&str → &[u8]`, counters (`meta:next_node_id`, …), `format_version`

All graph structure lives in `data`, namespaced by a **string key prefix**. redb is a
copy-on-write B-tree, so `data` is one ordered keyspace and any contiguous prefix is a
range scan. The backend contract (`StorageBackend`) is `get` / `put` / `delete` /
`scan_prefix(prefix) -> Vec<(key, value)>`.

## Key layout

`id` values are `u64` **little-endian**, fixed 8 bytes (`LE8`). `uuid` is 16 raw bytes.

| Purpose | Key | Value |
|---|---|---|
| Node record | `node:` + `id:LE8` | serialized `Node` (title, body, kind, uuid, props, timestamps) |
| Edge record | `edge:` + `id:LE8` | serialized `Edge` (**from_id, to_id, kind**, uuid, props, timestamps) |
| **Out adjacency** | `out:` + `from_id:LE8` + `:` + `edge_id:LE8` | `to_id:LE8` + `kind` (denormalized, #243)¹ |
| **In adjacency** | `in:` + `to_id:LE8` + `:` + `edge_id:LE8` | `from_id:LE8` + `kind` (denormalized, #243)¹ |
| Node UUID index | `node_uuid:` + `uuid:16` | `node_id:LE8` |
| Node title index | `node_title:` + `title` | `node_id:LE8` |
| Node kind index | `node_kind:` + `kind` + `:` + `node_id:LE8` | empty |
| Edge UUID index | `edge_uuid:` + `uuid:16` | `edge_id:LE8` |
| Edge kind index | `edge_kind:` + `kind` + `:` + `edge_id:LE8` | empty |
| Updated index | `updated:` + `ts` + `:` + `id` | empty |

¹ **#243 slice 1 (landed).** The adjacency value stores the *other* endpoint's
node id (`LE8`) + the edge `kind`, so "who is adjacent to X" reads straight from
the scan with **zero** `get_edge`. A migrated value is always ≥ 8 bytes, so it is
unambiguously distinct from a legacy **empty** value (databases written before
#243). Readers decode the value and fall back to `get_edge` only for legacy
empties; [`Drevo::backfill_adjacency_values`] upgrades them in place. This is a
**value-only** change — the key format is untouched, so no `format_version` bump.

Neighbor expansion now splits into two paths. The **id/kind-only** fan-out
(`neighbor_ids`, and `neighbors` built on it) never loads an edge record:

```rust
// src/db.rs — adjacency_targets_prefixed (feeds neighbor_ids)
let entries = self.backend.scan_prefix(&prefix)?;   // 1 range scan → ALL out-edge keys
for (key, value) in entries {
    match decode_adjacency_value(&value) {
        Some((neighbor_id, kind)) => { /* straight from the value — 0 get_edge */ }
        None => { /* legacy empty value → one get_edge fallback */ }
    }
}
```

The **full-edge** path (`edges_of`, used by weighted traversal / Dijkstra, which
still needs `weight` + props) keeps the 1-scan-+-N-`get_edge` shape:

```rust
// src/db.rs — outgoing_edges
let entries = self.backend.scan_prefix(&prefix)?;   // 1 range scan → ALL out-edge keys
for (key, _) in entries {
    let edge_id = edge_id_from_adjacency_key(&key, &prefix);
    if let Some(edge) = self.get_edge(edge_id)? {   // N point lookups (one get per edge)
        edges.push(edge);
    }
}
```

## Assessment

### What the layout gets right

- **Direction-partitioned** (`out:` vs `in:`): a traversal expands only the needed
  direction, never reads the other. (Q2)
- **Per-node contiguity**: all of a node's edges share the `out:{id}:` / `in:{id}:`
  prefix → one B-tree descent + a **sequential range scan**, not scattered lookups. (Q1)
- **Symmetric reverse index**: in-edges have their own mirrored `in:` prefix, so
  `<-[:T]-` is as cheap to *locate* as `->`. (Q5)
- **Clean delete path**: deleting an edge removes its `edge:`, `out:`, `in:`, and
  `edge_kind:` keys (db.rs:840–844); updates delete-then-reput. redb is a COW B-tree,
  so there are **no tombstones** — no deleted-edge read-amp, unlike an LSM backend. (Q6/Q7)

### Gaps (supernode / type-filter unfriendly)

1. ~~**Empty adjacency value → 1 + N point lookups.**~~ **Fixed — #243 slice 1.** The
   `out:`/`in:` value now denormalizes `neighbor_id + kind`, so `neighbor_ids` (and
   `neighbors`) recover "who are X's neighbors" and do in-memory `kind` filtering from
   the scan alone — **0 `get_edge`**. The full-edge path (`edges_of`, for weighted
   traversal that needs `weight`/props) still pays 1 + N, which is inherent to loading
   whole edges. Legacy (pre-#243) databases keep working via a `get_edge` fallback until
   `backfill_adjacency_values()` upgrades them.

2. **`edge_kind` is not in the adjacency key → no type slicing.** (Q3) `(X)-[:KNOWS]->()`
   cannot scan only the KNOWS slice; it scans **all** out-edges of X, loads each edge
   record, and filters `kind` in memory. The global `edge_kind:{kind}:{edge_id}` index
   does **not** help here — it is not scoped by `from_id`, so using it would require an
   intersection with `out:{X}:`.

3. **`scan_prefix` returns a full `Vec` → no pagination / streaming.** (Q4) The backend
   contract materializes the entire neighbor set into a `Vec<(Vec,Vec)>`, then
   `outgoing_edges` allocates a second `Vec<Edge>` and loads every edge. A supernode with
   millions of edges materializes millions of keys **plus** millions of edge loads in RAM
   — even if the caller wants only the first K, or a `kind`-filtered subset.

**Supernode verdict:** scan-*correct* but not supernode-*friendly* — unbounded
materialization, no type slice, and a mandatory extra read per edge. The first thing that
hurts on a hot hub node.

**Churn verdict:** deletes/updates are clean (no tombstones; freed pages go to the redb
freelist; adjacency + kind index keys are all removed/rewritten, so no index drift from
the adjacency side). Physical locality still degrades under churn per the general
COW-B-tree story (file holds its high-water mark; neighbor pages scatter across the
freelist) and is restored by `compact()` — the exact degradation [#241] is meant to
measure.

### Subtle notes

- **`id` is little-endian**, so the *global* order of `out:` keys is not `from_id`-numeric
  and, within a node, edges come back in LE-byte order, not numeric `edge_id` order.
  Irrelevant to a single-node scan (the `from_id` is fully specified), but callers that
  need numeric order sort explicitly (e.g. the deterministic dump).
- The `:` separator between the two fixed-width `LE8` ids is redundant (lengths are fixed)
  but harmless; `out_prefix` includes the trailing `:` so a scan is unambiguous.

## Recommendations (feed [#241], then a follow-up)

Ordered cheapest-first; validate against [#241]'s numbers before any format migration:

1. ~~**Denormalize `to_id` (+ optionally `kind`) into the adjacency _value_.**~~ **Done —
   #243 slice 1.** Value-only change, no key-format migration. Kills the 1 + N for pure
   neighbor reads and in-memory `kind` filtering via `neighbor_ids` / `neighbors`; legacy
   entries fall back to `get_edge` until `backfill_adjacency_values()` runs.
2. **Put `kind` in the adjacency _key_** (`out:{from}:{kind}:{edge_id}` or `…:{to}:…`)
   for true type-sliced sub-prefix scans. Key-format migration — heavier. Still open
   (#243 slice 2), gated on the supernode/type-filter numbers from [#241].
3. **Streaming / bounded scan API** (iterator or `scan_prefix_limited(prefix, start,
   limit)`) so supernode expansion doesn't materialize the whole neighbor set. Still open
   (#243 slice 3), format-independent and low-risk.

Options 1 and 3 are independent of the format and low-risk; option 2 should wait on the
supernode numbers from [#241].

[#240]: https://github.com/ice1x/drevo/issues/240
[#241]: https://github.com/ice1x/drevo/issues/241
[#243]: https://github.com/ice1x/drevo/issues/243
