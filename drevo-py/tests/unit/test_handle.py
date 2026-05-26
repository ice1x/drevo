"""Unit tests for `drevo.Drevo` handle lifecycle (00118).

Boundary: every public method on the handle that does NOT touch graph
state — `open` / `open_in_memory` / `close` / `__enter__` / `__exit__`
/ `compact` / `health_check`. CRUD lives in `test_nodes.py` /
`test_edges.py`.

Per RFC §4.2 the handle releases the GIL around every storage I/O
call; this file asserts the visible *contract* (return types, idempotence,
post-close behaviour) — the GIL discipline itself is asserted by the
text-level scaffolding tests in `tests/python_api_scaffolding_tests.rs`.
"""

from __future__ import annotations

import os
import tempfile

import pytest

import drevo


def test_open_in_memory_returns_drevo_instance() -> None:
    """`Drevo.open_in_memory()` returns a `Drevo` (not e.g. a Result)."""
    db = drevo.Drevo.open_in_memory()
    try:
        assert isinstance(db, drevo.Drevo)
    finally:
        db.close()


def test_open_with_tempfile_round_trips() -> None:
    """`Drevo.open(path)` produces a handle whose state survives close+reopen.

    A unit test owns the temp path it creates so the assertion lives
    fully inside the test body — the integration suite is where larger
    multi-test fixtures over the disk backend belong.
    """
    with tempfile.TemporaryDirectory() as td:
        path = os.path.join(td, "u.drevo")
        with drevo.Drevo.open(path) as db:
            node = db.create_node(drevo.NewNode(kind="note", title="persists"))
            saved_id = node.id
        with drevo.Drevo.open(path) as db2:
            fetched = db2.get_node(saved_id)
            assert fetched is not None
            assert fetched.title == "persists"


def test_context_manager_enter_returns_self() -> None:
    """`__enter__` returns the same handle (Python `with` idiom)."""
    db = drevo.Drevo.open_in_memory()
    with db as opened:
        assert opened is db


def test_context_manager_exit_closes_handle() -> None:
    """After `__exit__`, the handle behaves as closed."""
    db = drevo.Drevo.open_in_memory()
    with db:
        pass
    with pytest.raises(RuntimeError):
        db.health_check()


def test_close_is_idempotent() -> None:
    """Calling `close()` twice does not raise — defensive for test
    fixtures that may double-close on cleanup paths.
    """
    db = drevo.Drevo.open_in_memory()
    db.close()
    db.close()  # must not raise


def test_methods_after_close_raise_runtime_error(drevo_db: drevo.Drevo) -> None:
    """Every storage method on a closed handle raises `RuntimeError`.

    Locks the RFC §5 invariant that closed handles surface a Python
    `RuntimeError`, not e.g. `StorageError` or a segfault.
    """
    drevo_db.close()
    with pytest.raises(RuntimeError):
        drevo_db.create_node(drevo.NewNode(kind="note", title="x"))


def test_health_check_on_open_handle_does_not_raise(drevo_db: drevo.Drevo) -> None:
    """`health_check()` returns `None` on a healthy handle."""
    assert drevo_db.health_check() is None


def test_compact_returns_compact_report(drevo_db: drevo.Drevo) -> None:
    """`compact()` returns a `CompactReport` instance (not e.g. a dict)."""
    report = drevo_db.compact()
    assert isinstance(report, drevo.CompactReport)


def test_compact_report_as_dict_has_expected_keys(drevo_db: drevo.Drevo) -> None:
    """`CompactReport.as_dict()` exposes every documented field."""
    report = drevo_db.compact()
    payload = report.as_dict()
    for key in ("bytes_before", "bytes_after", "bytes_reclaimed", "next_node_id", "next_edge_id"):
        assert key in payload


def test_compact_bytes_reclaimed_is_non_negative(drevo_db: drevo.Drevo) -> None:
    """`bytes_reclaimed` never goes negative — a no-op compact still
    returns ≥ 0, matching the i64 → u64 contract on the Rust side.
    """
    report = drevo_db.compact()
    assert report.bytes_reclaimed >= 0
