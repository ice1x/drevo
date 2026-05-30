"""Unit tests for `drevo.rag.embedding` (Phase 12 task `00079`).

Covers the pure-Python embedding helpers — `Embedder`, `VectorHit`,
`embed_and_store`, `vector_search` — plus the now-implemented
`Retriever.retrieve_with_embedding` and `ingest_documents`' first-class
persistence. The `orthogonal_embedder` fixture gives one-hot vectors per
distinct string, so nearest-neighbour outcomes are deterministic.
"""

from __future__ import annotations

from typing import Callable

import pytest

import drevo
from drevo.rag import (
    Embedder,
    Retriever,
    SimpleDocument,
    VectorHit,
    embed_and_store,
    ingest_documents,
    vector_search,
)

Embed = Callable[[list[str]], list[list[float]]]


def _doc_node(db: drevo.Drevo, title: str, text: str | None = None) -> drevo.Node:
    props = {"text": text} if text is not None else {}
    return db.create_node(drevo.NewNode(kind="doc", title=title, properties=props))


# ── Embedder protocol ────────────────────────────────────────────────


def test_embedder_protocol_accepts_plain_function(det_embedder: Embed) -> None:
    assert isinstance(det_embedder, Embedder)


# ── embed_and_store ──────────────────────────────────────────────────


def test_embed_and_store_persists_one_vector_per_node(
    drevo_db: drevo.Drevo, det_embedder: Embed
) -> None:
    nodes = [_doc_node(drevo_db, f"n{i}", text=f"body {i}") for i in range(4)]
    stored = embed_and_store(drevo_db, nodes, det_embedder)
    assert stored == 4
    assert drevo_db.embedding_count() == 4
    # Each persisted vector matches the embedder's output for that text.
    expected = det_embedder(["body 0"])[0]
    assert drevo_db.get_embedding(nodes[0].id) == pytest.approx(expected)


def test_embed_and_store_empty_is_noop(drevo_db: drevo.Drevo, det_embedder: Embed) -> None:
    assert embed_and_store(drevo_db, [], det_embedder) == 0
    assert drevo_db.embedding_count() == 0


def test_embed_and_store_falls_back_to_title_without_text(
    drevo_db: drevo.Drevo, det_embedder: Embed
) -> None:
    node = _doc_node(drevo_db, "just-a-title")  # no text property
    embed_and_store(drevo_db, [node], det_embedder)
    assert drevo_db.get_embedding(node.id) == pytest.approx(det_embedder(["just-a-title"])[0])


def test_embed_and_store_custom_text_of(drevo_db: drevo.Drevo, det_embedder: Embed) -> None:
    node = _doc_node(drevo_db, "title", text="body")
    embed_and_store(drevo_db, [node], det_embedder, text_of=lambda n: n.kind)
    assert drevo_db.get_embedding(node.id) == pytest.approx(det_embedder(["doc"])[0])


def test_embed_and_store_size_mismatch_raises(drevo_db: drevo.Drevo) -> None:
    node = _doc_node(drevo_db, "a", text="x")

    def bad_embedder(texts: list[str]) -> list[list[float]]:
        return []  # wrong count

    with pytest.raises(ValueError):
        embed_and_store(drevo_db, [node], bad_embedder)


# ── vector_search ────────────────────────────────────────────────────


def test_vector_search_with_raw_vector(drevo_db: drevo.Drevo, orthogonal_embedder: Embed) -> None:
    nodes = [_doc_node(drevo_db, t, text=t) for t in ("alpha", "beta", "gamma")]
    embed_and_store(drevo_db, nodes, orthogonal_embedder)
    query = orthogonal_embedder(["beta"])[0]
    hits = vector_search(drevo_db, query, k=1)
    assert len(hits) == 1
    assert isinstance(hits[0], VectorHit)
    assert hits[0].node.title == "beta"
    assert hits[0].similarity == pytest.approx(1.0, abs=1e-6)


def test_vector_search_with_str_query_uses_embedder(
    drevo_db: drevo.Drevo, orthogonal_embedder: Embed
) -> None:
    nodes = [_doc_node(drevo_db, t, text=t) for t in ("alpha", "beta", "gamma")]
    embed_and_store(drevo_db, nodes, orthogonal_embedder)
    hits = vector_search(drevo_db, "gamma", embedder=orthogonal_embedder, k=1)
    assert hits[0].node.title == "gamma"


def test_vector_search_str_query_without_embedder_raises(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(ValueError):
        vector_search(drevo_db, "needs embedder")


def test_vector_search_rejects_non_positive_k(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(ValueError):
        vector_search(drevo_db, [1.0, 0.0], k=0)


def test_vector_search_empty_store_returns_empty(drevo_db: drevo.Drevo) -> None:
    assert vector_search(drevo_db, [1.0, 0.0], k=3) == []


# ── ingest_documents first-class persistence ─────────────────────────


def test_ingest_documents_persists_first_class_embeddings(
    drevo_db: drevo.Drevo, det_embedder: Embed
) -> None:
    docs = [SimpleDocument(page_content=f"doc body {i}") for i in range(3)]
    nodes = ingest_documents(drevo_db, docs, embedder=det_embedder)
    # First-class store populated...
    assert drevo_db.embedding_count() == 3
    # ...and the property is still there for the Cypher similar() path.
    assert "embedding" in nodes[0].properties


def test_ingest_documents_without_embedder_stores_no_vectors(drevo_db: drevo.Drevo) -> None:
    ingest_documents(drevo_db, [SimpleDocument(page_content="x")])
    assert drevo_db.embedding_count() == 0


# ── Retriever.retrieve_with_embedding ────────────────────────────────


def test_retrieve_with_embedding_resolves_seed_and_expands(
    drevo_db: drevo.Drevo, orthogonal_embedder: Embed
) -> None:
    # alpha cites beta; a query embedding near alpha should seed on alpha
    # and surface beta as a neighbour.
    alpha = _doc_node(drevo_db, "alpha", text="alpha")
    beta = _doc_node(drevo_db, "beta", text="beta")
    drevo_db.create_edge(drevo.NewEdge(from_id=alpha.id, to_id=beta.id, kind="cites"))
    embed_and_store(drevo_db, [alpha, beta], orthogonal_embedder)

    retriever = Retriever(drevo_db, hops=1)
    query = orthogonal_embedder(["alpha"])[0]
    ctx = retriever.retrieve_with_embedding(query, limit=1)

    assert [s.title for s in ctx.seeds] == ["alpha"]
    assert any(n.title == "beta" for n in ctx.neighbours)
