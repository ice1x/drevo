"""`ingest_documents` — batched node creation from a Document list.

Pure-Python orchestration on top of `drevo.Drevo.create_node`. The
algorithm is intentionally simple (one Document → one Node, no
deduplication) so the failure modes are obvious to a reader without
diving into the Rust storage layer. RFC §8.2 specifies the shape;
RFC §10 Q-5 records the deferred dedup decision.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Callable

from ._document import Document

if TYPE_CHECKING:
    from .. import Drevo, NewNode, Node
else:
    # Runtime import — pulled from the parent package, which has set
    # up the cdylib + shim glue by the time `drevo.rag.ingest` loads.
    # `drevo.rag` is opt-in (not eagerly imported by drevo/__init__.py),
    # so this never produces a circular import.
    from .. import NewNode


_TITLE_MAX_CHARS = 200


@dataclass(frozen=True)
class IngestSchema:
    """Optional mapping from Document.metadata to Node fields.

    `kind_from` / `title_from` look up a metadata key and use its value
    as the node's `kind` / `title`. `property_map` renames metadata
    keys before they land in `Node.properties`. `edge_specs` is reserved
    for follow-up work — wiring inter-document edges from metadata —
    and is currently parsed but unused.
    """

    kind_from: str | None = None
    title_from: str | None = None
    property_map: dict[str, str] = field(default_factory=dict)
    edge_specs: list[Any] = field(default_factory=list)


def _truncate(text: str, limit: int) -> str:
    if len(text) <= limit:
        return text
    return text[:limit]


def _resolve_title(content: str, meta: dict[str, Any], schema: IngestSchema | None) -> str:
    if schema is not None and schema.title_from is not None:
        raw = meta.get(schema.title_from)
        if raw is not None:
            return str(raw)
    return _truncate(content.replace("\n", " "), _TITLE_MAX_CHARS) or "untitled"


def _resolve_kind(default_kind: str, meta: dict[str, Any], schema: IngestSchema | None) -> str:
    if schema is not None and schema.kind_from is not None:
        raw = meta.get(schema.kind_from)
        if raw is not None:
            return str(raw)
    return default_kind


def _resolve_properties(
    content: str, meta: dict[str, Any], schema: IngestSchema | None
) -> dict[str, Any]:
    props: dict[str, Any] = dict(meta)
    if schema is not None:
        for src, dst in schema.property_map.items():
            if src in props:
                props[dst] = props.pop(src)
    props["text"] = content
    return props


def ingest_documents(
    drevo: "Drevo",
    docs: list[Document],
    *,
    schema: IngestSchema | None = None,
    kind: str = "doc",
    embedder: Callable[[list[str]], list[list[float]]] | None = None,
) -> list["Node"]:
    """Ingest a list of duck-typed Documents as nodes.

    Each Document becomes one Node:
      - `kind = kind` (overridable by `schema.kind_from`)
      - `title = truncate(page_content, 200)` (overridable by `schema.title_from`)
      - `properties = doc.metadata | {"text": page_content}` (renamed per `schema.property_map`)

    If `embedder` is provided, its output for each doc is persisted **two
    ways** (Phase 12 task `00079`): under the property key ``"embedding"``
    as a `list[float]` (what the `00077` Cypher ``similar(...)`` predicate
    reads), *and* first-class in drevo's durable vector store via
    `Drevo.set_embeddings_batch` (what `drevo.rag.embedding.vector_search`
    and `Retriever.retrieve_with_embedding` query through the HNSW index).
    The two paths serve different query surfaces, so both are kept.

    Returns the created Nodes in the same order as `docs`. No
    deduplication — duplicate truncated titles raise
    `drevo.DuplicateTitleError`; callers that need dedup pass a
    `schema.title_from` resolving to a unique field.
    """

    if not docs:
        return []

    embeddings: list[list[float] | None]
    if embedder is None:
        embeddings = [None] * len(docs)
    else:
        embeddings = list(embedder([d.page_content for d in docs]))
        if len(embeddings) != len(docs):
            raise ValueError(
                f"embedder returned {len(embeddings)} vectors for "
                f"{len(docs)} documents — counts must match"
            )

    nodes: list[Node] = []
    first_class: list[tuple[int, list[float]]] = []
    for doc, embedding in zip(docs, embeddings):
        meta = dict(doc.metadata) if doc.metadata is not None else {}
        node_kind = _resolve_kind(kind, meta, schema)
        node_title = _resolve_title(doc.page_content, meta, schema)
        properties = _resolve_properties(doc.page_content, meta, schema)
        if embedding is not None:
            properties["embedding"] = list(embedding)
        new_node = NewNode(kind=node_kind, title=node_title, properties=properties)
        node = drevo.create_node(new_node)
        nodes.append(node)
        if embedding is not None:
            first_class.append((node.id, list(embedding)))

    if first_class:
        drevo.set_embeddings_batch(first_class)
    return nodes
