"""Unit tests for the `Drevo` vector-embedding bridge (Phase 12 task `00079`).

These cover the PyO3 methods that expose the `00078` durable vector store
to Python — `set_embedding` / `set_embeddings_batch` / `get_embedding` /
`delete_embedding` / `embedding_count` / `vector_search` — against the
in-memory backend (the canonical unit stand-in). Disk durability lives in
the integration tier.
"""

from __future__ import annotations

import pytest

import drevo


def _node(db: drevo.Drevo, title: str) -> drevo.Node:
    return db.create_node(drevo.NewNode(kind="doc", title=title))


def test_set_then_get_embedding_round_trips(drevo_db: drevo.Drevo) -> None:
    node = _node(drevo_db, "a")
    drevo_db.set_embedding(node.id, [0.5, -0.25, 2.0])
    got = drevo_db.get_embedding(node.id)
    assert got == pytest.approx([0.5, -0.25, 2.0])


def test_get_embedding_missing_returns_none(drevo_db: drevo.Drevo) -> None:
    node = _node(drevo_db, "a")
    assert drevo_db.get_embedding(node.id) is None


def test_set_embedding_on_missing_node_raises(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(drevo.NodeNotFoundError):
        drevo_db.set_embedding(9999, [1.0, 2.0])


def test_embedding_count_tracks_inserts_and_deletes(drevo_db: drevo.Drevo) -> None:
    assert drevo_db.embedding_count() == 0
    a, b = _node(drevo_db, "a"), _node(drevo_db, "b")
    drevo_db.set_embedding(a.id, [1.0, 0.0])
    drevo_db.set_embedding(b.id, [0.0, 1.0])
    assert drevo_db.embedding_count() == 2
    drevo_db.delete_embedding(a.id)
    assert drevo_db.embedding_count() == 1


def test_set_embeddings_batch_inserts_all(drevo_db: drevo.Drevo) -> None:
    nodes = [_node(drevo_db, f"n{i}") for i in range(5)]
    drevo_db.set_embeddings_batch([(n.id, [float(i), 0.0]) for i, n in enumerate(nodes)])
    assert drevo_db.embedding_count() == 5
    assert drevo_db.get_embedding(nodes[3].id) == pytest.approx([3.0, 0.0])


def test_set_embeddings_batch_all_or_nothing_on_bad_id(drevo_db: drevo.Drevo) -> None:
    a = _node(drevo_db, "a")
    with pytest.raises(drevo.NodeNotFoundError):
        drevo_db.set_embeddings_batch([(a.id, [1.0, 0.0]), (8888, [0.0, 1.0])])
    # The whole batch is rejected before any write.
    assert drevo_db.embedding_count() == 0


def test_delete_node_cascades_embedding(drevo_db: drevo.Drevo) -> None:
    node = _node(drevo_db, "doomed")
    drevo_db.set_embedding(node.id, [1.0, 2.0, 3.0])
    assert drevo_db.embedding_count() == 1
    drevo_db.delete_node(node.id)
    assert drevo_db.get_embedding(node.id) is None
    assert drevo_db.embedding_count() == 0


def test_vector_search_returns_nearest_first(drevo_db: drevo.Drevo) -> None:
    # One-hot embeddings: the query [1,0,0] is nearest to node `x`.
    x = _node(drevo_db, "x")
    y = _node(drevo_db, "y")
    z = _node(drevo_db, "z")
    drevo_db.set_embeddings_batch(
        [(x.id, [1.0, 0.0, 0.0]), (y.id, [0.0, 1.0, 0.0]), (z.id, [0.0, 0.0, 1.0])]
    )
    hits = drevo_db.vector_search([1.0, 0.0, 0.0], 1)
    assert len(hits) == 1
    node_id, distance = hits[0]
    assert node_id == x.id
    assert distance == pytest.approx(0.0, abs=1e-6)


def test_vector_search_empty_store_returns_empty(drevo_db: drevo.Drevo) -> None:
    assert drevo_db.vector_search([1.0, 0.0], 5) == []


def test_vector_search_dimension_mismatch_raises(drevo_db: drevo.Drevo) -> None:
    a, b = _node(drevo_db, "a"), _node(drevo_db, "b")
    drevo_db.set_embedding(a.id, [1.0, 0.0])
    drevo_db.set_embedding(b.id, [1.0, 0.0, 0.0])
    with pytest.raises(ValueError):
        drevo_db.vector_search([1.0, 0.0], 2)
