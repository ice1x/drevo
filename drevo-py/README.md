# drevo-py

PyO3 bindings for the [drevo](https://github.com/ice1x/drevo) embedded
graph database — Phase 16 task `00115`.

This crate implements the contract in
[`audit/RFC-python-api.md`](../audit/RFC-python-api.md): a frozen, typed,
GIL-releasing surface that mirrors the public Rust API of the
[`drevo`](../) crate.

## Status

| Phase 16 task | Status     | Notes                                                         |
|---------------|------------|---------------------------------------------------------------|
| `00114` RFC   | ✅ shipped | `audit/RFC-python-api.md`                                     |
| `00115` core  | ✅ shipped | this crate — `Drevo` handle, CRUD, traversal, FTS, errors    |
| `00116` wheels| ⏳ pending | `pyproject.toml`, `maturin`, type stubs, `cibuildwheel`      |
| `00117` rag   | ⏳ pending | pure-Python `drevo.rag.{Retriever, Context, MMRReranker}`    |
| `00118` unit  | ⏳ pending | `tests/unit/` (~80 tests against the PyO3 surface)            |
| `00119` integ | ⏳ pending | `tests/integration/` (real redb tempfile)                     |
| `00120` e2e   | ⏳ pending | five scenario domains + RAG scenario                          |
| `00121` MCP   | ⏳ pending | KG-backed symbol introspection                                |
| `00122` CI    | ⏳ pending | `.github/workflows/python.yml` (3.10 × {linux, mac, windows}) |

## Install (after task `00116`)

```bash
# From a published wheel (lands once a PyPI release task ships).
pip install drevo-py

# From source — works today against this repo. Requires Python ≥ 3.10
# plus a Rust toolchain (`rustup`) so maturin can compile the cdylib.
pip install maturin
pip install .                  # builds + installs the wheel
# OR for an editable dev install:
maturin develop --release
```

After install:

```python
import drevo

with drevo.Drevo.open_in_memory() as db:
    node = db.create_node(drevo.NewNode(kind="note", title="hello"))
    print(node.uuid)            # uuid.UUID, not bytes
```

## Examples

These snippets are a curated, self-contained usage corpus. Keep each block
runnable — they double as copy-paste starters and as fixtures for the doc-example
tests. (Historically they were also indexed by an in-tree `python_api_examples`
MCP tool; the MCP server now lives in a separate repository —
[github.com/ice1x/drevo-mcp](https://github.com/ice1x/drevo-mcp).)

### Create and read a node

```python
import drevo

with drevo.Drevo.open_in_memory() as db:
    node = db.create_node(drevo.NewNode(kind="task", title="Write tests"))
    fetched = db.get_node(node.id)
    assert fetched == node
```

### Connect two nodes with an edge

```python
with drevo.Drevo.open_in_memory() as db:
    a = db.create_node(drevo.NewNode(kind="task", title="Design"))
    b = db.create_node(drevo.NewNode(kind="task", title="Implement"))
    edge = db.create_edge(
        drevo.NewEdge(from_id=a.id, to_id=b.id, kind="blocks")
    )
```

### Traverse the graph (BFS)

```python
with drevo.Drevo.open(path) as db:
    reachable = db.bfs(
        start_id=root.id,
        max_depth=3,
        direction=drevo.Direction.OUT,
    )
    for node in reachable:
        print(node.title)
```

### Full-text search over node titles

```python
with drevo.Drevo.open(path) as db:
    hits = db.search_fts("authentication bug", limit=10)
    for hit in hits:
        print(hit.score, hit.node.title)
```

### Retrieve a graph-RAG context for an LLM prompt

```python
from drevo.rag import Retriever

with drevo.Drevo.open(path) as db:
    retriever = Retriever(db, hops=2, max_nodes=50)
    context = retriever.retrieve("onboarding checklist", limit=5)
    prompt = context.to_text(format="markdown")
```

### Vector search over stored embeddings

```python
from drevo.rag import vector_search

with drevo.Drevo.open(path) as db:
    hits = vector_search(db, query=my_embedding, k=5)
    for hit in hits:
        print(hit.similarity, hit.node.title)
```

### Run Cypher

`Drevo.execute(query, params=None)` runs one Cypher statement in autocommit
mode: a write is durable when the call returns. Parameters are named (`$name`)
and passed as a `dict` (None, bool, int, float, str, `uuid.UUID`, lists and
str-keyed dicts). Rows come back as dicts. Graph values are `CypherNode`,
`CypherRelationship` and `CypherPath`, with `uuid.UUID` ids.

```python
import drevo

with drevo.Drevo.open_in_memory() as db:
    db.execute(
        "CREATE (:Person {name: $a})-[:KNOWS {since: 2020}]->(:Person {name: $b})",
        {"a": "Ada", "b": "Bo"},
    )
    result = db.execute(
        "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a.name AS a, r, b"
    )
    result.columns            # ['a', 'r', 'b']
    for row in result:
        print(row["a"], row["r"].type, row["b"].properties["name"])

    try:
        db.execute("RETURN $missing AS v")
    except drevo.ParameterMissingError as exc:   # a drevo.CypherError
        print(exc)
```

Errors: `CypherSyntaxError` (did not parse), `ParameterMissingError`,
`QueryTimeoutError` and any other executor error derive from
`drevo.CypherError`. Storage failures keep their usual classes, so a duplicate
title is a `DuplicateTitleError` whether it comes from Cypher or `create_node`.

### Transactions

`db.begin()` (or `db.transaction()` in a `with` block) groups Cypher
statements atomically. Statements read their own writes, nobody else sees them
until `commit()`, and the commit is one fsynced write. The `with` form commits
on a clean exit and rolls back on an exception.

```python
import drevo

with drevo.Drevo.open_in_memory() as db:
    with db.transaction() as tx:
        tx.execute("CREATE (:Account {id: 1, balance: 100})")
        tx.execute("CREATE (:Account {id: 2, balance: 0})")

    for attempt in range(3):                       # optimistic: retry on conflict
        try:
            with db.transaction() as tx:
                tx.execute("MATCH (a:Account {id: 1}) SET a.balance = a.balance - 30")
                tx.execute("MATCH (a:Account {id: 2}) SET a.balance = a.balance + 30")
            break
        except drevo.TransactionConflict:          # the graph changed since begin()
            continue
```

- Commit is **optimistic**. If any other write committed since `begin()`, the
  commit raises `TransactionConflict` (a `TransactionError`) and nothing is
  applied. Retry with a fresh transaction.
- A statement that fails inside the transaction **rolls it back** (as in
  Neo4j). Any later use of a closed transaction raises `TransactionError`.
- A violated declared constraint raises `ConstraintViolation` (a
  `ConflictError`).
- A transaction that is dropped without commit is rolled back. Closing the
  database does not wait for open transactions; using one afterwards raises
  `RuntimeError`.
- v1 scope: only `tx.execute` runs inside the transaction. The typed CRUD
  methods (`create_node`, …) stay autocommit on the handle.

### Two-phase commit

A transaction can take part in a distributed transaction run by an external
coordinator. `prepare(gid)` records it durably as *prepared*. It is then
resolved with `commit_prepared` or `rollback_prepared`, from any handle and
even after a restart:

```python
import drevo

with drevo.Drevo.open_in_memory() as db:
    tx = db.begin()
    tx.execute("CREATE (:Order {id: 42})")
    tx.prepare("order-42")            # phase one; the transaction is closed
    db.list_prepared()                # [PreparedTransaction(gid='order-42', op_count=1)]
    db.commit_prepared("order-42")    # phase two (or db.rollback_prepared)
```

While a transaction is prepared, every other write raises
`TransactionConflict` (retry once it is resolved); reads keep working. An
unknown gid raises `UnknownGidError`. An operator can unblock writes with
`db.heuristic_rollback_prepared(gid)`. The coordinator's late
`commit_prepared` then raises `HeuristicRollbackError`, instead of the
transaction silently vanishing. `drevo.dbapi` exposes the same through
PEP 249's TPC extension (`conn.xid(...)`, `tpc_begin`, `tpc_prepare`,
`tpc_commit`, `tpc_rollback`, `tpc_recover`). Design:
[RFC](../docs/rfc-two-phase-commit.md).

### DB-API 2.0 (PEP 249)

`drevo.dbapi` is a PEP 249 module with Cypher as the query language, for
tools and data layers that speak DB-API.

```python
import drevo.dbapi as dbapi

conn = dbapi.connect("/path/graph.drevo")      # or ":memory:", or a drevo.Drevo handle
cur = conn.cursor()
cur.execute("CREATE (:note {title: $t})", {"t": "x"})
cur.execute("MATCH (n:note) WHERE n.title = $t RETURN n.title", {"t": "x"})
cur.fetchall()        # [('x',)]
cur.description       # (('n.title', <class 'str'>, None, None, None, None, None),)
conn.commit()         # or conn.rollback()
conn.close()
```

- `apilevel = "2.0"`, `threadsafety = 1` (share the module, not connections),
  `paramstyle = "named"` using **Cypher's `$name`** syntax. PEP 249's `:name`
  would clash with `:Label`, so queries are passed through untouched. Dates
  and times are sent as ISO-8601 strings; binary values raise
  `NotSupportedError`.
- Each connection runs one implicit transaction on top of `Drevo.begin()`.
  `commit()` applies it, `rollback()` discards it, and `close()` rolls back
  pending work. `with dbapi.connect(...) as conn:` commits on a clean exit,
  rolls back on an exception, then closes. A failing statement rolls the
  transaction back.
- Errors follow the PEP 249 hierarchy:

  | DB-API error | Raised for |
  |---|---|
  | `ProgrammingError` | Cypher syntax or semantic errors, a missing parameter, misuse |
  | `IntegrityError` | duplicate title, constraint violation |
  | `OperationalError` | commit conflict (retry), statement timeout, storage |
  | `DataError` | value out of range |
  | `InterfaceError` | closed connection or cursor |

### Migrating from Neo4j

Importing an existing Neo4j graph is **not** part of `drevo-py` — the
database bindings know nothing about Neo4j. That lives in a separate,
one-way-dependent tool, [`neo4j-to-drevo`](../tools/neo4j-to-drevo/),
which depends on `drevo` and reads either an APOC JSON dump or a live
Bolt connection. See its README for the dump → load workflow.

## Local Rust build

`drevo-py` is intentionally **not** in `default-members` of the
workspace — the existing CI (`.github/workflows/ci.yml`) does not
provision a Python interpreter, and PyO3 requires one at build time. To
compile this crate at the Rust level (no Python install required for
the type-conversion / error-mapping tests):

```bash
# Compile the cdylib (requires Python ≥ 3.10 on PATH)
cargo build -p drevo-py

# Run rust-level unit tests (type conversions, error mapping)
cargo test -p drevo-py
```

The maturin wheel build (`maturin build` / `maturin develop`) lives
behind `drevo-py/pyproject.toml` (task `00116`); the cross-platform
`cibuildwheel` matrix runs on every PR via
`.github/workflows/python-wheels.yml`.

## Public surface (Rust-side)

* `errors` — `DrevoError` / `NotFoundError` / `NodeNotFoundError` /
  `EdgeNotFoundError` / `ConflictError` / `DuplicateTitleError` /
  `StorageError` / `SerializationError` / `LockedError` / `PanicError`
  classes, plus the `map_err` table.
* `types` — frozen `#[pyclass]` wrappers: `Node`, `Edge`, `NewNode`,
  `NewEdge`, `NodePatch`, `EdgePatch`, `ScoredNode`, `SubGraph`,
  `CompactReport`, and the `Direction` IntEnum.
* `handle::Drevo` — the database handle with `open`, `open_in_memory`,
  `close`, `__enter__` / `__exit__`, `compact`, `health_check`, full
  node + edge CRUD, `list_*_by_kind`, `list_recent`, `bfs`, `dfs`,
  `shortest_path`, `subgraph`, `neighbors`, and `search_fts`.

## Out of scope for `00115`

The following pieces are tracked under follow-on Phase 16 tasks and are
intentionally **not** included here:

* `pyproject.toml`, `maturin` build backend, `cibuildwheel` matrix
  (task `00116`).
* Pure-Python `drevo/__init__.py` shim that imports `_drevo` and
  wraps `bytes` UUIDs as `uuid.UUID` (task `00116`).
* `drevo.rag` graph-RAG idioms layer (task `00117`).
* Python unit / integration / e2e test suites (tasks `00118` / `00119` /
  `00120`).
* Batch APIs (`create_nodes` / `create_edges`) — require a transactional
  batch entry point on the Rust side, tracked separately under Phase 16.
