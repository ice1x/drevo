"""Unit tests for the source-agnostic migration engine.

The engine consumes any object satisfying the `GraphSource` protocol
(yields `SourceNode` / `SourceRelationship` records), so the whole
mapping + wiring + reporting path is exercised here with an in-memory
`FakeSource` and **no live Neo4j**. Storage is the real in-memory drevo
backend (`drevo_db` fixture) so a mapping bug surfaces as a wrong stored
row, not a mock assertion that drifts from reality.
"""

from __future__ import annotations

from collections.abc import Iterator
from typing import Any

import pytest

import drevo
from neo4j_to_drevo import (
    MigrationConfig,
    MigrationReport,
    SourceNode,
    SourceRelationship,
    migrate,
)


class FakeSource:
    """In-memory `GraphSource` built from explicit record lists."""

    def __init__(
        self,
        nodes: list[SourceNode] | None = None,
        relationships: list[SourceRelationship] | None = None,
    ) -> None:
        self._nodes = list(nodes or [])
        self._rels = list(relationships or [])

    def nodes(self) -> Iterator[SourceNode]:
        return iter(self._nodes)

    def relationships(self) -> Iterator[SourceRelationship]:
        return iter(self._rels)


# ── basic node mapping ───────────────────────────────────────────────


def test_empty_source_yields_empty_report(drevo_db: drevo.Drevo) -> None:
    report = migrate(FakeSource(), drevo_db)
    assert isinstance(report, MigrationReport)
    assert report.nodes_created == 0
    assert report.edges_created == 0
    assert report.id_map == {}


def test_single_node_maps_label_to_kind_and_preserves_properties(
    drevo_db: drevo.Drevo, fake: Any
) -> None:
    bio = fake.sentence()
    src = FakeSource(
        nodes=[SourceNode(id="4:abc:1", labels=["Person"], properties={"name": "Ada", "bio": bio})]
    )

    report = migrate(src, drevo_db)

    assert report.nodes_created == 1
    node = drevo_db.get_node(report.id_map["4:abc:1"])
    assert node is not None
    assert node.kind == "Person"
    assert node.title == "Ada"
    assert node.properties["bio"] == bio


def test_multi_label_node_joins_into_kind_and_records_full_label_set(
    drevo_db: drevo.Drevo,
) -> None:
    src = FakeSource(
        nodes=[SourceNode(id="n1", labels=["Person", "Employee"], properties={"name": "Bob"})]
    )

    report = migrate(src, drevo_db)
    node = drevo_db.get_node(report.id_map["n1"])

    assert node is not None
    assert node.kind == "Person:Employee"
    assert node.properties["_labels"] == ["Person", "Employee"]


def test_label_less_node_falls_back_to_default_kind(drevo_db: drevo.Drevo) -> None:
    src = FakeSource(nodes=[SourceNode(id="n1", labels=[], properties={"name": "x"})])

    report = migrate(src, drevo_db, config=MigrationConfig(default_kind="thing"))
    node = drevo_db.get_node(report.id_map["n1"])

    assert node is not None
    assert node.kind == "thing"


# ── title resolution + uniqueness ────────────────────────────────────


def test_title_property_precedence(drevo_db: drevo.Drevo) -> None:
    src = FakeSource(
        nodes=[
            SourceNode(id="n1", labels=["Doc"], properties={"name": "by-name", "title": "by-title"})
        ]
    )
    report = migrate(src, drevo_db)
    node = drevo_db.get_node(report.id_map["n1"])
    assert node is not None
    assert node.title == "by-title"


def test_node_without_title_property_gets_synthesized_unique_title(
    drevo_db: drevo.Drevo,
) -> None:
    src = FakeSource(nodes=[SourceNode(id="4:graph:7", labels=["Event"], properties={"when": 1})])
    report = migrate(src, drevo_db)
    node = drevo_db.get_node(report.id_map["4:graph:7"])
    assert node is not None
    assert "4:graph:7" in node.title
    assert node.title


def test_duplicate_source_titles_are_disambiguated_no_collision(
    drevo_db: drevo.Drevo,
) -> None:
    src = FakeSource(
        nodes=[
            SourceNode(id="a1", labels=["Person"], properties={"name": "Alice"}),
            SourceNode(id="a2", labels=["Person"], properties={"name": "Alice"}),
        ]
    )

    report = migrate(src, drevo_db)

    assert report.nodes_created == 2
    t1 = drevo_db.get_node(report.id_map["a1"]).title  # type: ignore[union-attr]
    t2 = drevo_db.get_node(report.id_map["a2"]).title  # type: ignore[union-attr]
    assert t1 != t2
    assert t1 == "Alice"
    assert "a2" in t2


# ── relationships ────────────────────────────────────────────────────


def test_relationship_wires_endpoints_via_id_map(drevo_db: drevo.Drevo) -> None:
    src = FakeSource(
        nodes=[
            SourceNode(id="p", labels=["Person"], properties={"name": "Ann"}),
            SourceNode(id="c", labels=["Company"], properties={"name": "Acme"}),
        ],
        relationships=[
            SourceRelationship(
                id="r1", type="WORKS_AT", start="p", end="c", properties={"since": 2020}
            )
        ],
    )

    report = migrate(src, drevo_db)

    assert report.edges_created == 1
    pid = report.id_map["p"]
    cid = report.id_map["c"]
    out_edges = drevo_db.edges_of(pid, drevo.Direction.OUT)
    assert len(out_edges) == 1
    edge = out_edges[0]
    assert edge.from_id == pid
    assert edge.to_id == cid
    assert edge.kind == "WORKS_AT"
    assert edge.properties["since"] == 2020


def test_relationship_weight_taken_from_property_else_default(drevo_db: drevo.Drevo) -> None:
    src = FakeSource(
        nodes=[
            SourceNode(id="a", labels=["N"], properties={"name": "a"}),
            SourceNode(id="b", labels=["N"], properties={"name": "b"}),
            SourceNode(id="d", labels=["N"], properties={"name": "d"}),
        ],
        relationships=[
            SourceRelationship(
                id="r1", type="LINK", start="a", end="b", properties={"weight": 3.5}
            ),
            SourceRelationship(id="r2", type="LINK", start="a", end="d", properties={}),
        ],
    )

    migrate(src, drevo_db)
    a_id = drevo_db.get_node_by_title("a").id  # type: ignore[union-attr]
    edges = drevo_db.edges_of(a_id, drevo.Direction.OUT)
    weights = {e.to_id: e.weight for e in edges}
    b_id = drevo_db.get_node_by_title("b").id  # type: ignore[union-attr]
    d_id = drevo_db.get_node_by_title("d").id  # type: ignore[union-attr]
    assert weights[b_id] == 3.5
    assert weights[d_id] == 1.0


def test_dangling_relationship_skipped_when_on_error_skip(drevo_db: drevo.Drevo) -> None:
    src = FakeSource(
        nodes=[SourceNode(id="a", labels=["N"], properties={"name": "a"})],
        relationships=[
            SourceRelationship(id="r1", type="LINK", start="a", end="missing", properties={})
        ],
    )

    report = migrate(src, drevo_db, config=MigrationConfig(on_error="skip"))

    assert report.edges_created == 0
    assert report.edges_skipped == 1
    assert report.errors


def test_dangling_relationship_raises_when_on_error_raise(drevo_db: drevo.Drevo) -> None:
    src = FakeSource(
        nodes=[SourceNode(id="a", labels=["N"], properties={"name": "a"})],
        relationships=[
            SourceRelationship(id="r1", type="LINK", start="a", end="missing", properties={})
        ],
    )

    with pytest.raises(KeyError):
        migrate(src, drevo_db, config=MigrationConfig(on_error="raise"))


# ── property coercion ────────────────────────────────────────────────


def test_temporal_like_property_coerced_via_isoformat(drevo_db: drevo.Drevo) -> None:
    import datetime

    when = datetime.datetime(2026, 6, 7, 12, 0, 0)
    src = FakeSource(
        nodes=[SourceNode(id="n", labels=["Event"], properties={"title": "E", "at": when})]
    )

    report = migrate(src, drevo_db)
    node = drevo_db.get_node(report.id_map["n"])

    assert node is not None
    assert node.properties["at"] == when.isoformat()


def test_nested_list_and_dict_properties_round_trip(drevo_db: drevo.Drevo) -> None:
    props = {"title": "N", "tags": ["x", "y"], "meta": {"k": 1, "nested": [1, 2]}}
    src = FakeSource(nodes=[SourceNode(id="n", labels=["N"], properties=props)])

    report = migrate(src, drevo_db)
    node = drevo_db.get_node(report.id_map["n"])

    assert node is not None
    assert node.properties["tags"] == ["x", "y"]
    assert node.properties["meta"] == {"k": 1, "nested": [1, 2]}


def test_unknown_object_property_coerced_to_str(drevo_db: drevo.Drevo) -> None:
    class Weird:
        def __str__(self) -> str:
            return "weird-value"

    src = FakeSource(
        nodes=[SourceNode(id="n", labels=["N"], properties={"title": "N", "w": Weird()})]
    )
    report = migrate(src, drevo_db)
    node = drevo_db.get_node(report.id_map["n"])
    assert node is not None
    assert node.properties["w"] == "weird-value"


# ── report integrity ─────────────────────────────────────────────────


def test_report_counts_match_created_rows(drevo_db: drevo.Drevo) -> None:
    nodes = [SourceNode(id=f"n{i}", labels=["N"], properties={"name": f"n{i}"}) for i in range(5)]
    rels = [
        SourceRelationship(id=f"r{i}", type="NEXT", start=f"n{i}", end=f"n{i + 1}", properties={})
        for i in range(4)
    ]
    report = migrate(FakeSource(nodes, rels), drevo_db)

    assert report.nodes_created == 5
    assert report.edges_created == 4
    assert len(report.id_map) == 5
    for _src_id, drevo_id in report.id_map.items():
        assert drevo_db.get_node(drevo_id) is not None
