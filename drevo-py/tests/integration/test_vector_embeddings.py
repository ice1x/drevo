"""Integration tests for the vector embedding surface (Phase 12 task `00079`).

The unit tier exercises the bridge + helpers against the in-memory
backend; this tier defends the boundary that matters — embeddings written
through the real *disk* redb backend survive a close/reopen, and
`vector_search` works after the index is rebuilt from the durable store on
the reopened handle.
"""

from __future__ import annotations

import hashlib

import drevo
from drevo.rag import SimpleDocument, ingest_documents, vector_search


def _hash_embed(text: str, dim: int = 8) -> list[float]:
    """Deterministic stdlib embedder (SHA-256 → dim floats); no NumPy,
    no network, constant across runs."""
    digest = hashlib.sha256(text.encode("utf-8")).digest()
    return [(b - 128) / 128.0 for b in digest[:dim]]


def _embed(texts: list[str]) -> list[list[float]]:
    return [_hash_embed(t) for t in texts]


def test_embeddings_persist_across_reopen(tmp_db_path: str) -> None:
    with drevo.Drevo.open(tmp_db_path) as db:
        nodes = [db.create_node(drevo.NewNode(kind="doc", title=f"d{i}")) for i in range(3)]
        db.set_embeddings_batch([(n.id, [float(i), 1.0]) for i, n in enumerate(nodes)])
        assert db.embedding_count() == 3
        target_id = nodes[1].id

    # Reopen the same file — embeddings must still be there.
    with drevo.Drevo.open(tmp_db_path) as db:
        assert db.embedding_count() == 3
        assert db.get_embedding(target_id) == [1.0, 1.0]


def test_vector_search_after_reopen(tmp_db_path: str) -> None:
    with drevo.Drevo.open(tmp_db_path) as db:
        x = db.create_node(drevo.NewNode(kind="doc", title="x"))
        y = db.create_node(drevo.NewNode(kind="doc", title="y"))
        db.set_embeddings_batch([(x.id, [1.0, 0.0, 0.0]), (y.id, [0.0, 1.0, 0.0])])
        x_id = x.id

    with drevo.Drevo.open(tmp_db_path) as db:
        hits = db.vector_search([1.0, 0.0, 0.0], 1)
        assert hits[0][0] == x_id


def test_ingest_with_embedder_is_searchable_on_disk(disk_db: drevo.Drevo) -> None:
    docs = [
        SimpleDocument(page_content="vectors and similarity search"),
        SimpleDocument(page_content="graph traversal and shortest paths"),
        SimpleDocument(page_content="a recipe for sourdough bread"),
    ]
    ingest_documents(disk_db, docs, embedder=_embed)
    assert disk_db.embedding_count() == 3

    # Query with the exact embedding of the first doc → it ranks first.
    query = _hash_embed("vectors and similarity search")
    hits = vector_search(disk_db, query, k=1)
    assert hits[0].node.properties["text"] == "vectors and similarity search"


def test_graph_rag_retrieve_with_embedding_on_disk(disk_db: drevo.Drevo) -> None:
    # Two cited docs + one unrelated; retrieve_with_embedding seeds on the
    # nearest vector and expands one hop into the citation.
    paper = disk_db.create_node(
        drevo.NewNode(kind="doc", title="paper", properties={"text": "vector index design"})
    )
    cited = disk_db.create_node(
        drevo.NewNode(kind="doc", title="cited", properties={"text": "hnsw graph layers"})
    )
    disk_db.create_edge(drevo.NewEdge(from_id=paper.id, to_id=cited.id, kind="cites"))
    from drevo.rag import embed_and_store

    embed_and_store(disk_db, [paper, cited], _embed)

    from drevo.rag import Retriever

    ctx = Retriever(disk_db, hops=1).retrieve_with_embedding(
        _hash_embed("vector index design"), limit=1
    )
    assert [s.title for s in ctx.seeds] == ["paper"]
    assert any(n.title == "cited" for n in ctx.neighbours)
