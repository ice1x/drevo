"""Unit tests for the GraphML PyO3 bindings (export/import + ImportReport).

Exercises the four new `Drevo` methods at the binding boundary on the
in-memory backend: `export_graphml` produces XML carrying the seeded data,
`import_graphml` round-trips it faithfully into a fresh handle and reports the
right counts, and re-importing is idempotent (rows skipped, not duplicated).
"""

from __future__ import annotations

import drevo
from faker import Faker


def _seed(db: drevo.Drevo, fake: Faker) -> tuple[list[int], list[int], dict[int, str]]:
    """Create 3 nodes + 2 edges; return (node_ids, edge_ids, id->title)."""
    titles = {}
    node_ids = []
    for i in range(3):
        title = f"node-{i}-{fake.word()}"
        node = db.create_node(drevo.NewNode(kind="note", title=title, body=fake.sentence()))
        node_ids.append(node.id)
        titles[node.id] = title
    edge_ids = [
        db.create_edge(
            drevo.NewEdge(from_id=node_ids[0], to_id=node_ids[1], kind="links_to", weight=2.5)
        ).id,
        db.create_edge(drevo.NewEdge(from_id=node_ids[1], to_id=node_ids[2], kind="mentions")).id,
    ]
    return node_ids, edge_ids, titles


def test_export_graphml_returns_xml_with_seeded_titles(drevo_db: drevo.Drevo, fake: Faker) -> None:
    _, _, titles = _seed(drevo_db, fake)
    xml = drevo_db.export_graphml()
    assert xml.lstrip().startswith("<?xml") or "<graphml" in xml
    for title in titles.values():
        assert title in xml


def test_import_graphml_round_trips_into_fresh_handle(drevo_db: drevo.Drevo, fake: Faker) -> None:
    node_ids, edge_ids, titles = _seed(drevo_db, fake)
    xml = drevo_db.export_graphml()

    with drevo.Drevo.open_in_memory() as fresh:
        report = fresh.import_graphml(xml)
        assert report.nodes_imported == len(node_ids)
        assert report.edges_imported == len(edge_ids)
        assert report.nodes_skipped == 0
        assert report.edges_skipped == 0

        # Faithful, id-preserving round trip.
        for nid, title in titles.items():
            got = fresh.get_node(nid)
            assert got is not None
            assert got.title == title
            assert got.kind == "note"
        for eid in edge_ids:
            assert fresh.get_edge(eid) is not None


def test_import_graphml_is_idempotent(drevo_db: drevo.Drevo, fake: Faker) -> None:
    node_ids, edge_ids, _ = _seed(drevo_db, fake)
    xml = drevo_db.export_graphml()
    # Re-importing into the SAME db skips every byte-equal row.
    report = drevo_db.import_graphml(xml)
    assert report.nodes_imported == 0
    assert report.edges_imported == 0
    assert report.nodes_skipped == len(node_ids)
    assert report.edges_skipped == len(edge_ids)


def test_import_report_repr_and_as_dict(drevo_db: drevo.Drevo, fake: Faker) -> None:
    _seed(drevo_db, fake)
    with drevo.Drevo.open_in_memory() as fresh:
        report = fresh.import_graphml(drevo_db.export_graphml())
    assert "ImportReport(" in repr(report)
    d = report.as_dict()
    assert d["nodes_imported"] == 3
    assert d["edges_imported"] == 2
