"""Unit tests for `Drevo` node CRUD + listing (00118).

One focused assertion per case. Where the contract is shaped by a row
of input variants, `pytest.mark.parametrize` keeps the assertion body
single while still surfacing one failure per input row.
"""

from __future__ import annotations

import uuid

import pytest

import drevo

# ── create_node ──────────────────────────────────────────────────────


def test_create_node_minimal_returns_node(drevo_db: drevo.Drevo) -> None:
    node = drevo_db.create_node(drevo.NewNode(kind="note", title="hello"))
    assert isinstance(node, drevo.Node)


def test_create_node_assigns_positive_id(drevo_db: drevo.Drevo) -> None:
    node = drevo_db.create_node(drevo.NewNode(kind="note", title="x"))
    assert node.id >= 1


def test_create_node_ids_are_monotonic(drevo_db: drevo.Drevo) -> None:
    """Sequential `create_node` calls produce strictly increasing ids.

    Locks the redb auto-increment behaviour at the Python surface — a
    future change that switches to random ids would break this and
    deserves an RFC.
    """
    a = drevo_db.create_node(drevo.NewNode(kind="note", title="a"))
    b = drevo_db.create_node(drevo.NewNode(kind="note", title="b"))
    assert b.id > a.id


def test_create_node_assigns_uuid(drevo_db: drevo.Drevo) -> None:
    """The shim wraps the cdylib's `bytes` UUID as `uuid.UUID`."""
    node = drevo_db.create_node(drevo.NewNode(kind="note", title="t"))
    assert isinstance(node.uuid, uuid.UUID)


def test_create_node_uuids_are_unique(drevo_db: drevo.Drevo) -> None:
    a = drevo_db.create_node(drevo.NewNode(kind="note", title="a"))
    b = drevo_db.create_node(drevo.NewNode(kind="note", title="b"))
    assert a.uuid != b.uuid


def test_create_node_stores_body(drevo_db: drevo.Drevo, fake) -> None:
    body = fake.text()
    node = drevo_db.create_node(drevo.NewNode(kind="note", title="t", body=body))
    assert node.body == body


def test_create_node_stores_body_html(drevo_db: drevo.Drevo) -> None:
    node = drevo_db.create_node(drevo.NewNode(kind="note", title="t", body_html="<p>hi</p>"))
    assert node.body_html == "<p>hi</p>"


def test_create_node_stores_properties_dict(drevo_db: drevo.Drevo, fake) -> None:
    """Arbitrary JSON-shaped properties survive the Rust↔Python round-trip."""
    props = {"tags": [fake.word(), fake.word()], "count": 7, "nested": {"k": fake.word()}}
    node = drevo_db.create_node(drevo.NewNode(kind="note", title="t", properties=props))
    assert node.properties == props


def test_create_node_duplicate_title_raises(drevo_db: drevo.Drevo) -> None:
    """RFC §5.1 — a colliding title surfaces as `DuplicateTitleError`."""
    drevo_db.create_node(drevo.NewNode(kind="note", title="dup"))
    with pytest.raises(drevo.DuplicateTitleError):
        drevo_db.create_node(drevo.NewNode(kind="note", title="dup"))


# ── get_node / get_node_by_uuid / get_node_by_title ──────────────────


def test_get_node_returns_existing(drevo_db: drevo.Drevo) -> None:
    saved = drevo_db.create_node(drevo.NewNode(kind="note", title="t"))
    fetched = drevo_db.get_node(saved.id)
    assert fetched is not None
    assert fetched.id == saved.id


def test_get_node_returns_none_for_missing(drevo_db: drevo.Drevo) -> None:
    """`get_node` follows the Optional contract — `None`, not raise."""
    assert drevo_db.get_node(99_999) is None


def test_get_node_by_uuid_round_trip(drevo_db: drevo.Drevo) -> None:
    saved = drevo_db.create_node(drevo.NewNode(kind="note", title="t"))
    fetched = drevo_db.get_node_by_uuid(saved.uuid.bytes)
    assert fetched is not None
    assert fetched.id == saved.id


def test_get_node_by_uuid_returns_none_for_missing(drevo_db: drevo.Drevo) -> None:
    missing = bytes(16)
    assert drevo_db.get_node_by_uuid(missing) is None


def test_get_node_by_title_round_trip(drevo_db: drevo.Drevo) -> None:
    saved = drevo_db.create_node(drevo.NewNode(kind="note", title="unique-title"))
    fetched = drevo_db.get_node_by_title("unique-title")
    assert fetched is not None
    assert fetched.id == saved.id


def test_get_node_by_title_returns_none_for_missing(drevo_db: drevo.Drevo) -> None:
    assert drevo_db.get_node_by_title("no-such-title") is None


# ── update_node ──────────────────────────────────────────────────────


def test_update_node_patches_title(drevo_db: drevo.Drevo) -> None:
    saved = drevo_db.create_node(drevo.NewNode(kind="note", title="before"))
    patched = drevo_db.update_node(saved.id, drevo.NodePatch(title="after"))
    assert patched.title == "after"


def test_update_node_leaves_unset_fields_intact(drevo_db: drevo.Drevo, fake) -> None:
    """A `NodePatch` only touches the keys explicitly set on it."""
    body = fake.sentence()
    saved = drevo_db.create_node(drevo.NewNode(kind="note", title="t", body=body))
    patched = drevo_db.update_node(saved.id, drevo.NodePatch(title="new title"))
    assert patched.body == body


def test_update_node_patches_properties(drevo_db: drevo.Drevo) -> None:
    saved = drevo_db.create_node(drevo.NewNode(kind="note", title="t", properties={"v": 1}))
    patched = drevo_db.update_node(saved.id, drevo.NodePatch(properties={"v": 2}))
    assert patched.properties["v"] == 2


def test_update_node_missing_raises_node_not_found(drevo_db: drevo.Drevo) -> None:
    """RFC §5.1 — updating a missing id raises `NodeNotFoundError`."""
    with pytest.raises(drevo.NodeNotFoundError):
        drevo_db.update_node(99_999, drevo.NodePatch(title="x"))


# ── delete_node ──────────────────────────────────────────────────────


def test_delete_node_removes_existing(drevo_db: drevo.Drevo) -> None:
    saved = drevo_db.create_node(drevo.NewNode(kind="note", title="bye"))
    drevo_db.delete_node(saved.id)
    assert drevo_db.get_node(saved.id) is None


def test_delete_node_missing_raises_node_not_found(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(drevo.NodeNotFoundError):
        drevo_db.delete_node(99_999)


# ── list_nodes_by_kind / list_recent ─────────────────────────────────


def test_list_nodes_by_kind_filters_by_kind(drevo_db: drevo.Drevo) -> None:
    drevo_db.create_node(drevo.NewNode(kind="task", title="t-1"))
    drevo_db.create_node(drevo.NewNode(kind="note", title="n-1"))
    drevo_db.create_node(drevo.NewNode(kind="task", title="t-2"))
    tasks = drevo_db.list_nodes_by_kind("task", 100, 0)
    assert {n.title for n in tasks} == {"t-1", "t-2"}


def test_list_nodes_by_kind_respects_limit(drevo_db: drevo.Drevo) -> None:
    for i in range(5):
        drevo_db.create_node(drevo.NewNode(kind="note", title=f"n-{i}"))
    page = drevo_db.list_nodes_by_kind("note", 2, 0)
    assert len(page) == 2


def test_list_nodes_by_kind_respects_offset(drevo_db: drevo.Drevo) -> None:
    """Page two of a 5-item list contains the remaining rows."""
    for i in range(5):
        drevo_db.create_node(drevo.NewNode(kind="note", title=f"n-{i}"))
    page = drevo_db.list_nodes_by_kind("note", 2, 4)
    assert len(page) == 1


def test_list_nodes_by_kind_unknown_kind_returns_empty(drevo_db: drevo.Drevo) -> None:
    assert drevo_db.list_nodes_by_kind("nothing-like-this", 10, 0) == []


def test_list_recent_returns_at_most_limit_nodes(drevo_db: drevo.Drevo) -> None:
    for i in range(7):
        drevo_db.create_node(drevo.NewNode(kind="note", title=f"r-{i}"))
    assert len(drevo_db.list_recent(3)) == 3
