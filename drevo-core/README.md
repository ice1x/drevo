# drevo-core

An embeddable property-graph engine for Rust: nodes and edges with JSON
properties, held in memory, with optional write-ahead-log durability, ACID
transactions, and BM25 full-text, label and property indexes.

It is the engine underneath [drevo](https://github.com/ice1x/drevo), which adds
Cypher, the HTTP and Bolt servers, and the Python bindings on top. Use
`drevo-core` on its own when you want the graph inside your process and nothing
else.

## Example

```rust
use drevo_core::engine::GraphEngine;
use drevo_core::model::{Direction, NewEdge, NewNode, Properties};
use drevo_core::native::NativeGraph;

fn task(title: &str) -> NewNode {
    NewNode {
        kind: "task".into(),
        title: title.into(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

let graph = NativeGraph::open_durable("tasks.wal")?; // or NativeGraph::new()
let deploy = graph.create_node(task("Deploy release"))?;
let review = graph.create_node(task("Review release notes"))?;
graph.create_edge(NewEdge {
    from_id: deploy.id,
    to_id: review.id,
    kind: "blocked_by".into(),
    weight: 1.0,
    properties: Properties::default(),
})?;

let blockers = graph.neighbors(deploy.id, Direction::Outgoing, Some("blocked_by"))?;
assert_eq!(blockers[0].title, "Review release notes");
```

## What's inside

- `native::NativeGraph` — the engine: CRUD on nodes and edges, traversal,
  O(1) snapshots, transactions (`begin` / `commit`) and two-phase commit,
  durable mode via `open_durable`.
- `engine::GraphEngine` — the trait the operations are written against;
  implemented by the graph and by each open transaction.
- `model` — `Node`, `Edge`, and their create/patch inputs.
- `native_fts` (BM25 full-text search), `native_label_index`,
  `native_property_index` — secondary indexes that follow the graph through its
  change feed.
- `csr` — compressed-sparse-row adjacency for whole-graph parallel scans.
- `replica`, `delta`, `hlc`, `lww` — WAL-shipping read replicas and
  multi-writer merge (hybrid logical clocks, last-writer-wins CRDTs).
- `dump` — the whole graph as one serde value, for backup and transfer.

## Design notes

- **Few dependencies.** `serde` / `serde_json` / `bincode` for
  (de)serialization, `thiserror` for the error type, `uuid` for identifiers.
- **Builds for `wasm32-unknown-unknown`.** The clock comes from the browser
  there; enable the `wasm` feature so uuid generation gets its random source.

## Status

Pre-1.0: the API may change between `0.x` releases. Benchmarks are in
[`docs/native-core-baseline.md`](https://github.com/ice1x/drevo/blob/main/docs/native-core-baseline.md).

## License

Dual-licensed under **MIT OR Apache-2.0**; downstream consumers may pick
either, with no obligation to comply with both at once. Full texts:
[`LICENSE-MIT`](https://github.com/ice1x/drevo/blob/main/drevo-core/LICENSE-MIT)
and
[`LICENSE`](https://github.com/ice1x/drevo/blob/main/drevo-core/LICENSE)
(Apache-2.0).
