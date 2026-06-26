"""Batch-boundary tests for the migration engine.

The engine folds node/edge creation into chunks, each committed in one
``Drevo.create_nodes`` / ``create_edges`` transaction. These force several
chunks via a tiny ``_BATCH_SIZE`` and assert the result is identical to a
single chunk: every node mapped, every edge wired across chunk boundaries,
and title disambiguation still holds across chunks.
"""

from __future__ import annotations

from collections.abc import Iterator

import drevo
import pytest

from neo4j_to_drevo import MigrationConfig, SourceNode, SourceRelationship, migrate
from neo4j_to_drevo import _engine


class ListSource:
    """Minimal in-memory ``GraphSource`` (structural — duck-typed)."""

    def __init__(self, nodes: list[SourceNode], rels: list[SourceRelationship]) -> None:
        self._nodes = nodes
        self._rels = rels

    def nodes(self) -> Iterator[SourceNode]:
        yield from self._nodes

    def relationships(self) -> Iterator[SourceRelationship]:
        yield from self._rels


def test_migrate_spans_multiple_batches(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(_engine, "_BATCH_SIZE", 2)  # 5 items -> 3 node chunks
    n = 5
    nodes = [
        SourceNode(id=f"n{i}", labels=["Person"], properties={"name": f"p{i}"}) for i in range(n)
    ]
    # A chain n0 -> n1 -> ... -> n4 so the path crosses every chunk boundary.
    rels = [
        SourceRelationship(id=f"r{i}", type="KNOWS", start=f"n{i}", end=f"n{i + 1}", properties={})
        for i in range(n - 1)
    ]
    db = drevo.Drevo.open_in_memory()
    report = migrate(ListSource(nodes, rels), db, config=MigrationConfig())

    assert report.nodes_created == n
    assert report.edges_created == n - 1
    assert len(report.id_map) == n
    for drevo_id in report.id_map.values():
        assert db.get_node(drevo_id) is not None
    # Edges wired across chunk boundaries: a full path n0 -> n4 exists.
    path = db.shortest_path(report.id_map["n0"], report.id_map["n4"])
    assert path is not None
    assert len(path) == n


def test_batched_duplicate_titles_disambiguated_across_chunks(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(_engine, "_BATCH_SIZE", 1)  # each node is its own chunk
    # Same source 'name' on every node — the engine must disambiguate titles
    # so no chunk hits DuplicateTitleError (titles are globally unique in drevo).
    nodes = [
        SourceNode(id=f"n{i}", labels=["Person"], properties={"name": "same"}) for i in range(4)
    ]
    db = drevo.Drevo.open_in_memory()
    report = migrate(ListSource(nodes, []), db)
    assert report.nodes_created == 4
    assert len(report.id_map) == 4
