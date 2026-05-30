"""Type stubs for `drevo.rag` — Phase 16 task `00117`.

Hand-authored. The runtime `rag/__init__.py` re-exports symbols from
the sibling `.py` modules; these stubs mirror that surface so
`mypy --strict drevo/` resolves every name without importing the
runtime (which would in turn pull in the PyO3 cdylib).
"""

from __future__ import annotations

import uuid
from collections.abc import Callable, Sequence
from typing import Any, Final, Optional, Protocol, Union, runtime_checkable

from .. import Drevo, Edge, Node

# ── Document protocol ─────────────────────────────────────────────────

@runtime_checkable
class Document(Protocol):
    page_content: str
    metadata: dict[str, Any]

class SimpleDocument:
    page_content: str
    metadata: dict[str, Any]

    def __init__(
        self,
        page_content: str,
        metadata: Optional[dict[str, Any]] = ...,
    ) -> None: ...

# ── Ingest ────────────────────────────────────────────────────────────

class IngestSchema:
    kind_from: Optional[str]
    title_from: Optional[str]
    property_map: dict[str, str]
    edge_specs: list[Any]

    def __init__(
        self,
        *,
        kind_from: Optional[str] = ...,
        title_from: Optional[str] = ...,
        property_map: Optional[dict[str, str]] = ...,
        edge_specs: Optional[list[Any]] = ...,
    ) -> None: ...

def ingest_documents(
    drevo: Drevo,
    docs: list[Document],
    *,
    schema: Optional[IngestSchema] = ...,
    kind: str = ...,
    embedder: Optional[Callable[[list[str]], list[list[float]]]] = ...,
) -> list[Node]: ...

# ── Neighborhood ──────────────────────────────────────────────────────

class Neighborhood:
    nodes: list[Node]
    edges: list[Edge]
    hops_used: int

def expand_neighborhood(
    drevo: Drevo,
    node_uuid: Union[uuid.UUID, bytes, int],
    *,
    hops: int = ...,
    kind_filter: Optional[list[str]] = ...,
    max_nodes: int = ...,
) -> Neighborhood: ...

# ── Retriever + Context ───────────────────────────────────────────────

class ContextStats:
    seed_count: int
    neighbour_count: int
    edge_count: int
    hops_used: int

class Context:
    seeds: list[Node]
    neighbours: list[Node]
    edges: list[Edge]
    stats: ContextStats

    def to_text(self, *, format: str = ...) -> str: ...

class Retriever:
    def __init__(
        self,
        drevo: Drevo,
        *,
        hops: int = ...,
        kind_filter: Optional[list[str]] = ...,
        max_nodes: int = ...,
    ) -> None: ...
    def retrieve(
        self,
        seed: Union[str, int, uuid.UUID],
        *,
        limit: int = ...,
    ) -> Context: ...
    def retrieve_with_embedding(
        self,
        embedding: list[float],
        *,
        limit: int = ...,
    ) -> Context: ...

# ── Embedding helpers (Phase 12 task 00079) ───────────────────────────

@runtime_checkable
class Embedder(Protocol):
    def __call__(self, texts: list[str]) -> list[list[float]]: ...

class VectorHit:
    node: Node
    distance: float
    similarity: float

    def __init__(self, node: Node, distance: float, similarity: float) -> None: ...

def embed_and_store(
    drevo: Drevo,
    nodes: Sequence[Node],
    embedder: Callable[[list[str]], list[list[float]]],
    *,
    text_of: Optional[Callable[[Node], str]] = ...,
) -> int: ...
def vector_search(
    drevo: Drevo,
    query: Union[str, Sequence[float]],
    *,
    embedder: Optional[Callable[[list[str]], list[list[float]]]] = ...,
    k: int = ...,
) -> list[VectorHit]: ...

# ── MMR reranker ──────────────────────────────────────────────────────

class MMRReranker:
    lambda_: float

    def __init__(self, *, lambda_: float = ...) -> None: ...
    def rerank(
        self,
        candidates: Sequence[Any],
        *,
        embedder: Callable[[list[str]], list[list[float]]],
        k: int,
    ) -> list[Any]: ...

# ── Module surface ────────────────────────────────────────────────────

__all__: Final[list[str]] = [
    "Context",
    "ContextStats",
    "Document",
    "Embedder",
    "IngestSchema",
    "MMRReranker",
    "Neighborhood",
    "Retriever",
    "SimpleDocument",
    "VectorHit",
    "embed_and_store",
    "expand_neighborhood",
    "ingest_documents",
    "vector_search",
]
