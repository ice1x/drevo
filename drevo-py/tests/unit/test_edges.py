"""Unit tests for `Drevo` edge CRUD + listing (00118).

Same boundary discipline as `test_nodes.py`: one focused assertion per
case, real in-memory backend, no graph-shape fixtures (those live in
`test_traversal.py`).
"""

from __future__ import annotations

import math
import uuid

import pytest

import drevo


# ── shared two-node setup ────────────────────────────────────────────


@pytest.fixture
def two_nodes(drevo_db: drevo.Drevo) -> tuple[drevo.Node, drevo.Node]:
    """`(src, dst)` — a pair of bare `note` nodes for edge tests."""
    src = drevo_db.create_node(drevo.NewNode(kind="note", title="src"))
    dst = drevo_db.create_node(drevo.NewNode(kind="note", title="dst"))
    return src, dst


# ── create_edge ──────────────────────────────────────────────────────


def test_create_edge_returns_edge_instance(
    drevo_db: drevo.Drevo, two_nodes: tuple[drevo.Node, drevo.Node]
) -> None:
    src, dst = two_nodes
    edge = drevo_db.create_edge(drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to"))
    assert isinstance(edge, drevo.Edge)


def test_create_edge_assigns_positive_id(
    drevo_db: drevo.Drevo, two_nodes: tuple[drevo.Node, drevo.Node]
) -> None:
    src, dst = two_nodes
    edge = drevo_db.create_edge(drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to"))
    assert edge.id >= 1


def test_create_edge_default_weight_is_one(
    drevo_db: drevo.Drevo, two_nodes: tuple[drevo.Node, drevo.Node]
) -> None:
    """RFC default: an edge without an explicit weight gets `1.0`."""
    src, dst = two_nodes
    edge = drevo_db.create_edge(drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to"))
    assert edge.weight == pytest.approx(1.0)


def test_create_edge_custom_weight_round_trips(
    drevo_db: drevo.Drevo, two_nodes: tuple[drevo.Node, drevo.Node]
) -> None:
    src, dst = two_nodes
    edge = drevo_db.create_edge(
        drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to", weight=3.5)
    )
    assert edge.weight == pytest.approx(3.5)


def test_create_edge_assigns_uuid(
    drevo_db: drevo.Drevo, two_nodes: tuple[drevo.Node, drevo.Node]
) -> None:
    src, dst = two_nodes
    edge = drevo_db.create_edge(drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to"))
    assert isinstance(edge.uuid, uuid.UUID)


def test_create_edge_stores_properties(
    drevo_db: drevo.Drevo, two_nodes: tuple[drevo.Node, drevo.Node]
) -> None:
    src, dst = two_nodes
    edge = drevo_db.create_edge(
        drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to", properties={"score": 0.9})
    )
    assert edge.properties == {"score": 0.9}


@pytest.mark.parametrize("bad_weight", [float("nan"), math.inf, -math.inf])
def test_create_edge_invalid_weight_raises_invalid_weight_error(
    drevo_db: drevo.Drevo,
    two_nodes: tuple[drevo.Node, drevo.Node],
    bad_weight: float,
) -> None:
    """RFC §5.3 — non-finite weights surface `InvalidWeightError` (≤ ValueError)."""
    src, dst = two_nodes
    with pytest.raises(ValueError):
        drevo_db.create_edge(
            drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to", weight=bad_weight)
        )


# ── get_edge / get_edge_by_uuid ──────────────────────────────────────


def test_get_edge_returns_existing(
    drevo_db: drevo.Drevo, two_nodes: tuple[drevo.Node, drevo.Node]
) -> None:
    src, dst = two_nodes
    saved = drevo_db.create_edge(drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to"))
    fetched = drevo_db.get_edge(saved.id)
    assert fetched is not None
    assert fetched.id == saved.id


def test_get_edge_returns_none_for_missing(drevo_db: drevo.Drevo) -> None:
    assert drevo_db.get_edge(99_999) is None


def test_get_edge_by_uuid_round_trip(
    drevo_db: drevo.Drevo, two_nodes: tuple[drevo.Node, drevo.Node]
) -> None:
    src, dst = two_nodes
    saved = drevo_db.create_edge(drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to"))
    fetched = drevo_db.get_edge_by_uuid(saved.uuid.bytes)
    assert fetched is not None
    assert fetched.id == saved.id


# ── update_edge / delete_edge ────────────────────────────────────────


def test_update_edge_patches_weight(
    drevo_db: drevo.Drevo, two_nodes: tuple[drevo.Node, drevo.Node]
) -> None:
    src, dst = two_nodes
    saved = drevo_db.create_edge(
        drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to", weight=1.0)
    )
    patched = drevo_db.update_edge(saved.id, drevo.EdgePatch(weight=4.2))
    assert patched.weight == pytest.approx(4.2)


def test_update_edge_missing_raises_edge_not_found(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(drevo.EdgeNotFoundError):
        drevo_db.update_edge(99_999, drevo.EdgePatch(weight=1.0))


def test_delete_edge_removes_existing(
    drevo_db: drevo.Drevo, two_nodes: tuple[drevo.Node, drevo.Node]
) -> None:
    src, dst = two_nodes
    saved = drevo_db.create_edge(drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to"))
    drevo_db.delete_edge(saved.id)
    assert drevo_db.get_edge(saved.id) is None


def test_delete_edge_missing_raises_edge_not_found(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(drevo.EdgeNotFoundError):
        drevo_db.delete_edge(99_999)


# ── edges_of / list_edges_by_kind ────────────────────────────────────


def test_edges_of_out_direction_filters_outgoing(
    drevo_db: drevo.Drevo, two_nodes: tuple[drevo.Node, drevo.Node]
) -> None:
    src, dst = two_nodes
    drevo_db.create_edge(drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to"))
    out_edges = drevo_db.edges_of(src.id, drevo.Direction.OUT)
    assert {e.to_id for e in out_edges} == {dst.id}


def test_edges_of_in_direction_filters_incoming(
    drevo_db: drevo.Drevo, two_nodes: tuple[drevo.Node, drevo.Node]
) -> None:
    src, dst = two_nodes
    drevo_db.create_edge(drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to"))
    in_edges = drevo_db.edges_of(dst.id, drevo.Direction.IN)
    assert {e.from_id for e in in_edges} == {src.id}


def test_edges_of_both_direction_returns_union(
    drevo_db: drevo.Drevo, two_nodes: tuple[drevo.Node, drevo.Node]
) -> None:
    src, dst = two_nodes
    drevo_db.create_edge(drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to"))
    drevo_db.create_edge(drevo.NewEdge(from_id=dst.id, to_id=src.id, kind="links_to"))
    both = drevo_db.edges_of(src.id, drevo.Direction.BOTH)
    assert len(both) == 2


def test_list_edges_by_kind_filters_by_kind(
    drevo_db: drevo.Drevo, two_nodes: tuple[drevo.Node, drevo.Node]
) -> None:
    src, dst = two_nodes
    drevo_db.create_edge(drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="cites"))
    drevo_db.create_edge(drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to"))
    cites = drevo_db.list_edges_by_kind("cites", 100, 0)
    assert all(e.kind == "cites" for e in cites)
    assert len(cites) == 1
