"""`drevo.rag.embedding` — embedding integration helpers (Phase 12 task `00079`).

The capstone of Phase 12: pure-Python helpers that wire a user-supplied
*embedder* to drevo's first-class vector storage (`00078`) and HNSW search
(`00076`). Phases `00075`–`00078` built the durable embedding store and the
approximate-nearest-neighbour index in Rust and exposed them on the
`Drevo` handle (`set_embedding` / `set_embeddings_batch` / `get_embedding`
/ `delete_embedding` / `embedding_count` / `vector_search`); this module is
the ergonomic Python layer the SDK and the FastMCP server call.

Like the rest of `drevo.rag`, this is pure-Python composition over the
PyO3 `Drevo` methods — no FFI, testable with `pytest` alone.

Public surface:
- `Embedder` — structural protocol for a batch text→vector callable
  (the same shape LangChain / LlamaIndex / Haystack embedders expose).
- `VectorHit` — frozen dataclass: a matched `Node` plus its raw index
  `distance` and a convenience `similarity` score.
- `embed_and_store` — embed the text of existing nodes and persist the
  vectors first-class in one batched write.
- `vector_search` — embed a query (or accept a raw vector) and return the
  `k` nearest nodes as `VectorHit`s.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Callable, Protocol, Sequence, runtime_checkable

if TYPE_CHECKING:
    from .. import Drevo, Node


@runtime_checkable
class Embedder(Protocol):
    """A batch text→vector callable.

    `embedder(["a", "b"])` returns `[[...], [...]]` — one float vector per
    input string, in order. This is the structural shape every major
    framework's embedder already satisfies, so users pass theirs directly
    without an adapter.
    """

    def __call__(self, texts: list[str]) -> list[list[float]]: ...


# A plain function with the same signature is also accepted everywhere an
# `Embedder` is — kept as an alias for call sites that prefer the verb.
EmbedderFn = Callable[[list[str]], list[list[float]]]


@dataclass(frozen=True)
class VectorHit:
    """One result of a `vector_search`.

    `distance` is the raw index metric (cosine distance by default —
    smaller is nearer). `similarity` is the convenience inverse
    `1 - distance`, clamped to `[0, 1]`, so callers ranking by "higher is
    better" do not have to flip the sign. The dataclass is frozen so a hit
    can be cached / hashed by downstream pipelines.
    """

    node: "Node"
    distance: float
    similarity: float


def _default_text_of(node: "Node") -> str:
    """Pick the text to embed for a node: its ``text`` property if present
    (that is where `ingest_documents` stores the document body), else the
    title."""
    text = node.properties.get("text")
    if isinstance(text, str) and text:
        return text
    return node.title


def _similarity_from_distance(distance: float) -> float:
    sim = 1.0 - distance
    if sim < 0.0:
        return 0.0
    if sim > 1.0:
        return 1.0
    return sim


def embed_and_store(
    drevo: "Drevo",
    nodes: Sequence["Node"],
    embedder: EmbedderFn,
    *,
    text_of: Callable[["Node"], str] | None = None,
) -> int:
    """Embed the text of each node and persist the vectors first-class.

    The text for each node is chosen by `text_of` (default: the ``text``
    property, falling back to the title). All vectors are written in a
    single batched call (`Drevo.set_embeddings_batch`), so on the redb
    backend the whole set commits in one transaction.

    Returns the number of embeddings stored. A no-op (returns 0) for an
    empty `nodes`.

    Raises `ValueError` if the embedder returns a vector count that does
    not match `len(nodes)`.
    """
    if not nodes:
        return 0
    pick = text_of if text_of is not None else _default_text_of
    texts = [pick(n) for n in nodes]
    vectors = list(embedder(texts))
    if len(vectors) != len(nodes):
        raise ValueError(
            f"embedder returned {len(vectors)} vectors for {len(nodes)} "
            f"nodes — counts must match"
        )
    pairs = [(n.id, list(v)) for n, v in zip(nodes, vectors)]
    drevo.set_embeddings_batch(pairs)
    return len(pairs)


def vector_search(
    drevo: "Drevo",
    query: "str | Sequence[float]",
    *,
    embedder: EmbedderFn | None = None,
    k: int = 10,
) -> list[VectorHit]:
    """Find the `k` nearest stored nodes to `query`.

    `query` is either a raw embedding (`Sequence[float]`) or a `str`, in
    which case `embedder` must be supplied to turn it into a vector. The
    search runs against drevo's first-class HNSW index
    (`Drevo.vector_search`); each returned node id is resolved back to a
    `Node` and wrapped in a `VectorHit`. Node ids the index returns that
    no longer resolve (deleted between index build and lookup) are
    skipped, so the result may be shorter than `k`.

    Results are ordered nearest first. Raises `ValueError` for a
    non-positive `k`, or a `str` query with no `embedder`.
    """
    if k < 1:
        raise ValueError(f"vector_search: k must be ≥ 1 (got {k})")

    if isinstance(query, str):
        if embedder is None:
            raise ValueError(
                "vector_search: a str query needs an `embedder` to turn it "
                "into a vector; pass embedder=... or a raw list[float] query"
            )
        embedding = list(embedder([query])[0])
    else:
        embedding = [float(x) for x in query]

    hits = drevo.vector_search(embedding, k)
    results: list[VectorHit] = []
    for node_id, distance in hits:
        node = drevo.get_node(node_id)
        if node is None:
            continue
        results.append(
            VectorHit(
                node=node,
                distance=distance,
                similarity=_similarity_from_distance(distance),
            )
        )
    return results
