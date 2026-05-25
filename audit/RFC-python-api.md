# RFC-python-api — Python API Surface for `drevo-py`

**Task.** Phase 16 `00114` — design the public Python API surface for the
`drevo-py` package before any PyO3 code lands in `00115`.

**Status.** Accepted (this document is the contract `00115`–`00122` implement
against). Any deviation requires a follow-up amendment block at the bottom
of this file, not a silent change in the implementation.

**Scope.** Defines naming, type system, sync-vs-async story, error mapping,
iterator-vs-list returns, batch APIs, `Document`/`Retriever` graph-RAG
idioms, and the comparison against `neo4j` / `kuzu` / `falkordb` Python
drivers (idioms to borrow, antipatterns to avoid). **No code.** The
next task (`00115`) implements PyO3 bindings against the surface defined
here.

**Cites.**

- `.claude/skills/drevo-architecture/SKILL.md` §"SOLID Principles", §"Anti-Patterns to AVOID".
- `.claude/skills/drevo-rust/SKILL.md` §"Error Handling", §"FFI Safety", §"Async / Tokio".
- `.claude/skills/drevo-tdd/SKILL.md` §"Three test layers" (informs §8 below).
- `audit/AUDIT-error.md`, `audit/AUDIT-ffi.md` (existing FFI patterns drevo-py mirrors at the Python boundary).
- README.md §"Phase 16 — Python Graph-RAG SDK" (the parent task definition).

---

## 0. Why This RFC Exists

PyO3 bindings sit at the most user-facing boundary drevo ships. Decisions
made here (`drevo.NotFoundError` vs `drevo.errors.NotFound`, list-return
vs generator, `with Drevo.open(...)` vs explicit `close()`) ossify the
moment the first wheel reaches PyPI: every fix afterwards is a breaking
change.

The C FFI (`src/ffi.rs`) and WASM (`src/wasm.rs`) layers already exist
and were audited under `00110` / `00111`. Both make different trade-offs
than the Python layer should:

- **C FFI** — JSON-over-the-boundary, opaque `drevo_t*`, thread-local
  errno. Optimised for ABI stability across compilers; unergonomic for
  scripting.
- **WASM** — `JsValue::from_str(error)` losing the variant. Acceptable
  because browsers do not pattern-match exception types the way Python
  does.

Python is the first binding where users **will** write
`except drevo.NotFoundError:` and lean on typed exceptions, iterator
laziness, `with` blocks, `mypy --strict`, and PEP 484 generics. So the
Python surface gets its own contract — not a copy of the C surface, not
a copy of the WASM surface.

---

## 1. Goals and Non-Goals

### 1.1. Goals

1. **Idiomatic Python.** A Python developer who has never touched Rust
   should be able to read example code and recognise the conventions
   (`snake_case`, context managers, iterators, typed exceptions,
   `Optional[T]` returns where `None` is meaningful, dataclasses for
   plain data).
2. **Graph-RAG first.** The headline use case is retrieval-augmented
   generation: build a graph, ingest documents, embed, retrieve a seed
   + neighbourhood, format as LLM context. `drevo.rag` is a first-class
   layer in this RFC — not an afterthought bolted onto a generic
   "Python bindings" package.
3. **Three test layers per `drevo-tdd`.** Every public symbol has unit,
   integration, and e2e tests (`00118` / `00119` / `00120`).
4. **`mypy --strict` clean.** Type stubs (`drevo/__init__.pyi`,
   `drevo/rag.pyi`) ship in every wheel; `py.typed` marker tells
   downstream type-checkers to honour them.
5. **GIL discipline.** Every storage I/O call releases the GIL
   (`py.allow_threads`) so Python threads stay responsive; this is a
   **performance** contract, not a "we'll do it if convenient" one.
6. **Pure-Python idioms layer.** `drevo.rag` (`Retriever`, `Context`,
   `MMRReranker`, `ingest_documents`) is plain Python on top of the
   PyO3 bindings — no Rust dependency creep for orchestration logic
   that can live one layer up. Cites `drevo-architecture` §S
   (Single Responsibility) and §I (Interface Segregation): PyO3 owns
   storage I/O; `drevo.rag` owns retrieval composition.

### 1.2. Non-Goals

- **Async I/O at the FFI boundary** (sync only in `00115`; async-pyo3
  evaluated as a follow-up RFC, see §4).
- **Cypher executor wrappers** beyond a single gated `query(text, params=)`
  method (executor itself lands in Phase 10 `00063`; the Python API
  signature is reserved here but raises `NotImplementedError` until the
  Rust side ships).
- **LangChain / LlamaIndex / Haystack hard dependencies.** Adapters
  ship as optional extras (`pip install drevo-py[langchain]`); the
  core package accepts duck-typed `Document` objects with
  `.page_content: str` and `.metadata: dict`.
- **Vector storage from day one.** Vector helpers (`store_embedding`,
  `search_vectors`) are placeholder signatures in this RFC, gated
  behind a `vector=True` kwarg, fully implemented after Phase 12
  (`00075`–`00079`) lands the HNSW index.
- **Re-exporting the Rust `Drevo` type as-is.** PyO3 forces ownership
  decisions (Python GC vs Rust `Drop`); the Python `Drevo` is a thin
  wrapper, not a transparent re-export.

---

## 2. Package Layout

```
drevo-py/                       # workspace member (00115)
├── Cargo.toml                  # crate-type = ["cdylib"]
├── pyproject.toml              # PEP 621; build-backend = maturin
├── README.md
├── LICENSE
├── CHANGELOG.md
├── src/
│   └── lib.rs                  # PyO3 #[pymodule] entry point
├── python/
│   └── drevo/
│       ├── __init__.py         # re-exports + Pythonic shims
│       ├── __init__.pyi        # type stubs
│       ├── py.typed            # PEP 561 marker
│       ├── errors.py           # exception hierarchy (def in §5)
│       ├── rag/
│       │   ├── __init__.py
│       │   ├── retriever.py    # Retriever, Context, MMRReranker
│       │   ├── ingest.py       # ingest_documents, schema mapping
│       │   ├── _document.py    # duck-typed Document protocol
│       │   └── __init__.pyi
│       └── _compat.py          # any version-shim glue
└── tests/
    ├── unit/                   # 00118 — ~80 tests
    ├── integration/            # 00119 — ~40 tests
    └── e2e/                    # 00120 — 5 scenarios + RAG scenario
```

**Why split `python/drevo/` from `src/`?** Per `drevo-architecture`
§"SOLID — S": the PyO3 layer (Rust) owns storage I/O *only*. The
`drevo.rag` layer is pure Python — algorithmic, no FFI, no `unsafe`,
testable with `pytest` alone without compiling Rust. Mixing them in
one `.so` would make the rag layer un-iterable during development
(every change → recompile).

**Wheel layout.** `maturin build` produces a wheel that contains
`drevo/_drevo.<platform>.so` (the PyO3 module, imported lazily by
`drevo/__init__.py`) plus all `.py` files under `python/drevo/`. End
users `pip install drevo-py` and `import drevo`; the `_drevo` private
module is an implementation detail, never imported directly by users.

---

## 3. Naming and Type System

### 3.1. Naming

| Layer            | Convention            | Examples                                   |
|------------------|-----------------------|--------------------------------------------|
| modules          | `lower_snake_case`    | `drevo`, `drevo.rag`, `drevo.errors`       |
| classes          | `UpperCamelCase`      | `Drevo`, `Node`, `Edge`, `Retriever`       |
| functions/methods| `lower_snake_case`    | `create_node`, `search_fts`, `to_text`     |
| constants        | `UPPER_SNAKE_CASE`    | `Direction.OUT`, `MAX_BFS_DEPTH`           |
| private          | `_leading_underscore` | `_Drevo`, `_validate_kind`                 |
| dunder ports     | reserved              | `__enter__`, `__exit__`, `__iter__`, `__repr__` |

Rust method names map by case conversion: `create_node` (Rust) →
`create_node` (Python; identical), `list_nodes_by_kind` → `list_nodes_by_kind`.
**No re-spellings** ("createNode", "listByKind"); the Rust spelling is
already idiomatic Python `snake_case`.

### 3.2. Type Mapping

| Rust                          | Python                              | Notes                                                  |
|-------------------------------|-------------------------------------|--------------------------------------------------------|
| `u64` (node/edge id)          | `int`                               | Python ints are arbitrary precision; no overflow.     |
| `[u8; 16]` (UUID v7)          | `bytes` (len 16) **or** `uuid.UUID` | `Node.uuid` returns `uuid.UUID`; bytes accepted on input. |
| `&str`, `String`              | `str`                               | UTF-8 enforced at boundary; non-UTF-8 → `ValueError`.  |
| `f32` (edge weight)           | `float`                             | NaN/±Inf → `ValueError` (mirrors `InvalidWeight`).     |
| `i64` (timestamp_ms)          | `int`                               | Caller can wrap in `datetime.fromtimestamp(ms / 1000)`.|
| `serde_json::Value`           | Python native (`dict`/`list`/`str`/`int`/`float`/`bool`/`None`) | `properties` round-trips through `serde_json` ↔ `pythonize`. |
| `HashMap<String, Value>` (Properties) | `dict[str, Any]`            | Order-preserved on the Python side (Python 3.7+ dict). |
| `Vec<T>`                      | `list[T]`                           | See §6 on iterator-vs-list trade-offs.                 |
| `Option<T>`                   | `T | None` (PEP 604)                | Returned where `None` is semantic (e.g., `get_node`).  |
| `Result<T, DrevoError>`       | `T` or raise (see §5)               | No `Result` type leaks; errors raise.                  |
| `Direction` enum              | `drevo.Direction` (IntEnum)         | `Direction.OUT`, `Direction.IN`, `Direction.BOTH`.     |
| `SubGraph`                    | `drevo.SubGraph` (dataclass)        | `nodes: list[Node]`, `edges: list[Edge]`.              |
| `ScoredNode`                  | `drevo.ScoredNode` (dataclass)      | `node: Node`, `score: float`.                          |

**Plain-data classes** (`Node`, `Edge`, `NewNode`, `NewEdge`, `NodePatch`,
`EdgePatch`, `SubGraph`, `ScoredNode`) are emitted as `@dataclass(frozen=True)`
on the Python side (via PyO3 `#[pyclass(frozen)]`), so they are hashable,
`__eq__`-able, `__repr__`-able by default. Mutation goes through
`NodePatch`/`EdgePatch`, mirroring the Rust side — no in-place property
edits. Cites `drevo-architecture` §"Stringly Typed" anti-pattern
(decision: typed dataclasses instead of `dict`).

### 3.3. Type Stubs

`drevo/__init__.pyi` is hand-authored (PyO3 stubgen output reviewed and
trimmed), checked into the repo, shipped in every wheel. CI runs
`mypy --strict drevo/` on the stubs against a representative usage
snippet so the stubs cannot silently drift from the runtime surface.

Example stub fragment:

```python
# drevo/__init__.pyi
from __future__ import annotations
from typing import Iterator, Optional, Self
from uuid import UUID
from drevo.errors import (
    DrevoError, NotFoundError, ConflictError, StorageError,
)

class Direction:
    OUT: Direction
    IN: Direction
    BOTH: Direction

class Node:
    id: int
    uuid: UUID
    kind: str
    title: str
    properties: dict[str, object]
    created_at_ms: int
    updated_at_ms: int

class Edge:
    id: int
    uuid: UUID
    src: int
    dst: int
    kind: str
    weight: float
    properties: dict[str, object]
    created_at_ms: int
    updated_at_ms: int

class Drevo:
    @classmethod
    def open(cls, path: str | bytes) -> Drevo: ...
    @classmethod
    def open_in_memory(cls) -> Drevo: ...
    def __enter__(self) -> Self: ...
    def __exit__(self, *exc: object) -> None: ...
    def close(self) -> None: ...
    def compact(self) -> CompactReport: ...
    def health_check(self) -> None: ...

    def create_node(self, new_node: NewNode) -> Node: ...
    def get_node(self, id: int) -> Optional[Node]: ...
    def get_node_by_uuid(self, uuid: UUID | bytes) -> Optional[Node]: ...
    def get_node_by_title(self, title: str) -> Optional[Node]: ...
    def update_node(self, id: int, patch: NodePatch) -> Node: ...
    def delete_node(self, id: int) -> None: ...
    # … etc. (full list mirrors the Rust public surface)
```

---

## 4. Sync vs Async

### 4.1. Default — Synchronous

`00115` ships a **synchronous** PyO3 surface. Every method blocks until
the underlying redb transaction commits.

Rationale:

1. **redb is blocking** by design (`drevo-rust` §"Async / Tokio"). Adding
   `async` at the FFI boundary without an async storage backend would
   only paper over the truth.
2. **Python's most common consumers are synchronous**: data-science
   scripts, Jupyter notebooks, batch ingest jobs, MCP servers. Forcing
   `await` everywhere is a tax for a benefit only the small async-Python
   community sees.
3. **GIL release** (next subsection) lets the *Python* runtime stay
   responsive without the FFI being `async`. Threads continue to run
   while drevo holds a write transaction.

### 4.2. GIL Release Contract

Every storage I/O call wraps the Rust body in `py.allow_threads(|| {...})`.
**This is a hard contract**, audited per method in `00118`:

- `create_node`, `update_node`, `delete_node`, `create_edge`, `update_edge`,
  `delete_edge`, `compact`, `health_check`, `verify_invariants` — release.
- `get_node`, `get_node_by_uuid`, `get_node_by_title`, `get_edge`, `bfs`,
  `dfs`, `shortest_path`, `subgraph`, `search_fts` — release.
- `open`, `open_in_memory`, `close` — release (file system + lock).
- Property getters (`Node.id`, `Node.title`) — **do not** release
  (no I/O; the allow_threads round-trip would dominate).

CI test (`tests/unit/test_gil.py` under `00118`): spawn a Python thread
that ticks `threading.Event` once per 10 ms; call a long-running
`drevo.search_fts("...", limit=1000)` on the main thread; assert the
background thread tick count is ≥ 90% of the wall-clock-derived
expectation. A drop below 90% means a method forgot to release the GIL.

### 4.3. Async Story — Future Work

A follow-up RFC (`RFC-python-async.md`, deferred until Phase 13 MVCC
lands) will evaluate `pyo3-async-runtimes` (formerly `pyo3-asyncio`).
The likely shape is an *additional* `drevo.aio.Drevo` class that wraps
the sync `Drevo` via `asyncio.to_thread`, **not** a duplicate set of
`async def` PyO3 methods. Reasons:

- redb stays sync; `asyncio.to_thread` is honest about that.
- The sync API surface stays the single source of truth; the async
  layer is a thin shim.
- Avoids the "coloured functions" problem of maintaining two parallel
  implementations.

This RFC reserves the namespace `drevo.aio` but does not implement it
in Phase 16.

### 4.4. Concurrency Model

`Drevo` is `Send + Sync` on the Rust side (`Arc<RwLock<...>>` under the
hood). On the Python side a single `Drevo` instance is safe to share
across threads — multiple threads may call `get_node` concurrently;
writers serialise via the inner `RwLock`. The PyO3 wrapper does **not**
add a Python-level lock; the storage layer already provides MVCC for
reads and serialised writes.

Multi-process usage (`multiprocessing`, `fork`) is **not supported**.
Open a fresh `Drevo` instance per process — redb file locks otherwise
panic on the second opener. Documented in the `Drevo.open` docstring
with an example.

---

## 5. Error Mapping

### 5.1. Hierarchy

```
BaseException
└── Exception
    ├── drevo.DrevoError                  (root; ValueError is NOT a parent)
    │   ├── drevo.NotFoundError
    │   │   ├── drevo.NodeNotFoundError   (id: int)
    │   │   └── drevo.EdgeNotFoundError   (id: int)
    │   ├── drevo.ConflictError
    │   │   └── drevo.DuplicateTitleError (title: str)
    │   ├── drevo.StorageError            (source: Exception)
    │   ├── drevo.SerializationError      (kind: 'encode' | 'decode')
    │   ├── drevo.LockedError
    │   └── drevo.PanicError              (FFI panic re-raised; see §5.4)
    └── ValueError                        (drevo invalid-input cases)
        └── drevo.InvalidWeightError      (weight: float)
```

**Rationale for the root being `drevo.DrevoError`, not `ValueError`:**
generic `except ValueError:` blocks in user code would inadvertently
swallow drevo errors. A user who wants to catch "anything from drevo"
writes `except drevo.DrevoError:`; a user who wants only invalid-input
errors writes `except ValueError:`. The two are intentionally
disjoint, with one exception: `InvalidWeightError` extends `ValueError`
(not `DrevoError`) because the canonical Python idiom for "you passed
me a bad number" is `ValueError`, and code that already handles
`ValueError` should not need to learn a drevo-specific type for this
case. Cites `drevo-rust` §"Error Handling" and `drevo-architecture`
§"Stringly Typed" anti-pattern.

### 5.2. Variant Mapping (Concrete)

| Rust variant                                  | Python exception                | `__init__` args                  |
|-----------------------------------------------|---------------------------------|----------------------------------|
| `DrevoError::NodeNotFound(u64)`               | `drevo.NodeNotFoundError`       | `(id: int)`                      |
| `DrevoError::EdgeNotFound(u64)`               | `drevo.EdgeNotFoundError`       | `(id: int)`                      |
| `DrevoError::DuplicateTitle(String)`          | `drevo.DuplicateTitleError`     | `(title: str)`                   |
| `DrevoError::InvalidWeight(f32)`              | `drevo.InvalidWeightError`      | `(weight: float)`                |
| `DrevoError::Locked`                          | `drevo.LockedError`             | `()`                             |
| `DrevoError::Storage(StorageError)`           | `drevo.StorageError`            | `(source: Exception)`            |
| `DrevoError::Encode(EncodeError)`             | `drevo.SerializationError`      | `(kind='encode', source=...)`    |
| `DrevoError::Decode(DecodeError)`             | `drevo.SerializationError`      | `(kind='decode', source=...)`    |
| `DrevoError::Io(io::Error)`                   | `drevo.StorageError`            | `(source=OSError(...))`          |
| **panic across FFI**                          | `drevo.PanicError`              | `(message: str)`                 |

The Rust variant is preserved in `.__cause__` (PyO3's
`set_cause`) so `traceback.format_exception()` shows the full chain.

### 5.3. PyO3 Implementation Sketch

```rust
// src/lib.rs (drevo-py crate)
use pyo3::create_exception;
use pyo3::exceptions::PyValueError;

create_exception!(drevo, DrevoError,         pyo3::exceptions::PyException);
create_exception!(drevo, NotFoundError,      DrevoError);
create_exception!(drevo, NodeNotFoundError,  NotFoundError);
create_exception!(drevo, EdgeNotFoundError,  NotFoundError);
create_exception!(drevo, ConflictError,      DrevoError);
create_exception!(drevo, DuplicateTitleError,ConflictError);
create_exception!(drevo, StorageError,       DrevoError);
create_exception!(drevo, SerializationError, DrevoError);
create_exception!(drevo, LockedError,        DrevoError);
create_exception!(drevo, PanicError,         DrevoError);
// InvalidWeightError extends PyValueError — declared in pure Python in
// drevo/errors.py so the inheritance reads naturally to `mypy`.

fn map_err(py: Python<'_>, e: drevo::DrevoError) -> PyErr {
    match e {
        drevo::DrevoError::NodeNotFound(id) =>
            NodeNotFoundError::new_err((id,)),
        drevo::DrevoError::EdgeNotFound(id) =>
            EdgeNotFoundError::new_err((id,)),
        drevo::DrevoError::DuplicateTitle(t) =>
            DuplicateTitleError::new_err((t,)),
        drevo::DrevoError::InvalidWeight(w) =>
            PyValueError::new_err(format!("invalid edge weight: {w}")),
        drevo::DrevoError::Locked =>
            LockedError::new_err(()),
        drevo::DrevoError::Storage(s) =>
            StorageError::new_err(s.to_string()),
        drevo::DrevoError::Encode(e) =>
            SerializationError::new_err(("encode", e.to_string())),
        drevo::DrevoError::Decode(e) =>
            SerializationError::new_err(("decode", e.to_string())),
        drevo::DrevoError::Io(e) =>
            StorageError::new_err(e.to_string()),
    }
}
```

### 5.4. Panic Safety

Every `#[pyfunction]` / `#[pymethods]` body is wrapped in
`std::panic::catch_unwind`. A caught panic raises `drevo.PanicError`
with the message converted from the panic payload. Mirrors `drevo-rust`
§"FFI Safety — No panics across FFI" and the existing `src/ffi.rs`
discipline (`audit/AUDIT-ffi.md` §"panic-catch contract").

CI test: `tests/unit/test_panic_safety.py` forces a panic via a
crafted invalid input and asserts `drevo.PanicError` is raised, not
SIGABRT.

### 5.5. Error Messages Are Stable

Exception messages are part of the public API for the duration of a
major version. Tests assert on substring matches, not exact equality,
to give us room for adding context (e.g., "node not found: 42" →
"node not found: id=42 (after 0 hops)") without breaking downstream
matchers.

---

## 6. Iterator vs List Returns

### 6.1. Default — Lists

The default return for "all results" is `list[T]`:

- `list_nodes_by_kind(kind, limit, offset)` → `list[Node]`
- `list_edges_by_kind(kind, limit, offset)` → `list[Edge]`
- `search_fts(query, *, limit)` → `list[ScoredNode]`
- `bfs(root, depth, ...)` → `list[Node]`
- `subgraph(root, depth)` → `SubGraph` (which holds two `list`s)

**Rationale.** Python users expect to slice, index, `len()`, JSON-serialise,
and iterate twice. Returning a generator forces them to wrap in `list(...)`
in 90% of cases. The Rust API already enforces `limit` / `offset` /
`depth` bounds, so unbounded result sets are not a concern at this
layer.

### 6.2. Opt-In Iterators

For known-large surfaces, a generator variant is exposed with a
`_iter` suffix:

- `iter_nodes_by_kind(kind, *, batch_size=1000) -> Iterator[Node]`
- `iter_edges_by_kind(kind, *, batch_size=1000) -> Iterator[Edge]`
- `iter_recent(*, batch_size=1000) -> Iterator[Node]`

The iterator internally calls the bounded paginated method in chunks
of `batch_size` and yields nodes one at a time. The GIL is released
inside each batch fetch. **Not** a streaming cursor — a paginated
generator. We add a true streaming cursor in a later phase if
benchmarks show pagination overhead is material.

### 6.3. Traversal Sequences

`bfs` / `dfs` return `list[Node]` by default. Once Phase 12 (vector)
or Phase 13 (MVCC) introduces a streaming traversal API on the Rust
side, a corresponding `bfs_iter` / `dfs_iter` is added here. Not in
scope for `00115`.

---

## 7. Batch APIs

### 7.1. Why Batches Matter

`drevo-rust` §"redb Transaction Patterns — Batch writes" calls out that
per-operation ACID transactions on 100K+ inserts measure ~530s on the
project's reference machine — unusable. Python users hitting this from
a `for doc in docs: drevo.create_node(...)` loop would see the same
pathology without warning.

### 7.2. API

```python
# create_nodes — single transaction, single GIL release
def create_nodes(self, new_nodes: list[NewNode]) -> list[Node]: ...

# create_edges — same shape
def create_edges(self, new_edges: list[NewEdge]) -> list[Edge]: ...

# update_nodes / update_edges — list of (id, patch) tuples
def update_nodes(self, patches: list[tuple[int, NodePatch]]) -> list[Node]: ...
def update_edges(self, patches: list[tuple[int, EdgePatch]]) -> list[Edge]: ...

# delete_nodes / delete_edges — list of ids; all-or-nothing
def delete_nodes(self, ids: list[int]) -> None: ...
def delete_edges(self, ids: list[int]) -> None: ...
```

Semantics:

- **All-or-nothing.** If any item in the batch fails (e.g., a duplicate
  title), the entire transaction rolls back and the corresponding
  exception is raised. The exception carries the **index** of the
  offending item in `.__notes__` (Python 3.11+) so users can locate
  it without re-trying linearly.
- **Same GIL-release pattern.** A single `py.allow_threads` around the
  whole batch — not one per item.
- **Order preserved.** Returned `list[Node]` is in the same order as
  the input `list[NewNode]`.

### 7.3. Iteration of Input

Batch methods accept any `Iterable[NewNode]`, materialised internally
into a `Vec` before crossing the FFI. We accept the small memory cost
(N PyObject → Rust struct conversions) in exchange for guaranteeing
"the entire batch is in one redb transaction" — partial batching would
re-introduce the per-op ACID pathology.

---

## 8. Graph-RAG Idioms (`drevo.rag`)

This layer is the **headline value** of Phase 16. It is pure Python on
top of the PyO3 bindings — no Rust dependency creep — so the algorithm
choices below are reviewable by anyone with a Python stack trace and
without a Rust compiler.

### 8.1. `Document` Protocol

```python
# drevo/rag/_document.py
from typing import Protocol

class Document(Protocol):
    """Duck-typed interface for ingestable text+metadata records.

    Compatible with LangChain Document, LlamaIndex Document, Haystack
    Document, and any plain object with these two attributes.
    """
    page_content: str
    metadata: dict[str, object]
```

No `from langchain_core.documents import Document` import — the protocol
is structural. Adapters for the three big frameworks ship as optional
extras (`drevo-py[langchain]`, `drevo-py[llama-index]`,
`drevo-py[haystack]`) that re-export their `Document` type for
convenience, but the core package has zero hard dependency on any of
them.

### 8.2. `ingest_documents`

```python
def ingest_documents(
    drevo: Drevo,
    docs: list[Document],
    *,
    schema: IngestSchema | None = None,
    kind: str = "doc",
    embedder: Callable[[list[str]], list[list[float]]] | None = None,
) -> list[Node]:
    """Ingest a list of duck-typed Documents as nodes.

    Each Document becomes one Node with:
      - kind=kind (default "doc")
      - title=truncate(doc.page_content, 200) unless schema overrides
      - properties=doc.metadata | {"text": doc.page_content}

    If `embedder` is provided, also stores the embedding under the
    property key "embedding" (list[float]). Phase 12 (vector index)
    later promotes this to first-class vector storage.

    If `schema` is provided, it maps Document.metadata fields to
    Node.properties / Node.kind / Node.title — see IngestSchema below.
    """
```

`IngestSchema` is a small dataclass:

```python
@dataclass(frozen=True)
class IngestSchema:
    kind_from: str | None = None        # metadata key → Node.kind
    title_from: str | None = None       # metadata key → Node.title
    property_map: dict[str, str] = field(default_factory=dict)  # metadata key → property key
    edge_specs: list[EdgeSpec] = field(default_factory=list)    # see below
```

### 8.3. `Retriever`

```python
class Retriever:
    def __init__(
        self,
        drevo: Drevo,
        *,
        hops: int = 2,
        kind_filter: list[str] | None = None,
        max_nodes: int = 50,
    ): ...

    def retrieve(
        self,
        seed: str | int | UUID,         # FTS query, node id, or node uuid
        *,
        limit: int = 10,
    ) -> Context:
        """Resolve `seed` to one or more seed nodes, expand `hops`
        deep, and return a Context object containing the seed nodes,
        the expanded neighbourhood, and a deterministic ordering."""

    def retrieve_with_embedding(
        self,
        embedding: list[float],
        *,
        limit: int = 10,
    ) -> Context:
        """Vector-store variant — gated; raises NotImplementedError
        until Phase 12 (00075) ships the HNSW index."""
```

### 8.4. `Context`

```python
@dataclass(frozen=True)
class Context:
    seeds: list[Node]            # the nodes the retriever hit first
    neighbours: list[Node]       # everything within `hops` of seeds, minus seeds
    edges: list[Edge]            # all edges connecting any pair in seeds | neighbours
    stats: ContextStats          # for telemetry: hops actually used, dedup count, etc.

    def to_text(self, *, format: str = "markdown") -> str:
        """Format the context as LLM-ready text.

        format = "markdown"  — headings + bullet lists (default)
        format = "json"      — JSON object {seeds, neighbours, edges}
        format = "turtle"    — RDF Turtle (for SPARQL/RAG hybrid stacks)
        """
```

`Context.to_text` is deterministic given the same context — sort by
`(kind, title, id)`. This is what `tests/e2e/` (`00120`) asserts on.

### 8.5. `MMRReranker`

```python
class MMRReranker:
    """Maximum Marginal Relevance reranker.

    Given candidate nodes + their similarity-to-query scores + an
    embedder, pick top-k that maximise relevance while minimising
    redundancy.

    Pure math; no drevo storage I/O.
    """
    def __init__(self, *, lambda_: float = 0.5): ...
    def rerank(
        self,
        candidates: list[ScoredNode],
        *,
        embedder: Callable[[list[str]], list[list[float]]],
        k: int,
    ) -> list[ScoredNode]: ...
```

The reranker has its own unit tests (`00118`) — closed-form expected
output for a 3-element input given a fixed `lambda_` and a deterministic
embedder stub.

---

## 9. Comparison to Existing Python Drivers

| Driver       | Idioms to borrow                                                                                                                   | Antipatterns to avoid                                                                                                                                            |
|--------------|------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `neo4j` (official Python driver) | Context-manager driver (`with GraphDatabase.driver(uri) as driver:`); `session.run(cypher, params=)` shape; typed `Record` objects keyed by column. | Implicit transaction commit on success — drevo will be explicit (`with drevo.write_tx() as tx:` once MVCC lands). `Result.data()` returning `list[dict]` loses typing — we keep dataclasses. |
| `kuzu`       | Tight Rust core + thin Python wrapper; `Connection.execute(query, params)` returning a typed result; columnar arrow returns for analytics. | "Connection" abstraction without an explicit `with`-block default (users forget to `.close()`). drevo's `Drevo.open` returns an instance with `__enter__`/`__exit__` to make resource ownership obvious. |
| `falkordb`   | Cypher-first API; explicit `graph.query("...")`; cleanly mirrors the Redis-graph wire protocol.                                    | Returns rows as `list[list[Any]]` (positional, no column names). drevo's row API will return named dataclasses or `dict[str, Any]` — never positional lists.    |
| `redis-py`   | `pipeline()` context manager for batched ops; clean async/sync split (`redis.Redis` vs `redis.asyncio.Redis`).                     | Two separate import paths (`from redis import Redis` / `from redis.asyncio import Redis`) is the model drevo follows (`drevo.Drevo` + future `drevo.aio.Drevo`), but redis-py also has parallel signatures we will avoid by making `aio.Drevo` a thin wrapper. |

### 9.1. Concrete Idioms Adopted

- **`with` block as the default open-pattern.** `with Drevo.open(path) as d: ...` mirrors `neo4j`'s driver lifecycle.
- **Named dataclasses for rows**, not positional lists or untyped dicts. Mirrors `kuzu`'s columnar dataclass-y returns; avoids `falkordb`'s positional rows.
- **Explicit `params=`** dict on `query(cypher, params=...)` — `neo4j` and `falkordb` both agree on this shape.
- **Pipeline-style batches** for bulk writes (`drevo.create_nodes([...])`) — mirrors `redis-py`'s pipeline, just exposed as a method not a context manager. We picked the method form because there is no read-side analogue — pipelines in redis-py do both — and a method matches the all-or-nothing transactional semantics.

### 9.2. Concrete Idioms Rejected

- **Lazy execution where success is reported only at iteration time.** `neo4j` returns a `Result` that fails only when you iterate. We raise immediately on the bound method call (after `py.allow_threads` returns). Lazy-fail-on-iterate makes error backtraces useless for the typical `try: nodes = drevo.search_fts(...); except DrevoError:` pattern.
- **Untyped row returns** (`falkordb`). Every row in drevo is a dataclass with named fields.
- **Two parallel `async def` implementations** (`redis-py`). We will ship `drevo.aio.Drevo` as a thin wrapper, not a re-implemented surface.
- **String-based query builders** (`py2neo`'s `Cypher.create(...)`). drevo's API is direct method calls (`create_node`, `bfs`, ...) until the Cypher executor (Phase 10 `00063`) lands; then `Drevo.query(text, params=...)` is added. No DSL.

---

## 10. Open Questions

Each is a tracked decision point. Resolved before `00115` PR opens, or
recorded as deferred with a follow-up RFC.

| ID  | Question                                                                                                            | Default if undecided                                          | Owner / Resolved-in    |
|-----|---------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------|------------------------|
| Q-1 | Do `Node.uuid` / `Edge.uuid` return `bytes` (len 16) or `uuid.UUID`?                                                | `uuid.UUID` (Pythonic); accept `bytes` on input.              | `00115` (this RFC)     |
| Q-2 | Does `Drevo.__del__` call `close()` implicitly?                                                                     | No — explicit `close()` or `with`-block. `__del__` warns once. | `00115` (this RFC)     |
| Q-3 | What's the wire format for `Context.to_text(format="json")`?                                                        | The dataclasses' default `__dict__` shape, stable across versions. | `00117` (lock in tests) |
| Q-4 | Are `MMRReranker` lambda semantics `1.0 = pure relevance` or `1.0 = pure diversity`?                                | `1.0 = pure relevance, 0.0 = pure diversity` (matches MMR paper). | `00117` (this RFC)     |
| Q-5 | Does `ingest_documents` deduplicate by content hash?                                                                | No deduplication by default; expose `deduplicate_by="title"` kwarg in a follow-up. | `00117` (deferred)     |
| Q-6 | Does `query(cypher_text, params=...)` accept positional params (`"$1"` / `"?"`) or only named (`"$name"`)?          | Named only — matches `neo4j`-driver convention.               | After `00063` lands    |
| Q-7 | Does the Python package vendor pre-built wheels, or require source builds?                                          | Pre-built wheels via `cibuildwheel` (`00116`).                | `00116`                |
| Q-8 | How are `drevo.Direction.OUT` / `IN` / `BOTH` exposed: `IntEnum`, `Enum`, or sentinel objects?                      | `IntEnum` so JSON-serialisable and ordered.                   | `00115` (this RFC)     |
| Q-9 | Does `bfs` accept a Python callable `visitor=fn` for streaming, or only collect-then-return?                        | Collect-then-return for `00115`; streaming variant in a later phase if benchmarks demand. | `00115` (this RFC) |
| Q-10| For multi-process safety, do we expose `Drevo.is_open_elsewhere(path)`?                                             | No — redb's file lock is the source of truth; document the exception. | `00115` (this RFC) |

---

## 11. Definition of Done for This RFC

This RFC is "done" when:

- [x] Naming, type system, sync/async story, error mapping, iterator-vs-list,
      batch APIs, graph-RAG idioms, comparison to other drivers — all
      documented above.
- [x] Every public Python symbol that `00115` will implement appears in
      §3.3 (type stubs) or §8 (rag layer).
- [x] Every `DrevoError` variant currently in `src/error.rs` has a row
      in §5.2 (mapping table).
- [x] Open questions are listed in §10 with a default position so
      `00115` can proceed without re-litigating.

**This RFC is the contract.** `00115` implements against §3–§8 exactly.
Any deviation in `00115` requires a follow-up amendment block below.

---

## 12. Amendments

*(None yet. Add a dated `## 12.N — <title>` block here for any
post-acceptance change. Mention the PR that landed the change.)*
