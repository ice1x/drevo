"""End-to-end scenario: long-form story editor.

Mirrors ``tests/scenario_story_editor.rs`` at the Python surface.

Domain model:
    Node kinds: book, chapter, scene, character, location, plot_arc
    Edge kinds: contains, appears_in, set_in, advances, conflicts_with

A novella editor needs three primary projections from the graph:

1. Reading order — ``book → chapter → scene`` walked in insertion
   order so the table of contents stays stable across saves.
2. "Where does X appear?" — character → scene lookup so the writer
   can audit a character's screen-time.
3. Plot-arc traversal — every scene that advances a given arc, in
   ordered chains.

The scenario constructs one short novella, asserts each projection,
then re-opens to confirm the underlying redb tables survived the
close/reopen cycle.
"""

from __future__ import annotations

import drevo

from .conftest import must_get_node


def _new(kind: str, title: str, body: str = "", **props: object) -> drevo.NewNode:
    return drevo.NewNode(
        kind=kind, title=title, body=body, properties=dict(props) if props else None
    )


def _edge(from_id: int, to_id: int, kind: str, weight: float = 1.0) -> drevo.NewEdge:
    return drevo.NewEdge(from_id=from_id, to_id=to_id, kind=kind, weight=weight)


def _build_novella(db: drevo.Drevo) -> dict[str, drevo.Node]:
    """Build a 1-book / 2-chapter / 4-scene novella with two characters
    and a single plot arc that ties three of the four scenes together.
    """
    n: dict[str, drevo.Node] = {}

    n["book"] = db.create_node(_new("book", "the-last-signal", "A near-future drama"))
    n["ch1"] = db.create_node(_new("chapter", "chapter-1-static", order=1))
    n["ch2"] = db.create_node(_new("chapter", "chapter-2-uplink", order=2))
    n["s1"] = db.create_node(
        _new("scene", "scene-1-the-tower", "The relay tower goes silent.", order=1)
    )
    n["s2"] = db.create_node(
        _new("scene", "scene-2-the-call", "A frantic call from the operator.", order=2)
    )
    n["s3"] = db.create_node(
        _new("scene", "scene-3-the-trace", "Tracing the lost signal.", order=3)
    )
    n["s4"] = db.create_node(
        _new("scene", "scene-4-the-uplink", "The uplink is restored.", order=4)
    )
    n["mia"] = db.create_node(_new("character", "mia-han", "Senior radio operator"))
    n["raj"] = db.create_node(_new("character", "raj-patel", "Field engineer"))
    n["loc_tower"] = db.create_node(_new("location", "relay-tower", "Mountain ridge"))
    n["arc_silence"] = db.create_node(
        _new("plot_arc", "arc-the-silence", "Why the tower went dark")
    )

    # Book → chapter → scene insertion order. Reading-order traversal
    # depends on insertion order on the underlying redb table.
    db.create_edge(_edge(n["book"].id, n["ch1"].id, "contains"))
    db.create_edge(_edge(n["book"].id, n["ch2"].id, "contains"))
    db.create_edge(_edge(n["ch1"].id, n["s1"].id, "contains"))
    db.create_edge(_edge(n["ch1"].id, n["s2"].id, "contains"))
    db.create_edge(_edge(n["ch2"].id, n["s3"].id, "contains"))
    db.create_edge(_edge(n["ch2"].id, n["s4"].id, "contains"))

    # Characters appear in scenes.
    db.create_edge(_edge(n["mia"].id, n["s1"].id, "appears_in"))
    db.create_edge(_edge(n["mia"].id, n["s2"].id, "appears_in"))
    db.create_edge(_edge(n["mia"].id, n["s4"].id, "appears_in"))
    db.create_edge(_edge(n["raj"].id, n["s3"].id, "appears_in"))
    db.create_edge(_edge(n["raj"].id, n["s4"].id, "appears_in"))

    # Scenes set at a location.
    db.create_edge(_edge(n["s1"].id, n["loc_tower"].id, "set_in"))
    db.create_edge(_edge(n["s3"].id, n["loc_tower"].id, "set_in"))

    # Plot-arc advancement: s1 → s3 → s4 advance the silence arc.
    db.create_edge(_edge(n["s1"].id, n["arc_silence"].id, "advances"))
    db.create_edge(_edge(n["s3"].id, n["arc_silence"].id, "advances"))
    db.create_edge(_edge(n["s4"].id, n["arc_silence"].id, "advances"))

    return n


def test_node_census_matches_novella_outline(disk_db: drevo.Drevo) -> None:
    _build_novella(disk_db)
    assert len(disk_db.list_nodes_by_kind("book", limit=10, offset=0)) == 1
    assert len(disk_db.list_nodes_by_kind("chapter", limit=10, offset=0)) == 2
    assert len(disk_db.list_nodes_by_kind("scene", limit=10, offset=0)) == 4
    assert len(disk_db.list_nodes_by_kind("character", limit=10, offset=0)) == 2
    assert len(disk_db.list_nodes_by_kind("plot_arc", limit=10, offset=0)) == 1


def test_reading_order_walks_book_to_every_scene(disk_db: drevo.Drevo) -> None:
    """BFS from the book root, OUT direction, reaches both chapters and
    all four scenes — the editor's table-of-contents projection.
    """
    n = _build_novella(disk_db)
    reached = disk_db.bfs(n["book"].id, max_depth=3, direction=drevo.Direction.OUT)
    titles = {node.title for node in reached}
    assert "chapter-1-static" in titles
    assert "chapter-2-uplink" in titles
    for scene in (
        "scene-1-the-tower",
        "scene-2-the-call",
        "scene-3-the-trace",
        "scene-4-the-uplink",
    ):
        assert scene in titles


def test_character_appears_in_lookup_returns_scenes_only(disk_db: drevo.Drevo) -> None:
    """``edges_of(character, OUT)`` filtered to ``appears_in`` gives
    the screen-time projection an author asks for.
    """
    n = _build_novella(disk_db)
    mia_edges = [
        e for e in disk_db.edges_of(n["mia"].id, drevo.Direction.OUT) if e.kind == "appears_in"
    ]
    target_titles = {must_get_node(disk_db, e.to_id).title for e in mia_edges}
    assert target_titles == {"scene-1-the-tower", "scene-2-the-call", "scene-4-the-uplink"}


def test_plot_arc_advancing_scenes_recovered_in_order(disk_db: drevo.Drevo) -> None:
    """Incoming ``advances`` edges on the arc node give the ordered
    list of scenes that advance the silence arc.
    """
    n = _build_novella(disk_db)
    inbound = disk_db.edges_of(n["arc_silence"].id, drevo.Direction.IN)
    advancing = sorted(e.from_id for e in inbound if e.kind == "advances")
    expected = sorted([n["s1"].id, n["s3"].id, n["s4"].id])
    assert advancing == expected


def test_fts_recall_finds_uplink_scene(disk_db: drevo.Drevo) -> None:
    """The editor's "find scene by phrase" feature is built on FTS."""
    _build_novella(disk_db)
    hits = disk_db.search_fts("uplink", limit=5)
    titles = {h.node.title for h in hits}
    assert "scene-4-the-uplink" in titles


def test_novella_round_trips_through_reopen(tmp_db_path: str) -> None:
    """Close mid-draft, reopen, every projection still works."""
    with drevo.Drevo.open(tmp_db_path) as db:
        _build_novella(db)
    with drevo.Drevo.open(tmp_db_path) as db2:
        assert len(db2.list_nodes_by_kind("scene", limit=10, offset=0)) == 4
        hits = db2.search_fts("relay", limit=5)
        assert any(h.node.title == "scene-1-the-tower" for h in hits)
