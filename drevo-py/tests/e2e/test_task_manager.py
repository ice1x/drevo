"""End-to-end scenario: IT task / sprint manager.

Mirrors ``tests/scenario_task_manager.rs`` at the Python surface.

Domain model:
    Node kinds: project, sprint, task, person, label
    Edge kinds: contains, assigned_to, depends_on, blocked_by, tagged_with

The scenario builds a one-project / one-sprint board with three tasks
in a small dependency chain, plus two people and two labels. The
assertions exercise the kanban-style projections an IT lead actually
runs against the graph (open tasks per person, dependency chains,
label filters), and check that the dependency-aware projection
survives a close + reopen cycle.
"""

from __future__ import annotations

import drevo

from .conftest import must_get_node


def _new(kind: str, title: str, body: str = "", **props: object) -> drevo.NewNode:
    return drevo.NewNode(
        kind=kind, title=title, body=body, properties=dict(props) if props else None
    )


def _edge(from_id: int, to_id: int, kind: str, weight: float = 1.0) -> drevo.NewEdge:
    return drevo.NewEdge(from_id=from_id, to_id=to_id, kind=kind, weight=weight)


def _build_board(db: drevo.Drevo) -> dict[str, drevo.Node]:
    n: dict[str, drevo.Node] = {}

    n["project"] = db.create_node(_new("project", "platform-revamp", "Q1 platform work"))
    n["sprint"] = db.create_node(
        _new("sprint", "sprint-2026-w05", goal="Stand up the new ingest path")
    )
    n["t1"] = db.create_node(_new("task", "design-schema", status="done", priority=3))
    n["t2"] = db.create_node(_new("task", "build-ingester", status="in_progress", priority=2))
    n["t3"] = db.create_node(_new("task", "wire-dashboard", status="todo", priority=1))
    n["alice"] = db.create_node(_new("person", "alice", role="backend"))
    n["bob"] = db.create_node(_new("person", "bob", role="frontend"))
    n["label_blocker"] = db.create_node(_new("label", "blocker", color="red"))
    n["label_nice_to_have"] = db.create_node(_new("label", "nice-to-have", color="green"))

    # Project → sprint → tasks
    db.create_edge(_edge(n["project"].id, n["sprint"].id, "contains"))
    db.create_edge(_edge(n["sprint"].id, n["t1"].id, "contains"))
    db.create_edge(_edge(n["sprint"].id, n["t2"].id, "contains"))
    db.create_edge(_edge(n["sprint"].id, n["t3"].id, "contains"))

    # Assignments
    db.create_edge(_edge(n["t1"].id, n["alice"].id, "assigned_to"))
    db.create_edge(_edge(n["t2"].id, n["alice"].id, "assigned_to"))
    db.create_edge(_edge(n["t3"].id, n["bob"].id, "assigned_to"))

    # Dependency chain: t1 → t2 → t3
    db.create_edge(_edge(n["t2"].id, n["t1"].id, "depends_on"))
    db.create_edge(_edge(n["t3"].id, n["t2"].id, "depends_on"))
    db.create_edge(_edge(n["t3"].id, n["t2"].id, "blocked_by"))

    # Labels
    db.create_edge(_edge(n["t2"].id, n["label_blocker"].id, "tagged_with"))
    db.create_edge(_edge(n["t3"].id, n["label_nice_to_have"].id, "tagged_with"))

    return n


def test_board_census_matches_definition(disk_db: drevo.Drevo) -> None:
    _build_board(disk_db)
    assert len(disk_db.list_nodes_by_kind("project", limit=10, offset=0)) == 1
    assert len(disk_db.list_nodes_by_kind("sprint", limit=10, offset=0)) == 1
    assert len(disk_db.list_nodes_by_kind("task", limit=10, offset=0)) == 3
    assert len(disk_db.list_nodes_by_kind("person", limit=10, offset=0)) == 2
    assert len(disk_db.list_nodes_by_kind("label", limit=10, offset=0)) == 2


def test_alice_open_assignments_returns_two_tasks(disk_db: drevo.Drevo) -> None:
    """Inbound ``assigned_to`` edges on Alice are her board column."""
    n = _build_board(disk_db)
    inbound = disk_db.edges_of(n["alice"].id, drevo.Direction.IN)
    assigned_titles = {
        must_get_node(disk_db, e.from_id).title for e in inbound if e.kind == "assigned_to"
    }
    assert assigned_titles == {"design-schema", "build-ingester"}


def test_dependency_chain_walked_by_shortest_path(disk_db: drevo.Drevo) -> None:
    """``shortest_path(t3 → t1)`` over ``depends_on`` resolves the
    end-to-end critical chain.
    """
    n = _build_board(disk_db)
    path = disk_db.shortest_path(n["t3"].id, n["t1"].id, edge_kind="depends_on")
    assert path == [n["t3"].id, n["t2"].id, n["t1"].id]


def test_blocker_label_lookup_returns_only_t2(disk_db: drevo.Drevo) -> None:
    """Following the ``tagged_with`` edge backward from the blocker
    label returns exactly the in-progress task.
    """
    n = _build_board(disk_db)
    inbound = disk_db.edges_of(n["label_blocker"].id, drevo.Direction.IN)
    tagged_titles = {
        must_get_node(disk_db, e.from_id).title for e in inbound if e.kind == "tagged_with"
    }
    assert tagged_titles == {"build-ingester"}


def test_subgraph_of_sprint_pulls_full_board(disk_db: drevo.Drevo) -> None:
    """A 3-hop subgraph from the sprint includes every task, every
    assignee, and every label that participates in the sprint.
    """
    n = _build_board(disk_db)
    sg = disk_db.subgraph(n["sprint"].id, depth=3)
    kinds = {node.kind for node in sg.nodes}
    assert {"sprint", "task", "person", "label"}.issubset(kinds)


def test_board_round_trips_through_reopen(tmp_db_path: str) -> None:
    """Persist mid-sprint, reopen, the dependency projection still
    points the lead to the right next task.
    """
    with drevo.Drevo.open(tmp_db_path) as db:
        n = _build_board(db)
        t3_id = n["t3"].id
        t1_id = n["t1"].id
    with drevo.Drevo.open(tmp_db_path) as db2:
        path = db2.shortest_path(t3_id, t1_id, edge_kind="depends_on")
        assert path is not None
        assert path[0] == t3_id
        assert path[-1] == t1_id
