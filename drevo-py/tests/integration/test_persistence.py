"""Round-trip persistence across `close` + reopen on the disk backend.

The in-memory backend used by the 00118 unit suite has no on-disk form,
so allocator-state, kind-index rebuild, FTS posting-list flush, and the
file-lock release path only show up here. Each test exercises:

    open(path) → write → close → open(path) → assert

— the minimum interesting test for a durable embedded database. If any
of these fail, downstream agents using `drevo` as a knowledge store
silently lose data across process restarts.
"""

from __future__ import annotations

import drevo


def test_node_survives_close_and_reopen(tmp_db_path: str) -> None:
    """A node created in session 1 is visible in session 2 with the
    same id, title, kind, body, and properties.

    Touches the redb commit path on session 1's close and the
    table-rebuild path on session 2's open.
    """
    with drevo.Drevo.open(tmp_db_path) as db:
        node = db.create_node(
            drevo.NewNode(
                kind="note",
                title="durability-1",
                body="this body must survive a reopen",
                properties={"priority": 7},
            )
        )
        saved_id = node.id
    with drevo.Drevo.open(tmp_db_path) as db:
        fetched = db.get_node(saved_id)
        assert fetched is not None
        assert fetched.id == saved_id
        assert fetched.title == "durability-1"
        assert fetched.kind == "note"
        assert fetched.body == "this body must survive a reopen"
        assert fetched.properties["priority"] == 7


def test_edge_survives_close_and_reopen(tmp_db_path: str) -> None:
    """An edge round-trips with from/to/kind/weight preserved.

    Edge weight is stored as ``f32`` on the Rust side, so the assertion
    uses :func:`pytest.approx` with an absolute tolerance one ULP wide
    enough to absorb the ``f64 -> f32 -> f64`` round-trip. The exact
    value 0.5 would compare bit-equal, but any non-power-of-two probe
    (0.42 here) shows the precision loss; using ``approx`` documents
    the contract honestly without hiding the storage width.
    """
    import pytest as _pytest

    with drevo.Drevo.open(tmp_db_path) as db:
        a = db.create_node(drevo.NewNode(kind="note", title="src"))
        b = db.create_node(drevo.NewNode(kind="note", title="dst"))
        e = db.create_edge(drevo.NewEdge(from_id=a.id, to_id=b.id, kind="links_to", weight=0.42))
        edge_id = e.id
    with drevo.Drevo.open(tmp_db_path) as db:
        fetched = db.get_edge(edge_id)
        assert fetched is not None
        assert fetched.kind == "links_to"
        assert fetched.weight == _pytest.approx(0.42, abs=1e-6)


def test_uuid_round_trips_through_reopen(tmp_db_path: str) -> None:
    """`Node.uuid` survives reopen byte-for-byte.

    The shim wraps the raw `bytes` returned by PyO3 in `uuid.UUID`; this
    pins that the wrapping is stable across handle lifetimes (the
    underlying bytes come from disk, not from the shim).
    """
    import uuid as _uuid

    with drevo.Drevo.open(tmp_db_path) as db:
        node = db.create_node(drevo.NewNode(kind="note", title="uuid-stable"))
        original_uuid = node.uuid
        assert isinstance(original_uuid, _uuid.UUID)
    with drevo.Drevo.open(tmp_db_path) as db:
        fetched = db.get_node_by_uuid(original_uuid.bytes)
        assert fetched is not None
        assert fetched.uuid == original_uuid


def test_node_lookup_by_title_after_reopen(tmp_db_path: str) -> None:
    """Title index is rebuilt at open time and lookup works on reopen."""
    with drevo.Drevo.open(tmp_db_path) as db:
        db.create_node(drevo.NewNode(kind="note", title="findable-by-title"))
    with drevo.Drevo.open(tmp_db_path) as db:
        fetched = db.get_node_by_title("findable-by-title")
        assert fetched is not None
        assert fetched.title == "findable-by-title"


def test_kind_index_survives_reopen(tmp_db_path: str) -> None:
    """`list_nodes_by_kind` returns the same rows after reopen.

    Specifically asserts the secondary index — not the primary table —
    survives. A previous regression had primary intact, index lost.
    """
    with drevo.Drevo.open(tmp_db_path) as db:
        for i in range(5):
            db.create_node(drevo.NewNode(kind="task", title=f"task-{i}"))
        db.create_node(drevo.NewNode(kind="note", title="note-1"))
    with drevo.Drevo.open(tmp_db_path) as db:
        tasks = db.list_nodes_by_kind("task", limit=100, offset=0)
        assert {n.title for n in tasks} == {f"task-{i}" for i in range(5)}


def test_fts_index_survives_reopen(tmp_db_path: str) -> None:
    """FTS posting lists are durable: a query made after reopen returns
    the same hits as one made before close.
    """
    with drevo.Drevo.open(tmp_db_path) as db:
        db.create_node(drevo.NewNode(kind="note", title="alpha", body="embedded graph"))
        db.create_node(drevo.NewNode(kind="note", title="beta", body="graph database"))
        db.create_node(drevo.NewNode(kind="note", title="gamma", body="unrelated"))
        before = {h.node.title for h in db.search_fts("graph", 10)}
    with drevo.Drevo.open(tmp_db_path) as db:
        after = {h.node.title for h in db.search_fts("graph", 10)}
    assert before == after == {"alpha", "beta"}


def test_deletes_persist_across_reopen(tmp_db_path: str) -> None:
    """A deleted node stays deleted after reopen — the tombstone /
    primary-row removal is committed, not just buffered.
    """
    with drevo.Drevo.open(tmp_db_path) as db:
        n = db.create_node(drevo.NewNode(kind="note", title="ephemeral"))
        db.delete_node(n.id)
        assert db.get_node(n.id) is None
    with drevo.Drevo.open(tmp_db_path) as db:
        assert db.get_node(n.id) is None


def test_allocator_continues_ids_after_reopen(tmp_db_path: str) -> None:
    """Node ids are monotonically increasing across reopens — the
    allocator persists its `next_id` cursor (not just the existing
    rows).
    """
    with drevo.Drevo.open(tmp_db_path) as db:
        first = db.create_node(drevo.NewNode(kind="note", title="first"))
    with drevo.Drevo.open(tmp_db_path) as db:
        second = db.create_node(drevo.NewNode(kind="note", title="second"))
    assert second.id > first.id


def test_property_values_round_trip_through_reopen(tmp_db_path: str) -> None:
    """Mixed-type properties survive serialise → flush → reopen.

    Strings, ints, floats, bools, None, nested dicts and lists must all
    deserialise to their Python equivalents.
    """
    payload = {
        "str": "hello",
        "int": 42,
        "float": 3.14,
        "bool": True,
        "none": None,
        "nested": {"k": [1, 2, 3]},
    }
    with drevo.Drevo.open(tmp_db_path) as db:
        node = db.create_node(drevo.NewNode(kind="note", title="mixed-props", properties=payload))
        nid = node.id
    with drevo.Drevo.open(tmp_db_path) as db:
        fetched = db.get_node(nid)
        assert fetched is not None
        for key, expected in payload.items():
            assert fetched.properties[key] == expected
