"""Integration tests for the GraphML backup CLI over the real disk backend.

Drives `python -m drevo` end to end: seed a disk database, `dump` it to
GraphML, `restore` into a fresh database, and assert a faithful, id-preserving
round trip. Also covers `shrink` (dump + re-import into a new file).
"""

from __future__ import annotations

from pathlib import Path

import drevo
from drevo.__main__ import main
from faker import Faker


def _seed_disk_db(path: str, fake: Faker) -> tuple[dict[int, tuple[str, str]], list[int]]:
    """Populate a disk db; return (node_id -> (title, body)), edge_ids."""
    nodes: dict[int, tuple[str, str]] = {}
    edge_ids: list[int] = []
    with drevo.Drevo.open(path) as db:
        ids: list[int] = []
        for i in range(6):
            title = f"n{i}-{fake.word()}-{i}"
            body = fake.sentence()
            node = db.create_node(drevo.NewNode(kind=f"kind{i % 3}", title=title, body=body))
            nodes[node.id] = (title, body)
            ids.append(node.id)
        for i in range(len(ids) - 1):
            edge_ids.append(
                db.create_edge(
                    drevo.NewEdge(
                        from_id=ids[i], to_id=ids[i + 1], kind="chain", weight=float(i + 1)
                    )
                ).id
            )
    return nodes, edge_ids


def test_cli_dump_then_restore_round_trips(tmp_path: Path, fake: Faker) -> None:
    src = str(tmp_path / "src.redb")
    graphml = str(tmp_path / "backup.graphml")
    dst = str(tmp_path / "restored.redb")

    nodes, edge_ids = _seed_disk_db(src, fake)

    assert main(["dump", src, graphml]) == 0
    assert Path(graphml).exists() and Path(graphml).stat().st_size > 0

    assert main(["restore", graphml, dst]) == 0

    # Faithful id-preserving round trip.
    with drevo.Drevo.open(dst) as db:
        for nid, (title, body) in nodes.items():
            got = db.get_node(nid)
            assert got is not None, f"node {nid} missing after restore"
            assert got.title == title
            assert got.body == body
        for eid in edge_ids:
            assert db.get_edge(eid) is not None, f"edge {eid} missing after restore"


def test_cli_shrink_produces_a_fresh_db_with_same_data(tmp_path: Path, fake: Faker) -> None:
    src = str(tmp_path / "src.redb")
    out_db = str(tmp_path / "small.redb")
    nodes, edge_ids = _seed_disk_db(src, fake)

    assert main(["shrink", src, out_db]) == 0
    assert Path(out_db).exists()

    with drevo.Drevo.open(out_db) as db:
        for nid, (title, _body) in nodes.items():
            got = db.get_node(nid)
            assert got is not None
            assert got.title == title
        for eid in edge_ids:
            assert db.get_edge(eid) is not None

    # The source is never modified by shrink.
    assert Path(src).exists()
