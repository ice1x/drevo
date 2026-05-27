"""Pagination boundary conditions on `list_*` over the real backend.

`Drevo.list_nodes_by_kind(kind, limit, offset)` and
`Drevo.list_edges_by_kind(kind, limit, offset)` are the surfaces an
agent uses to walk an unbounded result set in fixed-size pages. The
contract has three boundaries we pin:

1. `offset == 0` → first page; full `limit` rows returned if available.
2. `offset == len(corpus)` → exactly one empty page after the last row.
3. `offset > len(corpus)` → empty list, *not* an error.

Plus the invariant: concatenating all pages reproduces the full corpus
exactly once, in stable id order.
"""

from __future__ import annotations

from typing import Callable

import drevo


# ── list_nodes_by_kind ─────────────────────────────────────────────────


def test_list_nodes_first_page_returns_first_limit_rows(
    disk_db: drevo.Drevo, make_tag_corpus: Callable[[int, str], list[drevo.Node]]
) -> None:
    """Offset 0 returns the first `limit` rows in id order."""
    nodes = make_tag_corpus(20, "tag")
    page = disk_db.list_nodes_by_kind("tag", limit=5, offset=0)
    assert [n.id for n in page] == [n.id for n in nodes[:5]]


def test_list_nodes_offset_mid_corpus(
    disk_db: drevo.Drevo, make_tag_corpus: Callable[[int, str], list[drevo.Node]]
) -> None:
    """A mid-corpus offset returns the matching slice."""
    nodes = make_tag_corpus(20, "tag")
    page = disk_db.list_nodes_by_kind("tag", limit=5, offset=10)
    assert [n.id for n in page] == [n.id for n in nodes[10:15]]


def test_list_nodes_last_partial_page(
    disk_db: drevo.Drevo, make_tag_corpus: Callable[[int, str], list[drevo.Node]]
) -> None:
    """When the last page is short, only the remaining rows come back —
    no padding, no error.
    """
    nodes = make_tag_corpus(13, "tag")
    page = disk_db.list_nodes_by_kind("tag", limit=5, offset=10)
    assert len(page) == 3
    assert [n.id for n in page] == [n.id for n in nodes[10:13]]


def test_list_nodes_offset_at_end_returns_empty(
    disk_db: drevo.Drevo, make_tag_corpus: Callable[[int, str], list[drevo.Node]]
) -> None:
    """Offset exactly equal to corpus size returns `[]`."""
    make_tag_corpus(10, "tag")
    page = disk_db.list_nodes_by_kind("tag", limit=5, offset=10)
    assert page == []


def test_list_nodes_offset_past_end_returns_empty(
    disk_db: drevo.Drevo, make_tag_corpus: Callable[[int, str], list[drevo.Node]]
) -> None:
    """Offset *past* corpus size returns `[]`, never raises."""
    make_tag_corpus(10, "tag")
    page = disk_db.list_nodes_by_kind("tag", limit=5, offset=1000)
    assert page == []


def test_list_nodes_pagination_reassembles_full_corpus(
    disk_db: drevo.Drevo, make_tag_corpus: Callable[[int, str], list[drevo.Node]]
) -> None:
    """The full corpus = concat of all pages, in id order, no dupes."""
    nodes = make_tag_corpus(23, "tag")
    page_size = 7
    collected: list[drevo.Node] = []
    offset = 0
    while True:
        page = disk_db.list_nodes_by_kind("tag", limit=page_size, offset=offset)
        if not page:
            break
        collected.extend(page)
        offset += len(page)
    assert [n.id for n in collected] == [n.id for n in nodes]
    assert len({n.id for n in collected}) == 23


def test_list_nodes_kind_filter_excludes_other_kinds(
    disk_db: drevo.Drevo, make_tag_corpus: Callable[[int, str], list[drevo.Node]]
) -> None:
    """`list_nodes_by_kind("tag")` does not leak rows of other kinds."""
    make_tag_corpus(5, "tag")
    make_tag_corpus(5, "note")
    page = disk_db.list_nodes_by_kind("tag", limit=100, offset=0)
    assert {n.kind for n in page} == {"tag"}


def test_list_nodes_unknown_kind_returns_empty(disk_db: drevo.Drevo) -> None:
    """An unindexed kind returns `[]` rather than raising."""
    assert disk_db.list_nodes_by_kind("never-created", limit=10, offset=0) == []


# ── list_edges_by_kind ─────────────────────────────────────────────────


def test_list_edges_pagination_reassembles_full_corpus(disk_db: drevo.Drevo) -> None:
    """Edge pagination follows the same boundary rules as node
    pagination — concat of pages == full corpus.
    """
    a = disk_db.create_node(drevo.NewNode(kind="note", title="a"))
    targets = [
        disk_db.create_node(drevo.NewNode(kind="note", title=f"t-{i}"))
        for i in range(15)
    ]
    edges = [
        disk_db.create_edge(drevo.NewEdge(from_id=a.id, to_id=t.id, kind="links_to"))
        for t in targets
    ]
    collected: list[drevo.Edge] = []
    offset = 0
    while True:
        page = disk_db.list_edges_by_kind("links_to", limit=4, offset=offset)
        if not page:
            break
        collected.extend(page)
        offset += len(page)
    assert {e.id for e in collected} == {e.id for e in edges}
    assert len(collected) == len(edges)


# ── list_recent ────────────────────────────────────────────────────────


def test_list_recent_returns_most_recently_inserted_first(
    disk_db: drevo.Drevo, make_tag_corpus: Callable[[int, str], list[drevo.Node]]
) -> None:
    """`list_recent(limit)` returns the most-recent `limit` nodes,
    newest first, regardless of kind.
    """
    nodes = make_tag_corpus(10, "note")
    recent = disk_db.list_recent(3)
    assert [n.id for n in recent] == [n.id for n in nodes[-3:][::-1]]


def test_list_recent_caps_at_corpus_size(
    disk_db: drevo.Drevo, make_tag_corpus: Callable[[int, str], list[drevo.Node]]
) -> None:
    """Asking for more than exists returns the whole corpus."""
    make_tag_corpus(3, "note")
    recent = disk_db.list_recent(100)
    assert len(recent) == 3


def test_list_recent_on_empty_db_returns_empty(disk_db: drevo.Drevo) -> None:
    """An empty database returns `[]` for any positive limit."""
    assert disk_db.list_recent(10) == []
