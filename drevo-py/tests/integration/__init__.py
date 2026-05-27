"""Phase 16 task `00119` — Python integration-test suite for drevo-py.

Per `audit/RFC-python-api.md` §2 the package's test tree is layered:

    drevo-py/tests/
        ├── unit/         # 00118 — focused, mocked-where-possible cases
        ├── integration/  # 00119 — real redb backend, cross-component  ← here
        └── e2e/          # 00120 — five scenarios + graph-RAG scenario

This tier exists to catch invariants the unit suite cannot:

* **Durability** — open → write → close → reopen → verify. The
  in-memory backend used by 00118 has no on-disk form, so persistence
  bugs (allocator state, kind index rebuild, FTS posting lists) only
  surface here.
* **Index pagination** — `list_*` calls take `(limit, offset)`. The
  boundary cases (`offset == len`, `offset > len`, `limit == 0`, full
  scan reassembled by paging) need enough rows to make pagination
  observable; unit fixtures stay too small.
* **FTS recall over a real corpus** — tokeniser, posting list, TF-IDF
  ranking, and the public `search_fts(query, limit)` contract together.
  Unit tests pin individual edges; integration tests pin the
  *behaviour an embedding-augmented agent depends on*.
* **Traversal over real edge tables** — BFS / DFS / shortest_path /
  subgraph driven from the real redb edge index, not synthetic
  in-memory adjacency. Cycles, fan-out, and edge-kind filters are
  asserted against the persisted store.
* **GIL release under contention** — `Drevo` methods drop the GIL
  around storage I/O. Concurrent Python threads sharing one handle
  must serialise correctly *and* allow Python-side progress on a
  parallel thread (RFC §4.2).

Cypher round-trip coverage (originally listed in the 00119 brief
"once 00063 executor lands") is deferred: the 00063 executor exists at
the Rust level but is not yet wired into the PyO3 surface in `handle.rs`.
A separate Phase 16 follow-up will add `Drevo.cypher(query, params)` and
the corresponding integration tests live with that change.
"""
