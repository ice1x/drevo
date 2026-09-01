# drevo-core

The storage-agnostic core of [drevo](https://github.com/ice1x/drevo): the
domain model, the storage-agnostic `CoreError`, and the **native in-memory
graph engine** with its label, property, BM25 full-text, and value-cache
indexes — everything the engine needs, with none of the KV store, HTTP,
Bolt, or Python bindings the main `drevo` crate layers on top.

It is extracted so the engine can be depended on directly by other projects
without pulling in the server surface. The main `drevo` crate consumes this
crate by path and re-exports it, so `drevo::model::…`, `drevo::native::…`,
and friends keep resolving unchanged.

## What's inside

- `model` — nodes, edges, properties, directions, the graph domain types.
- `engine::GraphEngine` — the trait the Cypher executor runs against.
- `native::NativeGraph` — the in-memory engine implementing it (HashMap
  vertices/edges with denormalised, index-free adjacency; an arena/CSR
  representation is the planned Phase 2 of
  [RFC #307](https://github.com/ice1x/drevo/blob/main/docs/rfc-native-core.md)).
- `native_label_index`, `native_property_index`, `native_fts`, `bm25`,
  `value_encoding`, `tokenizer` — the secondary indexes and their support.
- `error::CoreError` — a `thiserror` enum that converts structurally to and
  from the main crate's `DrevoError`.

## Design notes

- **Dependency-light on purpose.** The only external crates are `serde` /
  `serde_json` / `bincode` for (de)serialization, `thiserror` for the error
  type, and `uuid` for identifiers.
- **`wasm32`-clean.** A `wasm` feature forwards `uuid`'s getrandom backend
  and swaps the clock source, so a `wasm32-unknown-unknown` build of the
  model keeps working (mirrors the main crate).

## Status

Pre-1.0. The public API tracks the main crate's needs and may change between
`0.x` releases. Performance characteristics and the scoreboard against
Memgraph are recorded in
[`docs/native-core-baseline.md`](https://github.com/ice1x/drevo/blob/main/docs/native-core-baseline.md).

## License

Dual-licensed under **MIT OR Apache-2.0**; downstream consumers may pick
either, with no obligation to comply with both at once. Full texts:
[`LICENSE-MIT`](https://github.com/ice1x/drevo/blob/main/drevo-core/LICENSE-MIT)
and
[`LICENSE`](https://github.com/ice1x/drevo/blob/main/drevo-core/LICENSE)
(Apache-2.0).
