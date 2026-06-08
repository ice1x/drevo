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

These snippets are the curated usage corpus surfaced by the
`python_api_examples` MCP tool (task `00121`): the `drevo-mcp` server
embeds this README at build time and fuzzy-searches these blocks by
intent, so an AI client can answer "how do I …?" without leaving the
conversation. Keep each block self-contained and runnable.

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
* `Drevo.query(cypher, params=)` — gated on Phase 10 task `00063`
  landing the Cypher executor.
