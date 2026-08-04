"""Integration tests for the #243 slice 2 adjacency migration surface.

Drives both the binding (`Drevo.migrate(path, direction)`) and the CLI
(`python -m drevo migrate ...`) over the real disk backend:

* migrating an up-to-date database down to the legacy layout makes a fresh
  `Drevo.open` raise `NeedsMigrationError`,
* migrating it back up restores access with the graph fully intact, and
* the CLI takes a raw-file backup before mutating and reports the edge count.

The migration only rebuilds the derived adjacency index, so these tests also
assert no graph data is lost across the round trip.
"""

from __future__ import annotations

from pathlib import Path

import drevo
import pytest
from drevo.__main__ import main
from faker import Faker


def _seed(path: str, fake: Faker) -> tuple[int, list[int]]:
    """Create a small star graph; return (hub_id, leaf_ids)."""
    with drevo.Drevo.open(path) as db:
        hub = db.create_node(drevo.NewNode(kind="hub", title=f"hub-{fake.word()}")).id
        leaves: list[int] = []
        for i in range(5):
            leaf = db.create_node(drevo.NewNode(kind="leaf", title=f"leaf-{i}-{fake.word()}")).id
            kind = "knows" if i % 2 == 0 else "likes"
            db.create_edge(drevo.NewEdge(from_id=hub, to_id=leaf, kind=kind))
            leaves.append(leaf)
    return hub, leaves


def test_binding_migrate_down_then_up_round_trips(tmp_db_path: str, fake: Faker) -> None:
    hub, leaves = _seed(tmp_db_path, fake)

    # Downgrade the freshly-written v2 file to the legacy v1 layout.
    moved = drevo.Drevo.migrate(tmp_db_path, "down")
    assert moved == len(leaves)

    # A legacy file must now be refused rather than misread.
    with pytest.raises(drevo.NeedsMigrationError):
        drevo.Drevo.open(tmp_db_path)

    # Migrate up: access restored, graph intact.
    assert drevo.Drevo.migrate(tmp_db_path, "up") == len(leaves)
    with drevo.Drevo.open(tmp_db_path) as db:
        got = sorted(n.id for n in db.neighbors(hub, drevo.Direction.OUT))
        assert got == sorted(leaves)


def test_binding_migrate_rejects_bad_direction(tmp_db_path: str, fake: Faker) -> None:
    _seed(tmp_db_path, fake)
    with pytest.raises(ValueError):
        drevo.Drevo.migrate(tmp_db_path, "sideways")


def test_cli_migrate_backs_up_then_upgrades(tmp_path: Path, fake: Faker) -> None:
    db_path = str(tmp_path / "graph.redb")
    hub, leaves = _seed(db_path, fake)

    # Put the file into the legacy layout so `migrate up` has work to do.
    drevo.Drevo.migrate(db_path, "down")

    rc = main(["migrate", "up", db_path])
    assert rc == 0

    # The CLI took a raw-file backup before mutating.
    assert (tmp_path / "graph.redb.pre-migrate.bak").exists()

    # And the upgraded database opens with the graph intact.
    with drevo.Drevo.open(db_path) as db:
        got = sorted(n.id for n in db.neighbors(hub, drevo.Direction.OUT))
        assert got == sorted(leaves)


def test_cli_migrate_refuses_to_clobber_existing_backup(tmp_path: Path, fake: Faker) -> None:
    db_path = str(tmp_path / "graph.redb")
    _seed(db_path, fake)
    drevo.Drevo.migrate(db_path, "down")

    backup = tmp_path / "graph.redb.pre-migrate.bak"
    backup.write_text("precious earlier backup")

    # Without --force the CLI refuses rather than overwriting the backup.
    rc = main(["migrate", "up", db_path])
    assert rc == 1
    assert backup.read_text() == "precious earlier backup"
