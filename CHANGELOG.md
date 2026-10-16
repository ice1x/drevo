# Changelog

All notable changes to drevo are recorded here. This complements the git
history and the in-repo issue tracker; format loosely follows
[Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added

- **Graph labels at any zoom** (issue #626). In the Web UI graph, a node's
  size follows its number of connections on the canvas, and the 8 most
  connected nodes keep a readable label however far the view is zoomed out.
  Clicking a node labels it and its 8 most connected neighbours the same
  way until the background is clicked.

- **Colour nodes by a property** (issue #624). A **Colour by** select in the
  Web UI's left rail colours the graph by label (the default) or by a
  property such as `type` or `project`, with a legend of the values on the
  canvas. It offers string properties on at least 30% of the nodes whose 12
  most common values cover at least 60% of the nodes that have them. The
  choice is remembered per browser.

- **Readable inspector** (issue #622). The node and relationship inspector
  shows strings without quotes, `null` dimmed and every date as
  `YYYY-MM-DD HH:MM:SS UTC` (epoch numbers under time-like keys and ISO 8601
  strings alike, raw value in the tooltip). Text over 300 characters is
  clipped behind **Show more**, and a relationship's **from** / **to** are
  node links that open that node's neighbourhood.

- **Web UI label filter** (issue #620). The label chips show the 12 most
  common labels (plus the selected one) with **Show all N labels**, and a
  **Filter labels** box shows every label containing the typed text. On a
  window narrower than 1200px the top bar hides the server version to keep
  room for the query editor.

- **Ask in plain language** (issue #616). The Web UI's **Ask** button sends
  the editor's text to `drevo.cypher.fromText` and puts the generated Cypher
  in the editor to review; it never runs it. `GET /` reports
  `"text2cypher": {"model": …}` when an upstream is configured (`null`
  otherwise), and the button explains the setup until then.

- **Web UI query editor** (issue #615). The query bar takes several lines and
  grows with the text. Enter runs a one-line query, Shift+Enter adds a line,
  Ctrl/Cmd+Enter runs any query. Alt+↑ / Alt+↓ step through the last 50
  queries, which the ▾ button also lists; the history is kept in the browser.

- **Web UI result views** (issue #613). A Cypher result opens in the main area
  under **Graph**, **Table** and **JSON** tabs instead of a table in the left
  rail. A result with nodes opens as a graph, otherwise as a table. Table cells
  show strings without quotes, `null` dimmed, numbers right-aligned and nodes
  as clickable labels; the header stays visible while scrolling, and
  **Copy CSV** copies the rows.

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

- **Replica stamps survive a reopen** (issue #612). A durable store keeps
  each live node's and edge's causal stamp in `stamps.jsonl` next to the WAL,
  so after a restart its version vector is intact:
  - `delta_since(&VersionVector::new())` still carries every live record, so
    a new peer can bootstrap from a replica that restarted since its last write.
  - A local edit keeps its stamp, so a peer's older copy cannot overwrite it
    after the restart, and a peer's newer edit still wins.
  - A record without a saved stamp (a store from an earlier version, or a
    crash before the stamp reached disk) reopens with the lowest stamp: it
    is still sent to a new peer but loses to any real write.
  - The log is appended per stamped write without `fsync` and rewritten to one
    line per live record on open, on `compact_wal` and once it grows large.

- **Search, vector and schema statements inside explicit transactions**
  (issue #610). Inside Bolt `BEGIN` … `COMMIT` (the Neo4j drivers'
  `execute_read` / `execute_write`) and a Python `Transaction`:
  - Full-text, vector and fuzzy search, `keywords()`, `drevo.semantic.*`
    queries and status now work. Before, they failed with an error blaming a
    "KV storage engine". Hits resolve against the transaction's snapshot.
  - `SHOW INDEXES` / `SHOW CONSTRAINTS` list the schema (they were empty).
  - Nodes and relationships created in the transaction are auto-embedded.
  - Full-text search after the transaction (or the same statement) wrote fails
    with a message saying the index covers committed data only (Bolt
    `Neo.ClientError.Transaction.ForbiddenDueToTransactionType`).
  - Schema commands (`CREATE` / `DROP INDEX`, `CREATE` / `DROP CONSTRAINT`,
    `drevo.semantic.register*` / `reindex*`) take effect at once and cannot be
    rolled back, so they fail with that code and say to use auto-commit mode.
  - A query run without the server's services now says which one is missing
    instead of naming the removed KV engine.
  - drevo-core: `NativeGraph::tx_has_writes`.

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
