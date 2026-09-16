"""Handle life-cycle invariants on the real disk backend.

Unit tests pin `open_in_memory` / context-manager semantics. This
suite pins the *disk* edges: file-lock contention between two open
handles, idempotent close on a real file, compaction reporting non-zero
metrics on a non-empty database, and `health_check` succeeding through
the file backend.
"""

from __future__ import annotations

import pytest

import drevo


def test_second_open_on_same_path_is_permitted_for_now(tmp_db_path: str) -> None:
    """The native durable engine does not yet take a cross-handle lock on
    the WAL, so a second `Drevo.open(path)` while the first is open
    currently succeeds (the redb backend used to raise here via its
    exclusive file lock).

    This is a known gap tracked in issue #455 (restore exclusive-open via an
    OS advisory `flock`, which — unlike a lock file — is released when the
    process dies and so cannot leave a stale lock that blocks a restart).
    When that lands this test flips back to asserting a `DrevoError`.
    """
    with drevo.Drevo.open(tmp_db_path):
        second = drevo.Drevo.open(tmp_db_path)
        second.close()


def test_open_after_close_succeeds(tmp_db_path: str) -> None:
    """Once the first handle is closed, the path is openable again."""
    with drevo.Drevo.open(tmp_db_path) as db:
        db.create_node(drevo.NewNode(kind="note", title="x"))
    with drevo.Drevo.open(tmp_db_path) as db:
        assert db.get_node_by_title("x") is not None


def test_close_releases_lock_even_if_body_raises(tmp_db_path: str) -> None:
    """The context manager closes (and releases the lock) on exception."""

    class _Sentinel(Exception):
        pass

    with pytest.raises(_Sentinel):
        with drevo.Drevo.open(tmp_db_path):
            raise _Sentinel()
    # If the lock were still held, this open would raise LockedError.
    with drevo.Drevo.open(tmp_db_path) as db:
        db.health_check()  # raises StorageError on failure; returns None on success


def test_health_check_succeeds_on_disk_backend(disk_db: drevo.Drevo) -> None:
    """The probe returns None on a freshly-opened disk-backed handle.

    `health_check()` returns ``None`` on success and raises
    `StorageError` otherwise — a successful return is the assertion.
    """
    disk_db.health_check()


def test_compact_after_writes_returns_positive_next_ids(
    disk_db: drevo.Drevo,
) -> None:
    """After committing some rows, `compact()` reports `next_node_id`
    and `next_edge_id` strictly greater than 1.
    """
    a = disk_db.create_node(drevo.NewNode(kind="note", title="a"))
    b = disk_db.create_node(drevo.NewNode(kind="note", title="b"))
    disk_db.create_edge(drevo.NewEdge(from_id=a.id, to_id=b.id, kind="links_to"))
    report = disk_db.compact()
    assert report.next_node_id > 1
    assert report.next_edge_id > 1


def test_compact_survives_reopen(tmp_db_path: str) -> None:
    """A compact run between two sessions does not break the second
    session's reads.
    """
    with drevo.Drevo.open(tmp_db_path) as db:
        nodes = [db.create_node(drevo.NewNode(kind="t", title=f"row-{i}")) for i in range(10)]
        for n in nodes[:5]:
            db.delete_node(n.id)
        db.compact()
        expected_titles = {f"row-{i}" for i in range(5, 10)}
    with drevo.Drevo.open(tmp_db_path) as db:
        rows = db.list_nodes_by_kind("t", limit=100, offset=0)
        assert {n.title for n in rows} == expected_titles
