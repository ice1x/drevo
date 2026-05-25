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

## Local build

`drevo-py` is intentionally **not** in `default-members` of the
workspace — the existing CI (`.github/workflows/ci.yml`) does not
provision a Python interpreter, and PyO3 requires one at build time. To
compile this crate locally:

```bash
# Compile the cdylib (requires Python ≥ 3.10 on PATH)
cargo build -p drevo-py

# Run rust-level unit tests (type conversions, error mapping)
cargo test -p drevo-py
```

A `maturin develop` workflow that builds the wheel and installs it into
the current virtualenv lands in task `00116`.

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
