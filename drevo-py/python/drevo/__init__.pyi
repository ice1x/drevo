"""Public type stubs for the drevo package.

These stubs mirror the runtime surface exported by the `_drevo` PyO3
extension and the pure-Python shim in `drevo/__init__.py`. Hand-authored
(PyO3 stubgen output trimmed and audited) per RFC §3.3 — the stubs
are the *contract* mypy sees, separate from whatever the cdylib
introspects at import time.

Generated wheels ship this `.pyi` alongside the `py.typed` marker
(PEP 561), so downstream `mypy --strict drevo/` resolves every symbol
declared here. The runtime types in `_drevo` are the source of truth
for behaviour; the stubs are the source of truth for *signatures*.

Implements Phase 16 task `00116`.
"""

from __future__ import annotations

import enum
import os
import types as _types
import uuid
from collections.abc import Mapping
from typing import Any, Final, Optional, Union

__version__: str

# ── Enums ──────────────────────────────────────────────────────────────

class Direction(enum.IntEnum):
    """Traversal direction for edge-following operations.

    Values match `drevo::model::Direction` exactly so users can pass
    either the named member or its integer value at the boundary.
    """

    OUT = 0
    IN = 1
    BOTH = 2

# ── Plain-data wrappers ────────────────────────────────────────────────

class Node:
    """Frozen view onto a node row.

    Instances are produced by `Drevo.create_node`, `Drevo.get_node`, and
    every traversal / FTS method. The class is hashable and supports
    structural equality (`==`).
    """

    id: int
    uuid: uuid.UUID
    kind: str
    title: str
    body: str
    body_html: str
    created_at: int
    updated_at: int
    properties: dict[str, Any]

    def __eq__(self, other: object) -> bool: ...
    def __repr__(self) -> str: ...
    def __hash__(self) -> int: ...

class Edge:
    """Frozen view onto an edge row."""

    id: int
    uuid: uuid.UUID
    from_id: int
    to_id: int
    kind: str
    weight: float
    created_at: int
    properties: dict[str, Any]

    def __eq__(self, other: object) -> bool: ...
    def __repr__(self) -> str: ...
    def __hash__(self) -> int: ...

class NewNode:
    """Input record passed to `Drevo.create_node`.

    All keyword arguments; `body`, `body_html`, and `properties` default
    to empty.
    """

    kind: str
    title: str
    body: str
    body_html: str
    properties: dict[str, Any]

    def __init__(
        self,
        *,
        kind: str,
        title: str,
        body: str = ...,
        body_html: str = ...,
        properties: Optional[Mapping[str, Any]] = ...,
    ) -> None: ...
    def __repr__(self) -> str: ...

class NewEdge:
    """Input record passed to `Drevo.create_edge`."""

    from_id: int
    to_id: int
    kind: str
    weight: float
    properties: dict[str, Any]

    def __init__(
        self,
        *,
        from_id: int,
        to_id: int,
        kind: str,
        weight: float = ...,
        properties: Optional[Mapping[str, Any]] = ...,
    ) -> None: ...
    def __repr__(self) -> str: ...

class NodePatch:
    """Partial update for an existing node."""

    def __init__(
        self,
        *,
        kind: Optional[str] = ...,
        title: Optional[str] = ...,
        body: Optional[str] = ...,
        body_html: Optional[str] = ...,
        properties: Optional[Mapping[str, Any]] = ...,
    ) -> None: ...
    def __repr__(self) -> str: ...

class EdgePatch:
    """Partial update for an existing edge."""

    def __init__(
        self,
        *,
        kind: Optional[str] = ...,
        weight: Optional[float] = ...,
        properties: Optional[Mapping[str, Any]] = ...,
    ) -> None: ...
    def __repr__(self) -> str: ...

class ScoredNode:
    """FTS hit — pair of `Node` + TF-IDF score."""

    node: Node
    score: float

    def __repr__(self) -> str: ...

class SubGraph:
    """Bounded-depth slice of the graph."""

    nodes: list[Node]
    edges: list[Edge]

    def __repr__(self) -> str: ...

class CompactReport:
    """Result of `Drevo.compact` — operator-facing storage metrics."""

    bytes_before: Optional[int]
    bytes_after: Optional[int]
    bytes_reclaimed: int
    next_node_id: int
    next_edge_id: int

    def as_dict(self) -> dict[str, Any]: ...
    def __repr__(self) -> str: ...

class BloatReport:
    """Result of `Drevo.bloat_report` — physical vs. logical storage size."""

    file_bytes: Optional[int]
    logical_bytes: int
    node_count: int
    edge_count: int
    bloat_ratio: Optional[float]

    def as_dict(self) -> dict[str, Any]: ...
    def __repr__(self) -> str: ...

class ImportReport:
    """Result of `Drevo.import_graphml` — rows inserted vs. skipped."""

    nodes_imported: int
    edges_imported: int
    nodes_skipped: int
    edges_skipped: int

    def as_dict(self) -> dict[str, Any]: ...
    def __repr__(self) -> str: ...

# ── Exception hierarchy ────────────────────────────────────────────────

class DrevoError(Exception):
    """Root of the drevo exception hierarchy."""

class NotFoundError(DrevoError):
    """A lookup failed because the requested entity does not exist."""

class NodeNotFoundError(NotFoundError):
    """A node lookup failed. `.args == (node_id,)`."""

class EdgeNotFoundError(NotFoundError):
    """An edge lookup failed. `.args == (edge_id,)`."""

class ConflictError(DrevoError):
    """A write conflicted with a uniqueness invariant."""

class DuplicateTitleError(ConflictError):
    """A node title collided with an existing one. `.args == (title,)`."""

class StorageError(DrevoError):
    """The underlying backend failed (redb, filesystem, etc.)."""

class SerializationError(DrevoError):
    """Encode / decode of a node, edge, or property failed."""

class LockedError(DrevoError):
    """Another process holds the exclusive file lock on the database."""

class NeedsMigrationError(DrevoError):
    """The on-disk adjacency format predates this build and must be migrated.

    Raised by `Drevo.open` when the database uses the pre-#243-slice-2
    adjacency layout. `.args == (found_major, required_major)`. Fix with
    `Drevo.migrate(path, "up")` or the `drevo migrate up` CLI.
    """

class PanicError(DrevoError):
    """A Rust panic was caught at the FFI boundary."""

class InvalidWeightError(ValueError):
    """An edge weight was non-finite (`NaN` / ±`inf`).

    Subclass of `ValueError` per RFC §5.3 — the canonical Python idiom
    for "you passed me a bad number". Backed by the pure-Python class
    in `drevo.errors`.
    """

# ── Handle ─────────────────────────────────────────────────────────────

_PathLike = Union[str, bytes, os.PathLike[str], os.PathLike[bytes]]

class Drevo:
    """Embedded graph database handle.

    Use `Drevo.open(path)` for a disk-backed instance, or
    `Drevo.open_in_memory()` for an ephemeral one. Every method releases
    the GIL around storage I/O so background Python threads stay
    responsive (RFC §4.2).

    The handle is reusable as a context manager — `with Drevo.open(p) as
    db:` closes the file lock on `__exit__` even if the body raises.
    """

    @classmethod
    def open(cls, path: _PathLike) -> Drevo: ...
    @classmethod
    def open_in_memory(cls) -> Drevo: ...
    def close(self) -> None: ...
    def __enter__(self) -> Drevo: ...
    def __exit__(
        self,
        exc_type: Optional[type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[_types.TracebackType],
    ) -> bool: ...
    @staticmethod
    def migrate(path: _PathLike, direction: str) -> int: ...
    def compact(self) -> CompactReport: ...
    def bloat_report(self) -> BloatReport: ...
    def health_check(self) -> None: ...
    def export_graphml(self) -> str: ...
    def export_graphml_to_path(self, path: _PathLike) -> None: ...
    def import_graphml(self, xml: str) -> ImportReport: ...
    def import_graphml_from_path(self, path: _PathLike) -> ImportReport: ...

    # ── Node CRUD ──────────────────────────────────────────────────────
    def create_node(self, new_node: NewNode) -> Node: ...
    def create_nodes(self, new_nodes: list[NewNode]) -> list[Node]: ...
    def get_node(self, id: int) -> Optional[Node]: ...
    def get_node_by_uuid(self, uuid: bytes) -> Optional[Node]: ...
    def get_node_by_title(self, title: str) -> Optional[Node]: ...
    def update_node(self, id: int, patch: NodePatch) -> Node: ...
    def delete_node(self, id: int) -> None: ...

    # ── Edge CRUD ──────────────────────────────────────────────────────
    def create_edge(self, new_edge: NewEdge) -> Edge: ...
    def create_edges(self, new_edges: list[NewEdge]) -> list[Edge]: ...
    def get_edge(self, id: int) -> Optional[Edge]: ...
    def get_edge_by_uuid(self, uuid: bytes) -> Optional[Edge]: ...
    def update_edge(self, id: int, patch: EdgePatch) -> Edge: ...
    def delete_edge(self, id: int) -> None: ...
    def edges_of(self, node_id: int, direction: Direction) -> list[Edge]: ...

    # ── Index queries ──────────────────────────────────────────────────
    def list_nodes_by_kind(self, kind: str, limit: int, offset: int) -> list[Node]: ...
    def list_edges_by_kind(self, kind: str, limit: int, offset: int) -> list[Edge]: ...
    def list_recent(self, limit: int) -> list[Node]: ...

    # ── Traversal ──────────────────────────────────────────────────────
    def bfs(
        self,
        start_id: int,
        max_depth: int,
        direction: Direction,
        edge_kind: Optional[str] = ...,
    ) -> list[Node]: ...
    def dfs(
        self,
        start_id: int,
        max_depth: int,
        direction: Direction,
        edge_kind: Optional[str] = ...,
    ) -> list[Node]: ...
    def shortest_path(
        self, from_id: int, to_id: int, edge_kind: Optional[str] = ...
    ) -> Optional[list[int]]: ...
    def subgraph(self, root: int, depth: int, edge_kind: Optional[str] = ...) -> SubGraph: ...
    def neighbors(
        self,
        node_id: int,
        direction: Direction,
        edge_kind: Optional[str] = ...,
    ) -> list[Node]: ...

    # ── Full-text search ───────────────────────────────────────────────
    def search_fts(self, query: str, limit: int) -> list[ScoredNode]: ...

    # ── Vector embeddings (Phase 12 task 00079) ─────────────────────────
    def set_embedding(self, node_id: int, embedding: list[float]) -> None: ...
    def set_embeddings_batch(self, embeddings: list[tuple[int, list[float]]]) -> None: ...
    def get_embedding(self, node_id: int) -> Optional[list[float]]: ...
    def delete_embedding(self, node_id: int) -> None: ...
    def embedding_count(self) -> int: ...
    def vector_search(self, query: list[float], k: int) -> list[tuple[int, float]]: ...

__all__: Final[list[str]] = [
    "Drevo",
    "BloatReport",
    "CompactReport",
    "Direction",
    "Edge",
    "EdgePatch",
    "NewEdge",
    "NewNode",
    "Node",
    "NodePatch",
    "ScoredNode",
    "SubGraph",
    "ConflictError",
    "DrevoError",
    "DuplicateTitleError",
    "EdgeNotFoundError",
    "InvalidWeightError",
    "LockedError",
    "NeedsMigrationError",
    "NodeNotFoundError",
    "NotFoundError",
    "PanicError",
    "SerializationError",
    "StorageError",
    "__version__",
]
