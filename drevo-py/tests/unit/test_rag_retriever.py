"""Unit tests for `drevo.rag.Retriever` (00118).

`Retriever.retrieve` dispatches by *seed type*:

  * `str` → `Drevo.search_fts`
  * `int` → `Drevo.get_node`
  * `uuid.UUID` → `Drevo.get_node_by_uuid(seed.bytes)`

We assert dispatch with a `MagicMock` (mocked storage) and behaviour
with the in-memory backend. The hop-1 cap + kind_filter pass-through
are covered with the real backend so we hit `expand_neighborhood`'s
algorithm without re-mocking it.
"""

from __future__ import annotations

import uuid
from unittest.mock import MagicMock

import pytest

import drevo
from drevo.rag import Context, Retriever

# ── Dispatch (mocked storage) ────────────────────────────────────────


def test_str_seed_dispatches_to_search_fts(mock_drevo: MagicMock) -> None:
    mock_drevo.search_fts.return_value = []
    Retriever(mock_drevo).retrieve("query string", limit=7)
    mock_drevo.search_fts.assert_called_once_with("query string", 7)


def test_int_seed_dispatches_to_get_node(mock_drevo: MagicMock) -> None:
    mock_drevo.get_node.return_value = None
    Retriever(mock_drevo).retrieve(42)
    mock_drevo.get_node.assert_called_once_with(42)


def test_uuid_seed_dispatches_to_get_node_by_uuid(mock_drevo: MagicMock) -> None:
    mock_drevo.get_node_by_uuid.return_value = None
    seed = uuid.UUID(bytes=bytes(range(16)))
    Retriever(mock_drevo).retrieve(seed)
    mock_drevo.get_node_by_uuid.assert_called_once_with(seed.bytes)


def test_bool_seed_raises_type_error(mock_drevo: MagicMock) -> None:
    """`bool ⊂ int` in Python, but the contract says int means node id —
    the dispatch table must reject `True`/`False` explicitly.
    """
    with pytest.raises(TypeError):
        Retriever(mock_drevo).retrieve(True)  # type: ignore[arg-type]


def test_unknown_seed_type_raises_type_error(mock_drevo: MagicMock) -> None:
    with pytest.raises(TypeError):
        Retriever(mock_drevo).retrieve(3.14)  # type: ignore[arg-type]


# ── Constructor validation ───────────────────────────────────────────


def test_negative_hops_raises_value_error(mock_drevo: MagicMock) -> None:
    with pytest.raises(ValueError):
        Retriever(mock_drevo, hops=-1)


def test_zero_max_nodes_raises_value_error(mock_drevo: MagicMock) -> None:
    with pytest.raises(ValueError):
        Retriever(mock_drevo, max_nodes=0)


# ── Real-backend retrieval behaviour ─────────────────────────────────


def test_retrieve_with_str_seed_returns_context(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    ctx = Retriever(drevo_db, hops=1).retrieve("a")
    assert isinstance(ctx, Context)


def test_retrieve_int_seed_returns_seed_as_first(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    ctx = Retriever(drevo_db, hops=0).retrieve(connected_chain["a"].id)
    assert [s.id for s in ctx.seeds] == [connected_chain["a"].id]


def test_retrieve_uuid_seed_round_trips(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    ctx = Retriever(drevo_db, hops=0).retrieve(connected_chain["b"].uuid)
    assert [s.title for s in ctx.seeds] == ["b"]


def test_retrieve_with_embedding_dispatches_to_vector_search(mock_drevo: MagicMock) -> None:
    """RFC §8.3 — vector seeds now resolve via `Drevo.vector_search`
    (Phase 12 task 00079). With no hits the context is empty and no
    expansion runs; the point is that the embedding + limit are forwarded
    to the bridge verbatim."""
    mock_drevo.vector_search.return_value = []
    ctx = Retriever(mock_drevo).retrieve_with_embedding([0.0, 1.0], limit=7)
    mock_drevo.vector_search.assert_called_once_with([0.0, 1.0], 7)
    assert ctx.seeds == []


def test_retrieve_missing_int_seed_returns_empty_seeds(
    drevo_db: drevo.Drevo,
) -> None:
    """A missing int seed surfaces as an empty `seeds` list, not as a
    raise — the retriever is intentionally lenient because FTS misses
    also return `[]`.
    """
    ctx = Retriever(drevo_db, hops=1).retrieve(999_999)
    assert ctx.seeds == []


def test_retrieve_with_kind_filter_does_not_include_excluded_kinds(
    drevo_db: drevo.Drevo, mixed_kind_neighbourhood
) -> None:
    root, _ = mixed_kind_neighbourhood
    ctx = Retriever(drevo_db, hops=1, kind_filter=["note"]).retrieve(root.id)
    assert all(n.kind == "note" for n in ctx.neighbours)


def test_context_stats_report_counts(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    ctx = Retriever(drevo_db, hops=2).retrieve(connected_chain["a"].id)
    assert ctx.stats.seed_count == len(ctx.seeds)
    assert ctx.stats.neighbour_count == len(ctx.neighbours)
    assert ctx.stats.edge_count == len(ctx.edges)
