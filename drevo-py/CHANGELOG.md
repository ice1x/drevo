# Changelog

All notable changes to `drevo-py` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Tracked here as Phase 16 tasks land. Sections roll into the next
released entry on a tagged commit.

### Added — task `00079` (embedding integration helpers — Phase 12)

- **`Drevo` vector bridge.** The PyO3 handle now exposes the durable
  `00078` vector store + `00076` HNSW search to Python:
  `set_embedding(node_id, list[float])`,
  `set_embeddings_batch(list[tuple[int, list[float]]])`,
  `get_embedding(node_id) -> list[float] | None`,
  `delete_embedding(node_id)`, `embedding_count() -> int`, and
  `vector_search(query, k) -> list[tuple[int, float]]` (nearest-first
  `(node_id, distance)`). Each releases the GIL and validates node
  existence; a dimension mismatch surfaces as `ValueError`.
- **`drevo.rag.embedding`** (new module) — pure-Python helpers on top of
  the bridge: the `Embedder` protocol, the `VectorHit` dataclass,
  `embed_and_store(drevo, nodes, embedder)` (one batched write), and
  `vector_search(drevo, query, *, embedder=None, k=10)` (raw vector or
  str query → ranked `VectorHit`s).
- **`Retriever.retrieve_with_embedding`** is implemented (was
  `NotImplementedError`): it seeds on the nearest vectors via
  `Drevo.vector_search` then expands the neighbourhood like `retrieve`.
- **`ingest_documents(embedder=...)`** now persists embeddings
  first-class via `set_embeddings_batch` in addition to the existing
  `"embedding"` property (which the `00077` Cypher `similar()` predicate
  reads), so an ingested corpus is queryable through both surfaces.

### Added — task `00122` (Python CI matrix on every PR — cibuildwheel pivot)

- `.github/workflows/python.yml` re-shaped as a `cibuildwheel`-driven
  matrix after a series of iterations exposed a fundamental GHA
  limitation. The original landing (3-cell `{python: [3.13]} × {os:
  [ubuntu-latest, macos-latest, windows-latest]}` on GitHub-hosted
  runners) hit the private-repo billing cap; the follow-up attempt
  to route the ubuntu cell through GHA `container: python:3.13-bookworm`
  on the self-hosted macOS host failed with `Container operations
  are only supported on Linux runners` (GHA's `container:` is
  implemented only in the Linux runner-agent regardless of Docker
  on the host).
- **Final shape: `{platform: [macos, linux]}` = 2 cells, both
  `runs-on: [self-hosted, macOS]`, zero GitHub-hosted billing.**
  cibuildwheel manages `docker run` itself for the Linux cell
  (manylinux container pulled and run on the same macOS host that
  serves the macos cell) — same pattern `python-wheels.yml` has
  used since the 2026-05-27 self-hosted revert.
- `CIBW_BUILD: "cp313-*"` — pinned per abi3-py310 per KG decision
  `decision_python_ci_matrix_pin_to_latest_only`. Scaffolding test
  negative-asserts `cp310-*` / `cp311-*` / `cp312-*` absence so a
  future PR can't silently reflate the build matrix.
- `CIBW_PLATFORM: ${{ matrix.platform }}` threads the matrix axis.
  Without it cibuildwheel only builds the host's native platform.
- `CIBW_TEST_REQUIRES: "pytest>=7 mypy ruff black"` — installs the
  five-gate tooling in cibuildwheel's isolated test env.
- `CIBW_TEST_COMMAND` chains the gates with `&&` so a failure
  surfaces in the cibuildwheel log:

      pytest -q {package}/tests/unit/      (00118)
      pytest -q {package}/tests/integration/  (00119)
      pytest -q {package}/tests/e2e/       (00120)
      mypy --strict {package}/python/drevo/
      ruff check {package}/python/ {package}/tests/
      black --check {package}/python/ {package}/tests/

  `{package}` resolves to the dir passed to cibuildwheel
  (`drevo-py/`).
- Linux container's in-container Rust install via
  `CIBW_BEFORE_ALL_LINUX` (rustup-init) with retry-friendly
  `CARGO_NET_RETRY=10 CARGO_HTTP_TIMEOUT=120` in
  `CIBW_ENVIRONMENT_LINUX` — defends against the kind of transient
  crates.io network blip that bit the `cargo install cargo-fuzz`
  fallback in ci.yml's Fuzz job on 2026-05-28.
- macOS cell preserves the `python-ci.yml`-style locator step that
  scans the python.org-pkg install paths and symlinks `python` /
  `python3` into `$GITHUB_WORKSPACE/.py-shim`; the persistent venv
  / persistent CARGO_TARGET_DIR optimisations from earlier
  iterations were dropped because cibuildwheel manages its own
  isolated env per wheel and adding them back as
  `CIBW_CONTAINER_ENGINE_PARAMS` volume-mounts is a follow-up.
- Windows is intentionally NOT in the matrix: cibuildwheel can't
  cross-build Windows wheels on a macOS host. That path stays in
  `python-wheels.yml` workflow_dispatch runs until a self-hosted
  Windows runner exists.
- `concurrency: python-matrix-${{ github.ref }}` with
  `cancel-in-progress: true` (distinct from `python-ci-` and
  `python-wheels-` groups); `fail-fast: false`; `timeout-minutes:
  30`; path filters narrowed to `drevo-py/**` + `src/**` +
  `Cargo.toml`/`Cargo.lock` + the workflow file.
- `tests/python_ci_matrix_tests.rs` — 16 text-level scaffolding
  cases for the cibuildwheel shape. Positive grep-asserts for
  cibuildwheel presence + `CIBW_BUILD` + `CIBW_PLATFORM` + every
  gate in `CIBW_TEST_COMMAND` + concurrency + no-PyPI-publish.
  Load-bearing **negative** assertions: no GHA `container:`
  directive at job-or-workflow level (prevents the broken regime
  from coming back); no `ubuntu-latest` / `windows-latest` in any
  `runs-on:` line (prevents accidental re-billing).
- The KG decision entities `decision_python_ci_macos_cell_on_self_hosted`,
  `decision_python_ci_macos_persistent_caches`, and
  `decision_python_ci_ubuntu_in_container_on_macos_host` from the
  earlier iterations are SUPERSEDED by
  `decision_python_ci_unified_via_cibuildwheel`. Earlier decisions
  remain in the KG as historical context with their relationships
  preserved.

### Added — task `00122` (Python CI matrix on every PR — original landing, superseded by cibuildwheel pivot above)

- `.github/workflows/python.yml` — the definition-of-done gate for
  Phase 16. Matrix as originally shipped: `{python: [3.13]} × {os: [ubuntu-latest,
  macos-latest, windows-latest]}` = 3 cells per PR.
- **macOS cell routed to self-hosted.** The macOS matrix cell runs
  on the project's persistent self-hosted macOS runner (labels
  `self-hosted, macOS`) via a ternary `runs-on:` expression —
  `${{ matrix.os == 'macos-latest' && fromJSON('["self-hosted", "macOS"]') || matrix.os }}`.
  Zero GitHub-hosted minutes for that cell. ubuntu-latest and
  windows-latest stay on GitHub-hosted because no self-hosted Linux
  / Windows runner is registered to this repo yet. The ternary
  expands cleanly when those runners come online — just extend the
  expression with additional OS-label branches.
- On the macOS cell `actions/setup-python@v5` is skipped (it cannot
  install into `/Users/runner/hostedtoolcache/` under the self-
  hosted runner's user) and replaced with a `Locate system Python
  3.13` step (mirrors `python-ci.yml`'s pattern): scans
  `/Library/Frameworks/Python.framework/Versions/3.13/bin/python3.13`
  + `$(command -v python3.13)` + `/opt/homebrew/bin/python3.13` +
  `/usr/local/bin/python3.13`, then symlinks the located interpreter
  as `python` / `python3` / `python3.13` inside
  `$GITHUB_WORKSPACE/.py-shim` and prepends that dir to
  `$GITHUB_PATH` so every subsequent venv / pip / maturin / pytest
  step resolves cleanly without per-step `if:` plumbing.
- Locked by `tests/python_ci_matrix_tests.rs::python_ci_matrix_macos_cell_runs_on_self_hosted`
  — positive assertions on the ternary expression + `fromJSON(["self-hosted", "macOS"])`
  + the locator step's existence. A future PR cannot silently drop
  the routing and put the macOS cell back on a billable runner.
- **Slimmed from the roadmap spec.** The original task text declared
  `python: [3.10, 3.11, 3.12, 3.13]` = 12 cells. At review time we
  pinned the Python axis to the latest interpreter only: drevo-py
  wheels ship `abi3-py310`, meaning one binary runs on every
  CPython 3.10+ without a recompile. Testing every minor multiplied
  CI spend (windows-latest cold-build is ~12 min × 4 versions per OS
  = ~50 hosted minutes wasted per PR) without meaningful coverage
  delta. The cross-OS signal (Windows locale-sensitive FTS UTF-8,
  redb file locking, macOS keychain) is preserved on the OS axis —
  it's the OS axis, not the Python axis, that delivers the actual
  platform-divergence coverage `00122` wanted. Locked by
  `tests/python_ci_matrix_tests.rs::python_ci_matrix_pins_python_to_latest_under_abi3`
  with positive (`"3.13"` present) + negative (`"3.10"`/`"3.11"`/`"3.12"`
  absent) assertions so a future PR cannot silently reflate the
  matrix without re-running this argument.
- Per cell: `actions/setup-python@v5` (with pip cache),
  `dtolnay/rust-toolchain@stable` + `Swatinem/rust-cache@v2` (matrix-
  cell-keyed so Linux/macOS/Windows caches stay separate and warm-
  path `maturin develop` drops from ~5 min cold to ~30 s on Linux /
  ~1 min on macOS+Windows), a venv-creation step that `python -m
  venv .venv` + exports `VIRTUAL_ENV` + prepends the venv `bin/`
  (or `Scripts/` on Windows under `shell: bash`) to `$GITHUB_PATH`
  (without it `maturin develop` errored with "Couldn't find a
  virtualenv or conda environment" in every cell — locked by
  `python_ci_matrix_creates_venv_before_maturin_develop`),
  `pip install maturin>=1.7,<2.0 pytest pytest-cov mypy ruff black`,
  `maturin develop --release` (the README spec's
  `--features="redb-backend"` was dropped — that feature lives on
  the root `drevo` crate's `default = […]` set, not on `drevo-py`,
  so passing it errored with "the package 'drevo-py' does not
  contain this feature: redb-backend"), then the five per-spec
  gates as DISCRETE Actions steps so the UI surfaces *which* layer
  regressed: `pytest tests/unit/` (00118), `pytest tests/integration/`
  (00119), `pytest tests/e2e/` (00120), `mypy --strict python/drevo/`,
  `ruff check + black --check`. A final `pytest --cov=drevo
  --cov-report=xml` runs per cell and uploads the report as a per-
  cell `coverage-${os}-py${python}` artefact (7-day retention).
- `coverage_comment` job — a second job on `ubuntu-latest`, triggered
  only on `pull_request`, pulls the canonical `coverage-ubuntu-latest-py3.13`
  artefact and posts a Markdown coverage summary to the PR via
  `actions/github-script@v7`. Vendor-neutral (no third-party action
  dependency); reads the XML produced by `pytest-cov` directly.
- Cost guards: `fail-fast: false` so the 3-cell signal stays
  diagnostic instead of collapsing to a single red mark;
  `concurrency: python-matrix-${{ github.ref }}` with
  `cancel-in-progress: true` (distinct from `python-ci-` and
  `python-wheels-` so the three workflows do not cancel each other on
  a shared key); `timeout-minutes: 30` per cell (Windows cold-build
  worst case during local probing); path filters narrowed to
  `drevo-py/**` + `src/**` + `Cargo.toml`/`Cargo.lock` + the workflow
  file itself so doc-only PRs do NOT pay the matrix cost.
- `tests/ci_self_hosted_runner_tests.rs` updated — the runner policy
  widens the macOS/Windows allow-list from `python-wheels.yml` only to
  the new `PYTHON_OS_MATRIX_WORKFLOWS = ["python-wheels.yml",
  "python.yml"]` const, with the same RFC §2 wheel-layout citation:
  PyO3 ABI checks are platform-native (no cross-compile path), FTS
  UTF-8 tokenisation is locale-sensitive on Windows, redb file locking
  is kernel-dependent — only running on real macOS / Windows runners
  gives meaningful regression signal for those classes. The test
  `macos_and_windows_runners_only_in_python_wheels_workflow` is
  renamed `..._python_matrix_workflows` and now asserts each
  `runs-on: macos-latest` / `windows-latest` literal lives in one of
  the allow-listed files.
- `tests/python_ci_matrix_tests.rs` — 19 text-level scaffolding cases
  that grep-lock the new workflow's shape end-to-end:
  - top-level `name:` includes `Python`;
  - triggers `pull_request: branches:[main]`, `push:`, and
    `workflow_dispatch:` are all present;
  - path filters cover `drevo-py/**`, `src/**`, and the workflow file
    itself;
  - the slim Python axis: positive `"3.13"` present + negative
    `"3.10"`/`"3.11"`/`"3.12"` absent assertions (locks the abi3-py310
    "test latest only" decision against silent expansion);
  - the venv-creation step exports `VIRTUAL_ENV=` and prepends to
    `$GITHUB_PATH` (locks the maturin venv-discovery fix);
  - every OS axis label (`ubuntu-latest`, `macos-latest`,
    `windows-latest`) enumerated;
  - `strategy: matrix:` block declared with `runs-on: ${{ matrix.os }}`
    threading the OS axis;
  - `maturin develop` with `--release`;
  - every tool from `pip install maturin pytest mypy ruff black`
    appears;
  - three DISCRETE `pytest tests/{unit,integration,e2e}/` invocations
    (not a single bulk `pytest tests/` call) so each layer surfaces
    its own pass/fail signal;
  - `mypy --strict`, `ruff check`, and `black --check` each named;
  - `pytest-cov` / `--cov` present;
  - `concurrency:` group named `python-matrix-…` with
    `cancel-in-progress: true`;
  - no `twine upload` / `pypi-publish` surface (the workflow is a GATE,
    not a release pipeline);
  - the runner policy file mentions `python.yml` (relaxation landed);
  - README ticks `[x] 00122` and includes a `Progress (YYYY-MM-DD,
    after task 00122)` note;
  - this CHANGELOG entry references `00122`.
- The new workflow does NOT replace `python-ci.yml`: that lightweight
  self-hosted single-cell gate stays as the fast (~1-2 min) feedback
  signal alongside the matrix; both run on every Python-touching PR.

### Added — task `00118` (Python unit-test suite)

- `drevo-py/tests/unit/` — focused, mocked-where-possible unit suite
  per RFC §2 ("Wheel layout" — the three-layer test tree). One
  assertion per case, dependencies (storage, embedder) stubbed at the
  smallest viable boundary (in-memory `Drevo` for storage,
  deterministic SHA-256 / one-hot embedders for vectors). 180+ runtime
  pytest cases organised by surface area:
  - `test_handle.py` — `Drevo` lifecycle (open / open_in_memory / close
    / context manager / compact / health_check).
  - `test_nodes.py` — full node CRUD + `list_nodes_by_kind`,
    `list_recent`, `get_node_by_*`, `update_node`, `delete_node`.
  - `test_edges.py` — full edge CRUD + `edges_of`, `list_edges_by_kind`,
    `InvalidWeightError` on non-finite weights.
  - `test_traversal.py` — `bfs` / `dfs` / `shortest_path` / `subgraph` /
    `neighbors` across the `connected_chain` fixture.
  - `test_fts.py` — `Drevo.search_fts` happy path + limit + no-match.
  - `test_errors.py` — every variant in RFC §5.1 + §5.3 inheritance
    chain (`NotFoundError`, `ConflictError`, `DuplicateTitleError`,
    `StorageError`, `SerializationError`, `LockedError`, `PanicError`,
    `InvalidWeightError`).
  - `test_rag_ingest.py` — `ingest_documents` schema + embedder edges.
  - `test_rag_neighborhood.py` — `expand_neighborhood` depth / kind
    filter / `max_nodes` / `hops_used` telemetry / seed-type union.
  - `test_rag_retriever.py` — `Retriever` dispatch (mocked) + real
    backend behaviour + `retrieve_with_embedding` deferral.
  - `test_rag_context.py` — all three `Context.to_text` formats
    (markdown / json / turtle); JSON validity; UUID textual length;
    Turtle quote escaping.
  - `test_rag_mmr.py` — MMR math at both `lambda_=1.0` (pure
    relevance) and `lambda_=0.0` (pure diversity) plus edge cases
    (`k=0`, `k>len`, empty input, embedder size mismatch).
- Shared fixtures in `drevo-py/tests/unit/conftest.py`: `drevo_db`,
  `mock_drevo` (typed `MagicMock`), `det_embedder` (hash-based),
  `orthogonal_embedder` (one-hot), `connected_chain` (a → b → c → d),
  `mixed_kind_neighbourhood` (root + one-of-each-kind), `make_scored`
  (duck-typed MMR candidate factory).
- 18 text-level scaffolding tests in
  `tests/python_unit_test_suite_tests.rs` locking the directory layout,
  the shared-fixture set, the per-surface test modules, the error
  hierarchy coverage, both MMR lambda semantics, and all three
  Context.to_text formats — runs inside the Rust-only CI gate so a
  rename / delete of a unit module is caught before the Python CI job
  fires.

### Added — task `00117` (graph-RAG idioms layer)

- `drevo-py/python/drevo/rag/` — pure-Python subpackage on top of the
  PyO3 bindings. No FFI, no `unsafe`, no Rust dependency — the rag
  layer can be iterated with `pytest` alone without recompiling the
  cdylib (RFC §2).
- `Document` (typing.Protocol, `@runtime_checkable`) + `SimpleDocument`
  reference implementation — duck-typed Document contract compatible
  with LangChain / LlamaIndex / Haystack out of the box (RFC §8.1).
- `IngestSchema` + `ingest_documents(drevo, docs, *, schema, kind,
  embedder)` — batched node creation with optional metadata mapping
  and embedder hook; full text always preserved under
  `properties["text"]` per RFC §8.2.
- `expand_neighborhood(drevo, node_uuid, *, hops, kind_filter,
  max_nodes)` — bounded BFS returning a `Neighborhood` dataclass with
  honest `hops_used` telemetry; the kind filter constrains the
  frontier, not the seed.
- `Retriever(drevo, *, hops, kind_filter, max_nodes)` with `retrieve`
  (dispatched by seed type: `str`→FTS, `int`→get_node,
  `uuid.UUID`→get_node_by_uuid) and `retrieve_with_embedding` (raises
  `NotImplementedError` until Phase 12 `00075` lands the HNSW index).
- `Context` + `ContextStats` frozen dataclasses + `Context.to_text(*,
  format="markdown"|"json"|"turtle")` — deterministic rendering
  (sorted by `(kind, title, id)` / `(from_id, to_id, id)`) so
  `00120` can assert byte-equality.
- `MMRReranker` — Maximum Marginal Relevance; semantics fixed per
  RFC §10 Q-4 (`lambda_=1.0` → pure relevance, `lambda_=0.0` → pure
  diversity). No drevo storage I/O, embedder called exactly once.
- `drevo-py/python/drevo/rag/__init__.pyi` — type stubs for the full
  public surface; `mypy --strict drevo/rag` clean.
- 10 text-level scaffolding tests in
  `tests/python_rag_idioms_tests.rs` locking the module layout, key
  exports, optional-dependency extras, and the contract that
  `import drevo` does NOT eagerly load `drevo.rag`.
- ~40 runtime pytest cases in `drevo-py/tests/test_rag.py` driving
  every public symbol from happy-path + edge cases.

### Added — task `00120` (Python end-to-end test suite)

- `drevo-py/tests/e2e/` — definition-of-done suite for Phase 16 per
  `drevo-tdd` §"E2E tests". Six scenario modules, each running the
  full domain workflow against the on-disk redb backend in a tempdir
  (no network, no real embedder):
  - `test_cbt_journal.py` — Cognitive Behavioural Therapy journal
    (situation / thought / emotion / cognitive_distortion /
    rational_response). Mirrors `tests/scenario_cbt_journal.rs` at
    the Python surface: per-kind census, BFS reach across the
    reframing chain, shortest path thought → calm, FTS distortion
    search, subgraph projection, close + reopen round-trip.
  - `test_story_editor.py` — long-form fiction outliner (book /
    chapter / scene / character / location / plot_arc). Reading-order
    walk, character screen-time lookup via inbound `appears_in`,
    plot-arc advancement chain, FTS recall on scene body, reopen
    round-trip. Mirrors `tests/scenario_story_editor.rs`.
  - `test_task_manager.py` — IT sprint board (project / sprint /
    task / person / label). Assignee column via inbound `assigned_to`,
    dependency-chain `shortest_path` with `edge_kind="depends_on"`,
    label projection, sprint-level subgraph, reopen round-trip.
    Mirrors `tests/scenario_task_manager.rs`.
  - `test_erp.py` — enterprise resource planning (company /
    department / employee / product / customer / purchase_order).
    Org-chart traversal, headcount via `employs`, management chain
    `shortest_path`, PO line items via `supplies`, full-org subgraph,
    PO-by-title FTS, reopen round-trip. Mirrors
    `tests/scenario_erp.rs`.
  - `test_bug_tracker.py` — bug + regression ledger (project / bug /
    component / release / person / test_case). Component-affecting
    bug projection, regression-per-release lookup, fixed-in changelog,
    uncovered-bug gap detection, body-text FTS, reopen round-trip.
    Mirrors `tests/scenario_bug_tracker.rs`.
  - `test_graph_rag.py` — the dedicated graph-RAG scenario called out
    in the 00120 brief: ingest a `SimpleDocument` corpus, wire
    reference edges, run `Retriever(hops=2)`, assert `Context` seeds
    + neighbours, exercise every `Context.to_text` format (markdown /
    json / turtle), confirm byte-stable output across runs, and
    persist + reopen so the same retrieval yields the same serialised
    context.
- Shared fixtures in `drevo-py/tests/e2e/conftest.py`: `tmp_db_path`
  (per-test temp dir with cleanup on scope exit), `disk_db` (one-shot
  context-managed handle), `deterministic_embedder` (SHA-256 → 8-d
  vector, no numpy, no network — keeps the suite runnable inside a
  bare wheel install).
- 18 text-level scaffolding tests in
  `tests/python_e2e_test_suite_tests.rs` that run on the Rust-only CI
  runners: directory + package-marker existence, conftest fixture
  signatures, scenario-module presence + disk-backend usage + reopen
  round-trips, domain-kind drift checks against the Rust scenario
  vocabulary, RAG-surface coverage (`Document` / `ingest_documents` /
  `Retriever` / `Context` named, all three `to_text` formats
  asserted, determinism contract asserted, embedder fixture wired),
  and this CHANGELOG entry.
- Total drevo-py runtime suite after 00120: 361 pytest cases
  (180+ unit + 52 integration + 40 e2e + the small `tests/` shim
  modules), all green against the on-disk redb backend.

### Pending

- `00119` — Python integration-test suite under `tests/integration/`.
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
- 40 Python runtime tests in `drevo-py/tests/test_shim.py` exercising
  the shim layer end-to-end: `Node.uuid` / `Edge.uuid` actually return
  `uuid.UUID` (not raw `bytes`), `InvalidWeightError` actually
  subclasses `ValueError`, every name in `__all__` actually resolves,
  every method enumerated in RFC §3.3 actually exists on `Drevo` at
  runtime, the `Direction` IntEnum has the documented integer values,
  the context manager closes the handle, and `get_node_by_uuid` still
  accepts raw `bytes` on input. Runs inside cibuildwheel's
  `CIBW_TEST_COMMAND` against every wheel before upload — broken wheel
  fails the matrix job.

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
