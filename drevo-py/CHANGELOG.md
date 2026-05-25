# Changelog

All notable changes to `drevo-py` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Tracked here as Phase 16 tasks land. Sections roll into the next
released entry on a tagged commit.

### Pending

- `00117` — pure-Python `drevo.rag` graph-RAG idioms layer
  (`Retriever`, `Context`, `MMRReranker`, `ingest_documents`).
- `00118` — Python unit-test suite under `tests/unit/`.
- `00119` — Python integration-test suite under `tests/integration/`.
- `00120` — Python e2e graph-RAG suite under `tests/e2e/`.
- `00121` — MCP introspection generator (Python symbols mirrored into
  the project knowledge graph for `drevo-mcp` clients).
- `00122` — Python CI matrix (`.github/workflows/python.yml`) — `cp310`
  through `cp313` × `{ubuntu, macos, windows}-latest`, gating PR merges.
- Cypher executor wrapper (`Drevo.query(text, params=)`) — unlocked
  once Phase 10 task `00063` ships.

## [0.1.0] — 2026-05-26

Initial alpha release. Covers Phase 16 tasks `00114`–`00116`. Wheel
build is exercised in CI on every PR via
`.github/workflows/python-wheels.yml`; no PyPI publishing yet (gated
behind a separate release task once `00122` makes the Python CI
matrix mandatory).

### Added — task `00114` (RFC)

- `audit/RFC-python-api.md` — accepted contract for the Python surface,
  cited by every Phase 16 implementation task. Twelve sections covering
  naming, type mapping, sync-vs-async, error mapping, iterator-vs-list,
  batch APIs, `drevo.rag` idioms, comparison to `neo4j` / `kuzu` /
  `falkordb` / `redis-py` drivers, ten open questions with default
  positions, definition-of-done, and an amendments block.

### Added — task `00115` (PyO3 bindings core surface)

- New `drevo-py` workspace member with `[lib] name = "_drevo"` (the
  underscore-prefixed Python extension module behind the user-facing
  `import drevo`).
- Typed exception hierarchy rooted at `drevo.DrevoError` —
  `NotFoundError`, `NodeNotFoundError`, `EdgeNotFoundError`,
  `ConflictError`, `DuplicateTitleError`, `StorageError`,
  `SerializationError`, `LockedError`, `PanicError` — with a `map_err`
  table covering every `drevo::error::DrevoError` variant.
- Frozen `#[pyclass]` wrappers for `Node`, `Edge`, `NewNode`,
  `NewEdge`, `NodePatch`, `EdgePatch`, `ScoredNode`, `SubGraph`,
  `CompactReport`, plus the `Direction` `IntEnum` (`OUT` / `IN` /
  `BOTH`).
- `Drevo` handle wired through `Arc<Mutex<Option<...>>>` so `close()`
  can consume by value and `compact()` can take `&mut self`. Methods:
  `open` / `open_in_memory` / `close` / `__enter__` / `__exit__` /
  `compact` / `health_check`; full node + edge CRUD; `edges_of`,
  `list_nodes_by_kind`, `list_edges_by_kind`, `list_recent`; `bfs`,
  `dfs`, `shortest_path`, `subgraph`, `neighbors`; `search_fts`.
- `py.allow_threads(...)` on every storage I/O call (RFC §4.2).
- `std::panic::catch_unwind` wrapper on every `#[pymethods]` body so a
  Rust panic surfaces as `drevo.PanicError` instead of aborting the
  process (RFC §5.4).
- 9 text-level scaffolding tests in
  `tests/python_api_scaffolding_tests.rs` locking the contract.

### Added — task `00116` (package skeleton + wheels)

- `drevo-py/pyproject.toml` — PEP 621 metadata, `maturin>=1.7,<2.0`
  build backend, `module-name = "drevo._drevo"`, `python-source =
  "python"`, classifiers covering CPython 3.10 / 3.11 / 3.12 / 3.13 on
  Linux / macOS / Windows.
- `drevo-py/python/drevo/__init__.py` — pure-Python shim re-exporting
  every public class from `_drevo`, wrapping `Node.uuid` /
  `Edge.uuid` 16-byte `bytes` as native `uuid.UUID` instances
  (RFC §12.2 amendment).
- `drevo-py/python/drevo/errors.py` — pure-Python `InvalidWeightError`
  subclass of `ValueError` (RFC §12.3 amendment).
- `drevo-py/python/drevo/__init__.pyi` — hand-authored type stubs for
  every public symbol; `mypy --strict drevo/` clean.
- `drevo-py/python/drevo/py.typed` — PEP 561 marker telling downstream
  type checkers to honour the stubs.
- `drevo-py/LICENSE` — dual MIT / Apache-2.0.
- `drevo-py/CHANGELOG.md` — this file.
- `.github/workflows/python-wheels.yml` — `cibuildwheel` matrix
  building wheels for `cp310`/`cp311`/`cp312`/`cp313` on Ubuntu, macOS
  (universal2), and Windows. Each job runs `twine check dist/*` to
  validate wheel metadata. **No PyPI publishing** — release uploads
  are gated on a follow-up task that requires `00122` (Python CI
  matrix) to be the mandatory branch-protection check first.
- 14 text-level scaffolding tests in
  `tests/python_package_wheels_tests.rs` locking every deliverable
  above so a future PR cannot silently drop a file or break the
  cibuildwheel matrix.

### Out of scope (deferred to follow-on Phase 16 tasks)

- Pure-Python `drevo.rag` graph-RAG idioms layer (`00117`).
- Python test suites — `tests/unit/` (`00118`), `tests/integration/`
  (`00119`), `tests/e2e/` (`00120`).
- MCP introspection generator (`00121`).
- Python CI matrix as a mandatory branch-protection check (`00122`).
- PyPI publishing — separate release task once the matrix is green.
- Batch APIs (`create_nodes` / `create_edges`) — require a transactional
  batch entry point on the Rust side.
- `Drevo.query(cypher, params=)` — gated on Phase 10 `00063` shipping
  the Cypher executor.

[Unreleased]: https://github.com/ice1x/drevo/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ice1x/drevo/releases/tag/v0.1.0
