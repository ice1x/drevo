"""Unit tests for `Drevo.bfs` / `dfs` / `shortest_path` / `subgraph` /
`neighbors` (00118).

The graph topologies come from `conftest.py` fixtures (`connected_chain`,
`mixed_kind_neighbourhood`). Each test asserts one traversal contract.
"""

from __future__ import annotations

import drevo

# ── BFS ──────────────────────────────────────────────────────────────


def test_bfs_depth_one_returns_immediate_neighbour(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    """a → b → c → d: BFS depth=1 from `a` returns the frontier `{b}`.

    The Rust traversal returns *discovered* nodes (the frontier) and
    does not re-emit the seed — RFC §3.3 documents `bfs(start, depth)`
    as "nodes reachable from start within depth edges, excluding start".
    """
    visited = drevo_db.bfs(connected_chain["a"].id, 1, drevo.Direction.OUT)
    assert {n.title for n in visited} == {"b"}


def test_bfs_depth_two_extends_one_more_hop(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    visited = drevo_db.bfs(connected_chain["a"].id, 2, drevo.Direction.OUT)
    assert {n.title for n in visited} == {"b", "c"}


def test_bfs_depth_zero_returns_empty(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    """Depth 0 = "no edges followed" — the frontier is empty."""
    visited = drevo_db.bfs(connected_chain["a"].id, 0, drevo.Direction.OUT)
    assert visited == []


def test_bfs_in_direction_walks_backwards(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    """From `d` with `Direction.IN` we reach `c` (d's predecessor)."""
    visited = drevo_db.bfs(connected_chain["d"].id, 1, drevo.Direction.IN)
    assert {n.title for n in visited} == {"c"}


def test_bfs_edge_kind_filter_only_follows_matching_edges(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    """No `unknown_kind` edges exist — depth=3 yields the empty frontier."""
    visited = drevo_db.bfs(
        connected_chain["a"].id, 3, drevo.Direction.OUT, edge_kind="unknown_kind"
    )
    assert visited == []


# ── DFS ──────────────────────────────────────────────────────────────


def test_dfs_returns_all_reachable_nodes(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    """DFS at sufficient depth reaches every descendant (excluding the
    seed, same convention as BFS).
    """
    visited = drevo_db.dfs(connected_chain["a"].id, 5, drevo.Direction.OUT)
    assert {n.title for n in visited} == {"b", "c", "d"}


# ── shortest_path ────────────────────────────────────────────────────


def test_shortest_path_returns_id_list(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    path = drevo_db.shortest_path(connected_chain["a"].id, connected_chain["d"].id)
    assert path == [
        connected_chain["a"].id,
        connected_chain["b"].id,
        connected_chain["c"].id,
        connected_chain["d"].id,
    ]


def test_shortest_path_returns_none_for_disconnected_nodes(drevo_db: drevo.Drevo) -> None:
    """Two isolated nodes have no path — the contract returns `None`."""
    a = drevo_db.create_node(drevo.NewNode(kind="note", title="iso-a"))
    b = drevo_db.create_node(drevo.NewNode(kind="note", title="iso-b"))
    assert drevo_db.shortest_path(a.id, b.id) is None


def test_shortest_path_self_is_singleton_list(drevo_db: drevo.Drevo) -> None:
    """`shortest_path(x, x)` is the one-element list `[x]`."""
    n = drevo_db.create_node(drevo.NewNode(kind="note", title="self"))
    assert drevo_db.shortest_path(n.id, n.id) == [n.id]


# ── subgraph ─────────────────────────────────────────────────────────


def test_subgraph_returns_subgraph_with_nodes_and_edges(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    sg = drevo_db.subgraph(connected_chain["a"].id, depth=2)
    assert isinstance(sg, drevo.SubGraph)
    assert any(n.id == connected_chain["a"].id for n in sg.nodes)


def test_subgraph_depth_one_returns_immediate_slice(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    sg = drevo_db.subgraph(connected_chain["a"].id, depth=1)
    titles = {n.title for n in sg.nodes}
    assert titles == {"a", "b"}


# ── neighbors ────────────────────────────────────────────────────────


def test_neighbors_out_direction_returns_out_neighbours(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    out = drevo_db.neighbors(connected_chain["a"].id, drevo.Direction.OUT)
    assert {n.title for n in out} == {"b"}


def test_neighbors_in_direction_returns_in_neighbours(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    inn = drevo_db.neighbors(connected_chain["b"].id, drevo.Direction.IN)
    assert {n.title for n in inn} == {"a"}


def test_neighbors_both_direction_returns_union(
    drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]
) -> None:
    both = drevo_db.neighbors(connected_chain["b"].id, drevo.Direction.BOTH)
    assert {n.title for n in both} == {"a", "c"}
