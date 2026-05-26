"""Unit tests for `drevo.rag.expand_neighborhood` (00118).

The neighbourhood routine is pure Python on top of `Drevo.edges_of` +
`Drevo.get_node`. These tests assert algorithmic contracts: depth caps,
node-count caps, kind-filter semantics, the root-exempt rule for
kind_filter, and the seed-type union (`uuid.UUID | bytes | int`).
"""

from __future__ import annotations

import uuid

import pytest

import drevo
from drevo.rag import Neighborhood, expand_neighborhood


def test_expand_neighborhood_returns_neighborhood_dataclass(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    nh = expand_neighborhood(drevo_db, connected_chain["a"].uuid, hops=1)
    assert isinstance(nh, Neighborhood)


def test_zero_hops_returns_only_root(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    nh = expand_neighborhood(drevo_db, connected_chain["a"].uuid, hops=0)
    assert [n.id for n in nh.nodes] == [connected_chain["a"].id]


def test_one_hop_includes_root_and_one_neighbour(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    nh = expand_neighborhood(drevo_db, connected_chain["a"].uuid, hops=1)
    assert {n.title for n in nh.nodes} == {"a", "b"}


def test_two_hops_reaches_two_levels(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    nh = expand_neighborhood(drevo_db, connected_chain["a"].uuid, hops=2)
    assert {n.title for n in nh.nodes} == {"a", "b", "c"}


def test_negative_hops_raises_value_error(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    with pytest.raises(ValueError):
        expand_neighborhood(drevo_db, connected_chain["a"].uuid, hops=-1)


def test_zero_max_nodes_raises_value_error(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    with pytest.raises(ValueError):
        expand_neighborhood(drevo_db, connected_chain["a"].uuid, hops=1, max_nodes=0)


def test_max_nodes_caps_visited_set(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    nh = expand_neighborhood(drevo_db, connected_chain["a"].uuid, hops=5, max_nodes=2)
    assert len(nh.nodes) == 2


def test_kind_filter_excludes_other_kinds(drevo_db: drevo.Drevo, mixed_kind_neighbourhood) -> None:
    root, others = mixed_kind_neighbourhood
    nh = expand_neighborhood(drevo_db, root.uuid, hops=1, kind_filter=["note"])
    kinds = {n.kind for n in nh.nodes}
    assert "tag" not in kinds and "person" not in kinds


def test_kind_filter_does_not_exclude_root(drevo_db: drevo.Drevo, mixed_kind_neighbourhood) -> None:
    """`kind_filter=['note']` constrains the *frontier* — the root,
    whose kind is `task`, is always returned (RFC neighborhood §)."""
    root, _ = mixed_kind_neighbourhood
    nh = expand_neighborhood(drevo_db, root.uuid, hops=1, kind_filter=["note"])
    assert any(n.id == root.id for n in nh.nodes)


def test_hops_used_reports_actual_depth_reached(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    """A 2-hop expansion on a 2-hop-reachable chain reports `hops_used == 2`."""
    nh = expand_neighborhood(drevo_db, connected_chain["a"].uuid, hops=2)
    assert nh.hops_used == 2


def test_hops_used_zero_when_graph_exhausted_before_cap(
    drevo_db: drevo.Drevo,
) -> None:
    """A 3-hop call against an isolated root reports `hops_used == 0`
    (no level discovered a new node).
    """
    isolated = drevo_db.create_node(drevo.NewNode(kind="note", title="solo"))
    nh = expand_neighborhood(drevo_db, isolated.uuid, hops=3)
    assert nh.hops_used == 0


def test_seed_accepts_raw_bytes(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    nh = expand_neighborhood(drevo_db, connected_chain["a"].uuid.bytes, hops=1)
    assert any(n.title == "a" for n in nh.nodes)


def test_seed_accepts_node_id(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    nh = expand_neighborhood(drevo_db, connected_chain["a"].id, hops=1)
    assert any(n.title == "a" for n in nh.nodes)


def test_seed_unknown_type_raises_type_error(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(TypeError):
        expand_neighborhood(drevo_db, 1.5, hops=1)  # type: ignore[arg-type]


def test_unknown_uuid_raises_value_error(drevo_db: drevo.Drevo) -> None:
    """A UUID with no corresponding node ⇒ caller asked for a missing
    root, which is a ValueError (cf. `Drevo.get_node_by_uuid` returns
    `None`, but the rag layer surfaces the missing root explicitly).
    """
    fake = uuid.UUID(bytes=bytes(16))
    with pytest.raises(ValueError):
        expand_neighborhood(drevo_db, fake, hops=1)
