"""Integration tests for the #253 slice 1 storage-bloat report binding.

Exercises `Drevo.bloat_report()` over the real disk backend: it reports a
physical `file_bytes`, a logical size that grows with the data, node/edge
counts, and a defined bloat ratio (≥ 1, since a redb file is never smaller
than the logical data it holds).
"""

from __future__ import annotations

import drevo
from faker import Faker


def test_bloat_report_on_disk_measures_file_logical_and_ratio(
    tmp_db_path: str, fake: Faker
) -> None:
    with drevo.Drevo.open(tmp_db_path) as db:
        ids = [
            db.create_node(
                drevo.NewNode(kind="n", title=f"{fake.unique.word()}-{i}", body=fake.text())
            ).id
            for i in range(15)
        ]
        for i in range(len(ids) - 1):
            db.create_edge(drevo.NewEdge(from_id=ids[i], to_id=ids[i + 1], kind="chain"))

        report = db.bloat_report()

    assert report.node_count == 15
    assert report.edge_count == 14
    assert report.logical_bytes > 0
    assert report.file_bytes is not None and report.file_bytes > 0
    # A real redb file is at least as large as its logical data.
    assert report.bloat_ratio is not None and report.bloat_ratio >= 1.0

    d = report.as_dict()
    assert d["node_count"] == 15
    assert d["edge_count"] == 14
    assert set(d) == {
        "file_bytes",
        "logical_bytes",
        "node_count",
        "edge_count",
        "bloat_ratio",
    }


def test_bloat_report_in_memory_has_no_file_bytes(fake: Faker) -> None:
    db = drevo.Drevo.open_in_memory()
    db.create_node(drevo.NewNode(kind="n", title=fake.word()))
    report = db.bloat_report()
    # Ephemeral backend → no physical footprint, so no ratio.
    assert report.file_bytes is None
    assert report.bloat_ratio is None
    assert report.node_count == 1
    assert report.logical_bytes > 0
