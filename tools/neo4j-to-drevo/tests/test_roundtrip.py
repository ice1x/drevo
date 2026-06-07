"""Integration test — a realistic Neo4j-shaped graph migrated into a real
drevo backend, then verified through drevo's own query surface.

The use-case proof: build an IT-task-manager graph the way a Neo4j export
presents it (labelled nodes + typed relationships + mixed property
types), run it through `migrate`, and assert the data is queryable in
drevo afterwards — traversal, FTS, and property fidelity.
"""

from __future__ import annotations

import datetime
from collections.abc import Iterator

import drevo
from neo4j_to_drevo import SourceNode, SourceRelationship, migrate


class _ListSource:
    def __init__(self, nodes: list[SourceNode], rels: list[SourceRelationship]) -> None:
        self._nodes, self._rels = nodes, rels

    def nodes(self) -> Iterator[SourceNode]:
        return iter(self._nodes)

    def relationships(self) -> Iterator[SourceRelationship]:
        return iter(self._rels)


def _task_manager_graph() -> _ListSource:
    nodes = [
        SourceNode(
            id="u1",
            labels=["Person", "Engineer"],
            properties={"name": "Dana", "email": "dana@x.io"},
        ),
        SourceNode(id="u2", labels=["Person"], properties={"name": "Lee"}),
        SourceNode(id="p1", labels=["Project"], properties={"name": "Apollo", "code": "APL"}),
        SourceNode(
            id="t1",
            labels=["Task"],
            properties={"title": "Design schema", "status": "done", "points": 5},
        ),
        SourceNode(
            id="t2",
            labels=["Task"],
            properties={
                "title": "Build API",
                "status": "in_progress",
                "due": datetime.date(2026, 7, 1),
            },
        ),
        SourceNode(
            id="t3",
            labels=["Task"],
            properties={"title": "Write docs", "status": "todo", "tags": ["docs", "md"]},
        ),
    ]
    rels = [
        SourceRelationship(id="r1", type="OWNS", start="u1", end="p1", properties={}),
        SourceRelationship(id="r2", type="HAS_TASK", start="p1", end="t1", properties={}),
        SourceRelationship(id="r3", type="HAS_TASK", start="p1", end="t2", properties={}),
        SourceRelationship(id="r4", type="HAS_TASK", start="p1", end="t3", properties={}),
        SourceRelationship(id="r5", type="ASSIGNED_TO", start="t1", end="u1", properties={}),
        SourceRelationship(id="r6", type="ASSIGNED_TO", start="t2", end="u2", properties={}),
        SourceRelationship(
            id="r7", type="DEPENDS_ON", start="t2", end="t1", properties={"hard": True}
        ),
    ]
    return _ListSource(nodes, rels)


def test_full_graph_round_trips_into_drevo(drevo_db: drevo.Drevo) -> None:
    report = migrate(_task_manager_graph(), drevo_db)

    assert report.nodes_created == 6
    assert report.edges_created == 7
    assert not report.errors

    people = drevo_db.list_nodes_by_kind("Person:Engineer", limit=10, offset=0)
    assert {n.title for n in people} == {"Dana"}

    project = drevo_db.get_node_by_title("Apollo")
    assert project is not None
    task_nodes = drevo_db.neighbors(project.id, drevo.Direction.OUT, edge_kind="HAS_TASK")
    assert {n.title for n in task_nodes} == {"Design schema", "Build API", "Write docs"}


def test_property_types_survive_migration(drevo_db: drevo.Drevo) -> None:
    migrate(_task_manager_graph(), drevo_db)

    t2 = drevo_db.get_node_by_title("Build API")
    assert t2 is not None
    assert t2.properties["status"] == "in_progress"
    assert t2.properties["due"] == "2026-07-01"

    t3 = drevo_db.get_node_by_title("Write docs")
    assert t3 is not None
    assert t3.properties["tags"] == ["docs", "md"]


def test_relationship_property_and_direction_preserved(drevo_db: drevo.Drevo) -> None:
    migrate(_task_manager_graph(), drevo_db)

    t2 = drevo_db.get_node_by_title("Build API")
    t1 = drevo_db.get_node_by_title("Design schema")
    assert t2 is not None and t1 is not None

    deps = [e for e in drevo_db.edges_of(t2.id, drevo.Direction.OUT) if e.kind == "DEPENDS_ON"]
    assert len(deps) == 1
    assert deps[0].to_id == t1.id
    assert deps[0].properties["hard"] is True


def test_migrated_nodes_are_full_text_searchable(drevo_db: drevo.Drevo) -> None:
    migrate(_task_manager_graph(), drevo_db)

    hits = drevo_db.search_fts("docs", limit=10)
    assert "Write docs" in {h.node.title for h in hits}
