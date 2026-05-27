"""Traversal correctness driven from the real redb edge index.

Unit tests assert one traversal contract per case on a small in-memory
chain. Integration tests pin the *combined* behaviour: BFS/DFS over
the on-disk adjacency, edge-kind filters interacting with the secondary
index, and traversals that survive a close-and-reopen.

If any of these fail, an agent running `bfs` / `subgraph` against a
durable knowledge graph either misses reachable nodes or visits stale
ones.
"""

from __future__ import annotations

import drevo


def _build_diamond(db: drevo.Drevo) -> dict[str, drevo.Node]:
    """    a → b → d
            ↘     ↗
              c

    Two paths from `a` to `d`. Lets us check edge-kind filters
    independently per branch (b-branch is "links_to", c-branch is
    "tagged_with").
    """
    nodes = {
        name: db.create_node(drevo.NewNode(kind="note", title=name))
        for name in ("a", "b", "c", "d")
    }
    db.create_edge(drevo.NewEdge(from_id=nodes["a"].id, to_id=nodes["b"].id, kind="links_to"))
    db.create_edge(drevo.NewEdge(from_id=nodes["a"].id, to_id=nodes["c"].id, kind="tagged_with"))
    db.create_edge(drevo.NewEdge(from_id=nodes["b"].id, to_id=nodes["d"].id, kind="links_to"))
    db.create_edge(drevo.NewEdge(from_id=nodes["c"].id, to_id=nodes["d"].id, kind="tagged_with"))
    return nodes


# ── BFS / DFS over disk-backed adjacency ──────────────────────────────


def test_bfs_walks_diamond_via_both_branches(disk_db: drevo.Drevo) -> None:
    """Unfiltered BFS reaches every descendant of the diamond root."""
    nodes = _build_diamond(disk_db)
    reached = disk_db.bfs(nodes["a"].id, max_depth=5, direction=drevo.Direction.OUT)
    assert {n.title for n in reached} == {"b", "c", "d"}


def test_bfs_edge_kind_filter_isolates_one_branch(disk_db: drevo.Drevo) -> None:
    """Filtering to `links_to` confines BFS to the b-branch."""
    nodes = _build_diamond(disk_db)
    reached = disk_db.bfs(
        nodes["a"].id, max_depth=5, direction=drevo.Direction.OUT, edge_kind="links_to"
    )
    assert {n.title for n in reached} == {"b", "d"}


def test_dfs_returns_same_reachable_set_as_bfs(disk_db: drevo.Drevo) -> None:
    """For the diamond, the *set* of visited nodes is identical between
    BFS and DFS — order differs, set does not.
    """
    nodes = _build_diamond(disk_db)
    bfs = {n.id for n in disk_db.bfs(nodes["a"].id, 5, drevo.Direction.OUT)}
    dfs = {n.id for n in disk_db.dfs(nodes["a"].id, 5, drevo.Direction.OUT)}
    assert bfs == dfs


def test_shortest_path_picks_minimum_hop_path(disk_db: drevo.Drevo) -> None:
    """Both branches are two hops; `shortest_path` returns a 3-node id
    list (a → ? → d), proving it stopped at the first minimum.
    """
    nodes = _build_diamond(disk_db)
    path = disk_db.shortest_path(nodes["a"].id, nodes["d"].id)
    assert path is not None
    assert len(path) == 3
    assert path[0] == nodes["a"].id
    assert path[-1] == nodes["d"].id


def test_shortest_path_respects_edge_kind_filter(disk_db: drevo.Drevo) -> None:
    """Filtering to `links_to` forces the b-branch path."""
    nodes = _build_diamond(disk_db)
    path = disk_db.shortest_path(nodes["a"].id, nodes["d"].id, edge_kind="links_to")
    assert path == [nodes["a"].id, nodes["b"].id, nodes["d"].id]


def test_subgraph_emits_all_diamond_nodes_and_edges(disk_db: drevo.Drevo) -> None:
    """A bounded-depth subgraph of the diamond root includes every node
    and every outgoing edge.
    """
    nodes = _build_diamond(disk_db)
    sg = disk_db.subgraph(nodes["a"].id, depth=5)
    assert {n.title for n in sg.nodes} == {"a", "b", "c", "d"}
    assert len(sg.edges) == 4


# ── edges_of / neighbors over disk-backed adjacency ────────────────────


def test_edges_of_out_returns_both_outgoing_edges(disk_db: drevo.Drevo) -> None:
    """`a` has two outgoing edges (links_to b, tagged_with c)."""
    nodes = _build_diamond(disk_db)
    out = disk_db.edges_of(nodes["a"].id, drevo.Direction.OUT)
    assert {(e.from_id, e.to_id, e.kind) for e in out} == {
        (nodes["a"].id, nodes["b"].id, "links_to"),
        (nodes["a"].id, nodes["c"].id, "tagged_with"),
    }


def test_edges_of_in_returns_both_incoming_edges(disk_db: drevo.Drevo) -> None:
    """`d` has two incoming edges from the diamond's two branches."""
    nodes = _build_diamond(disk_db)
    inn = disk_db.edges_of(nodes["d"].id, drevo.Direction.IN)
    assert {(e.from_id, e.to_id) for e in inn} == {
        (nodes["b"].id, nodes["d"].id),
        (nodes["c"].id, nodes["d"].id),
    }


def test_neighbors_both_returns_union_of_directions(disk_db: drevo.Drevo) -> None:
    """`b` is reachable from `a` (in) and reaches `d` (out); BOTH yields
    both as the neighbour set.
    """
    nodes = _build_diamond(disk_db)
    both = disk_db.neighbors(nodes["b"].id, drevo.Direction.BOTH)
    assert {n.title for n in both} == {"a", "d"}


# ── traversal survives reopen ──────────────────────────────────────────


def test_bfs_survives_close_and_reopen(tmp_db_path: str) -> None:
    """The edge index is rebuilt on open; BFS run after reopen sees the
    same frontier as before close.
    """
    with drevo.Drevo.open(tmp_db_path) as db:
        nodes = _build_diamond(db)
        before = {n.id for n in db.bfs(nodes["a"].id, 5, drevo.Direction.OUT)}
        seed_id = nodes["a"].id
    with drevo.Drevo.open(tmp_db_path) as db:
        after = {n.id for n in db.bfs(seed_id, 5, drevo.Direction.OUT)}
    assert before == after


def test_shortest_path_survives_close_and_reopen(tmp_db_path: str) -> None:
    """`shortest_path` returns the same id list after a process restart."""
    with drevo.Drevo.open(tmp_db_path) as db:
        nodes = _build_diamond(db)
        before = db.shortest_path(nodes["a"].id, nodes["d"].id, edge_kind="links_to")
        from_id, to_id = nodes["a"].id, nodes["d"].id
    with drevo.Drevo.open(tmp_db_path) as db:
        after = db.shortest_path(from_id, to_id, edge_kind="links_to")
    assert before == after


def test_cascade_delete_removes_incident_edges(disk_db: drevo.Drevo) -> None:
    """Deleting a node removes its incident edges; the surviving
    neighbour reports it as unreachable.
    """
    nodes = _build_diamond(disk_db)
    disk_db.delete_node(nodes["b"].id)
    # b is gone, so the links_to branch breaks; only the tagged_with
    # branch can still reach d.
    reached = disk_db.bfs(
        nodes["a"].id, 5, drevo.Direction.OUT, edge_kind="links_to"
    )
    assert reached == []
    # The tagged_with branch is intact.
    reached_tagged = disk_db.bfs(
        nodes["a"].id, 5, drevo.Direction.OUT, edge_kind="tagged_with"
    )
    assert {n.title for n in reached_tagged} == {"c", "d"}
