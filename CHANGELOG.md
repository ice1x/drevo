# Changelog

All notable changes to drevo are recorded here. This complements the git
history and the in-repo issue tracker; format loosely follows
[Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added

- **`IS NOT NULL`, composite `IS UNIQUE` and `IS NODE KEY` constraints**
  (issue #606), in the Neo4j 5 `FOR … REQUIRE` and Neo4j 4 `ON … ASSERT`
  spellings (incl. `exists(n.p)`):
  - Uniqueness of a property tuple is checked on every write; presence at the
    end of the statement and at transaction commit. While a presence rule
    exists, each writing statement runs as one transaction, so
    `MERGE … ON CREATE SET` can fill a required property and a violating
    statement writes nothing.
  - `SHOW CONSTRAINTS` reports types `NODE_PROPERTY_EXISTENCE` and `NODE_KEY`
    and accepts multi-word filters (`SHOW NODE KEY CONSTRAINTS`).
  - drevo-core: `Constraint::UniqueNodeProperties`;
    `NativeGraph::has_existence_constraints`. `PropertyExists` and `NodeKey`
    now cover secondary labels and the `title`/`body` fields, treat `null` as
    missing, and are checked on writes outside a transaction too; a `NodeKey`
    duplicate is refused by the write itself.

- **UNIQUE constraints** (issue #604):
  - `CREATE CONSTRAINT [name] [IF NOT EXISTS] FOR (n:Label) REQUIRE n.prop IS
    UNIQUE` (and Neo4j 4's `ON … ASSERT …`), `DROP CONSTRAINT`, and
    `SHOW CONSTRAINTS` listing them.
  - Enforced on every node write through a value index.
  - A violation surfaces as Bolt `Neo.ClientError.Schema.ConstraintValidationFailed`,
    HTTP 409 with that `code`, and Python `ConstraintViolation`.
  - Definitions persist next to the WAL. Other constraint forms stay
    accepted no-ops.

### Fixed

- **Explicit transactions while a two-phase commit is prepared** (issue #608):
  - A transaction that only read now commits while a transaction is prepared
    (before, its `COMMIT` failed with the retryable "writes are paused"
    error). In drevo-core, a commit with an empty write set always succeeds
    and never reports a conflict.
  - `drevo.tx.commitPrepared` / `rollbackPrepared` / `heuristicRollback` /
    `listPrepared` inside an explicit transaction fail with
    `Neo.ClientError.Transaction.ForbiddenDueToTransactionType` and a message
    saying to call them in auto-commit mode, instead of an unknown error that
    blamed the storage engine.

### Changed

- **Node titles are no longer unique** (issue #595). Any number of nodes may
  share a title, as in Neo4j and Memgraph:
  - Cypher `CREATE` / `SET`, the HTTP API, the Python SDK, batch creates and
    imports no longer reject a repeat. HTTP `POST /nodes` no longer answers 409.
  - `get_node_by_title` returns the oldest node with the title; the new
    `nodes_by_title` returns all of them.
  - `DuplicateTitle` / `DuplicateTitleError` stay in the API so existing
    matches compile, but node writes no longer produce them.

### Added

- **CSR adjacency snapshot** (`drevo_core::csr::CsrAdjacency`, issue #382,
  Phase 8): a Compressed-Sparse-Row view of a `GraphSnapshot`'s out-adjacency
  (`GraphSnapshot::csr_out`) and in-adjacency (`GraphSnapshot::csr_in`) — a
  cache-friendly, contiguous layout with dense vertex indices, built off the
  immutable MVCC snapshot. The substrate for whole-graph parallel algorithms
  (#408, #412).
- **Morsel-driven parallel scan** (`CsrAdjacency::par_map`, issue #382): runs a
  per-vertex closure across worker threads over disjoint output slices — no
  locking, no contention, no new dependency (`std::thread::scope`). The
  parallel-execution primitive the analytics can fan out over (#411).

### Removed

- **Duplicate CSR PageRank and weakly-connected-components** that had been added
  to `drevo_core::csr` in #409 and #410. **This was my (the assistant's)
  mistake:** I built second implementations of PageRank and WCC over the CSR
  layout without first checking that the crate already ships a complete graph
  analytics suite in `src/algorithms/` — `pagerank` (serial, `pagerank_parallel`,
  and `pagerank_native` over the native MVCC snapshot), `louvain`, `wcc`, `scc`,
  `triangles`, `betweenness`, `closeness` — the canonical, more capable versions
  (weighted edges, tolerance-based convergence, configurable damping), exposed as
  `CALL drevo.*` procedures. The csr.rs copies were weaker (unweighted,
  fixed-iteration), unused by anything, and pure duplication, so they are
  removed. Lesson recorded: grep `src/algorithms/` before implementing a graph
  algorithm; #382's actual intent is to give the *existing* algorithms a CSR
  layout for a real parallel speedup (as the `pagerank_native` doc comment
  already noted), not to reimplement them. The CSR layout and `par_map` above
  remain — those are the genuinely new, on-target contribution.
