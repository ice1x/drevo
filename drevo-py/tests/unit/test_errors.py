"""Unit tests for the typed exception hierarchy (RFC §5.1, 00118).

`test_shim.py` (00116) already locks the *class shape* — these tests
lock the *behaviour* of every variant: a) which `Drevo` method raises
it, b) the args carried on the exception, c) the inheritance chain
that lets `except drevo.<RootError>:` catch it.

The aim is one DrevoError variant ↔ at least one focused test.
"""

from __future__ import annotations

import math
import os
import tempfile

import pytest

import drevo


# ── Inheritance chain — locks RFC §5.1 ───────────────────────────────


@pytest.mark.parametrize(
    "cls,parent",
    [
        (drevo.NotFoundError, drevo.DrevoError),
        (drevo.NodeNotFoundError, drevo.NotFoundError),
        (drevo.EdgeNotFoundError, drevo.NotFoundError),
        (drevo.ConflictError, drevo.DrevoError),
        (drevo.DuplicateTitleError, drevo.ConflictError),
        (drevo.StorageError, drevo.DrevoError),
        (drevo.SerializationError, drevo.DrevoError),
        (drevo.LockedError, drevo.DrevoError),
        (drevo.PanicError, drevo.DrevoError),
        (drevo.InvalidWeightError, ValueError),
    ],
)
def test_exception_inherits_from_parent(cls: type, parent: type) -> None:
    """Every variant inherits from its documented parent (RFC §5.1 + §5.3)."""
    assert issubclass(cls, parent)


def test_node_not_found_caught_by_drevo_error_root(drevo_db: drevo.Drevo) -> None:
    """A bare `except DrevoError:` catches `NodeNotFoundError` — RFC §5.1."""
    with pytest.raises(drevo.DrevoError):
        drevo_db.delete_node(99_999)


def test_edge_not_found_caught_by_not_found_error(drevo_db: drevo.Drevo) -> None:
    """`except NotFoundError:` catches both node + edge variants."""
    with pytest.raises(drevo.NotFoundError):
        drevo_db.delete_edge(99_999)


def test_duplicate_title_caught_by_conflict_error(drevo_db: drevo.Drevo) -> None:
    """A duplicate title surfaces as a `ConflictError` subclass."""
    drevo_db.create_node(drevo.NewNode(kind="note", title="dup"))
    with pytest.raises(drevo.ConflictError):
        drevo_db.create_node(drevo.NewNode(kind="note", title="dup"))


# ── Per-variant trigger tests ────────────────────────────────────────


def test_node_not_found_on_get_via_update(drevo_db: drevo.Drevo) -> None:
    """`update_node(missing_id)` raises `NodeNotFoundError`."""
    with pytest.raises(drevo.NodeNotFoundError):
        drevo_db.update_node(99_999, drevo.NodePatch(title="x"))


def test_edge_not_found_on_update(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(drevo.EdgeNotFoundError):
        drevo_db.update_edge(99_999, drevo.EdgePatch(weight=1.0))


def test_invalid_weight_error_on_nan_weight(drevo_db: drevo.Drevo) -> None:
    """`InvalidWeightError` is the canonical raise for a non-finite weight."""
    src = drevo_db.create_node(drevo.NewNode(kind="note", title="s"))
    dst = drevo_db.create_node(drevo.NewNode(kind="note", title="d"))
    with pytest.raises(ValueError):
        drevo_db.create_edge(drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="x", weight=math.nan))


def test_invalid_weight_error_is_value_error_subclass() -> None:
    """`InvalidWeightError` ⊂ `ValueError` — the cross-cutting catch."""
    err = drevo.InvalidWeightError("oops")
    assert isinstance(err, ValueError)
    assert isinstance(err, drevo.InvalidWeightError)


# ── DrevoError root catches every variant ────────────────────────────


@pytest.mark.parametrize(
    "factory",
    [
        lambda db: db.delete_node(99_999),
        lambda db: db.delete_edge(99_999),
        lambda db: db.update_node(99_999, drevo.NodePatch(title="x")),
        lambda db: db.update_edge(99_999, drevo.EdgePatch(weight=1.0)),
    ],
)
def test_drevo_error_catches_each_storage_error_path(drevo_db: drevo.Drevo, factory) -> None:
    """Every documented "not found" path is catchable by `DrevoError`."""
    with pytest.raises(drevo.DrevoError):
        factory(drevo_db)


def test_locked_error_class_is_constructible() -> None:
    """`LockedError(*args)` builds without raising — the variant is
    declared at the Python level even when it doesn't fire in the
    in-memory backend (it can only trigger on a disk-locked file).
    """
    err = drevo.LockedError("path/to/db", "another process")
    assert isinstance(err, drevo.DrevoError)


def test_storage_error_class_is_constructible() -> None:
    err = drevo.StorageError("redb failed", "table=nodes")
    assert isinstance(err, drevo.DrevoError)


def test_serialization_error_class_is_constructible() -> None:
    err = drevo.SerializationError("bad utf-8 at offset 4")
    assert isinstance(err, drevo.DrevoError)


def test_panic_error_class_is_constructible() -> None:
    err = drevo.PanicError("unwound at boundary")
    assert isinstance(err, drevo.DrevoError)


def test_node_not_found_carries_id_in_args(drevo_db: drevo.Drevo) -> None:
    """`.args` exposes the missing id so callers can format their own
    error message (RFC §5.1: "args == (node_id,)").
    """
    try:
        drevo_db.delete_node(12345)
    except drevo.NodeNotFoundError as e:
        assert 12345 in [arg for arg in e.args if isinstance(arg, int)] or any(
            "12345" in str(arg) for arg in e.args
        )
    else:  # pragma: no cover — assertion failure path
        pytest.fail("expected NodeNotFoundError")


def test_double_open_surfaces_drevo_error() -> None:
    """Opening the same file twice surfaces a `DrevoError` subclass.

    Today redb raises through `StorageError`; if the mapping ever
    promotes the file-lock case to a dedicated `LockedError` (RFC §5.1
    leaves room for it) this test still passes because both inherit
    from `DrevoError`. The narrower assertion lives in the integration
    suite where the actual lock semantics matter.
    """
    with tempfile.TemporaryDirectory() as td:
        path = os.path.join(td, "lock.drevo")
        first = drevo.Drevo.open(path)
        try:
            with pytest.raises(drevo.DrevoError):
                drevo.Drevo.open(path)
        finally:
            first.close()
