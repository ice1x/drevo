"""Unit tests for `ApocJsonSource` — the offline dump → load path.

Reads an `apoc.export.json.all` JSON-Lines dump from disk (no live Neo4j,
no driver) and presents it as the engine's `GraphSource`.
"""

from __future__ import annotations

import json
from pathlib import Path

import drevo
from neo4j_to_drevo import SourceNode, SourceRelationship, migrate
from neo4j_to_drevo.apoc import ApocJsonSource, write_apoc_json

_DUMP_LINES = [
    {"type": "node", "id": "1", "labels": ["Person"], "properties": {"name": "Ada"}},
    {"type": "node", "id": "2", "labels": ["Company", "Org"], "properties": {"name": "Acme"}},
    {
        "id": "10",
        "type": "relationship",
        "label": "WORKS_AT",
        "start": {"id": "1", "labels": ["Person"]},
        "end": {"id": "2", "labels": ["Company"]},
        "properties": {"since": 2020},
    },
]


def _write_dump(tmp_path: Path, lines: list[dict[str, object]]) -> str:
    path = tmp_path / "graph.json"
    path.write_text("\n".join(json.dumps(obj) for obj in lines) + "\n", encoding="utf-8")
    return str(path)


def test_nodes_parsed_from_dump(tmp_path: Path) -> None:
    src = ApocJsonSource(_write_dump(tmp_path, _DUMP_LINES))
    assert list(src.nodes()) == [
        SourceNode(id="1", labels=["Person"], properties={"name": "Ada"}),
        SourceNode(id="2", labels=["Company", "Org"], properties={"name": "Acme"}),
    ]


def test_relationships_parsed_from_dump(tmp_path: Path) -> None:
    src = ApocJsonSource(_write_dump(tmp_path, _DUMP_LINES))
    assert list(src.relationships()) == [
        SourceRelationship(id="10", type="WORKS_AT", start="1", end="2", properties={"since": 2020})
    ]


def test_blank_lines_are_skipped(tmp_path: Path) -> None:
    path = tmp_path / "graph.json"
    path.write_text(
        json.dumps(_DUMP_LINES[0]) + "\n\n   \n" + json.dumps(_DUMP_LINES[1]) + "\n",
        encoding="utf-8",
    )
    assert len(list(ApocJsonSource(str(path)).nodes())) == 2


def test_element_id_preferred_over_legacy_id(tmp_path: Path) -> None:
    lines = [
        {
            "type": "node",
            "elementId": "4:x:1",
            "id": "1",
            "labels": ["N"],
            "properties": {"name": "a"},
        },
        {
            "type": "node",
            "elementId": "4:x:2",
            "id": "2",
            "labels": ["N"],
            "properties": {"name": "b"},
        },
        {
            "type": "relationship",
            "elementId": "5:x:9",
            "label": "LINK",
            "start": {"elementId": "4:x:1", "id": "1"},
            "end": {"elementId": "4:x:2", "id": "2"},
            "properties": {},
        },
    ]
    src = ApocJsonSource(_write_dump(tmp_path, lines))
    node_ids = {n.id for n in src.nodes()}
    rel = next(iter(src.relationships()))
    assert node_ids == {"4:x:1", "4:x:2"}
    assert rel.start in node_ids and rel.end in node_ids


class _ListSource:
    def __init__(self, nodes: list[SourceNode], rels: list[SourceRelationship]) -> None:
        self._nodes, self._rels = nodes, rels

    def nodes(self):  # type: ignore[no-untyped-def]
        return iter(self._nodes)

    def relationships(self):  # type: ignore[no-untyped-def]
        return iter(self._rels)


def test_write_apoc_json_round_trips_through_reader(tmp_path: Path) -> None:
    # A GraphSource written out by `write_apoc_json` and read back by
    # `ApocJsonSource` must yield the identical records (dump↔load symmetry).
    nodes = [
        SourceNode(id="1", labels=["Person"], properties={"name": "Ada"}),
        SourceNode(id="2", labels=["Company", "Org"], properties={"name": "Acme"}),
    ]
    rels = [
        SourceRelationship(id="9", type="WORKS_AT", start="1", end="2", properties={"since": 2020})
    ]
    out = tmp_path / "dump.json"

    counts = write_apoc_json(_ListSource(nodes, rels), str(out))
    assert counts == (2, 1)

    reread = ApocJsonSource(str(out))
    assert list(reread.nodes()) == nodes
    assert list(reread.relationships()) == rels


def test_write_apoc_json_coerces_non_json_properties(tmp_path: Path) -> None:
    import datetime

    when = datetime.date(2026, 7, 1)
    nodes = [SourceNode(id="1", labels=["Event"], properties={"name": "E", "on": when})]
    out = tmp_path / "dump.json"

    write_apoc_json(_ListSource(nodes, []), str(out))

    node = next(iter(ApocJsonSource(str(out)).nodes()))
    assert node.properties["on"] == when.isoformat()  # date coerced to ISO string


def test_dump_round_trips_into_drevo(tmp_path: Path) -> None:
    src = ApocJsonSource(_write_dump(tmp_path, _DUMP_LINES))
    with drevo.Drevo.open_in_memory() as db:
        report = migrate(src, db)

        assert report.nodes_created == 2
        assert report.edges_created == 1
        ada = db.get_node_by_title("Ada")
        acme = db.get_node_by_title("Acme")
        assert ada is not None and acme is not None
        assert acme.kind == "Company:Org"
        out = db.edges_of(ada.id, drevo.Direction.OUT)
        assert len(out) == 1
        assert out[0].kind == "WORKS_AT"
        assert out[0].to_id == acme.id
        assert out[0].properties["since"] == 2020
