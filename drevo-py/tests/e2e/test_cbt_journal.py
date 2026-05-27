"""End-to-end scenario: Cognitive Behavioural Therapy (CBT) journal.

Mirrors ``tests/scenario_cbt_journal.rs`` at the Python surface.

Domain model:
    Node kinds: situation, thought, emotion, cognitive_distortion,
                rational_response
    Edge kinds: triggered_by, leads_to, challenges, reframed_as

The scenario builds one full CBT entry, asserts every observable
output an agent or UI would surface (chain traversal, distortion
search, reframing path), then re-opens the database to confirm the
entry round-trips through redb.
"""

from __future__ import annotations

import drevo

# ── helpers ───────────────────────────────────────────────────────────


def _new(kind: str, title: str, body: str = "", **props: object) -> drevo.NewNode:
    return drevo.NewNode(
        kind=kind, title=title, body=body, properties=dict(props) if props else None
    )


def _edge(from_id: int, to_id: int, kind: str, weight: float = 1.0) -> drevo.NewEdge:
    return drevo.NewEdge(from_id=from_id, to_id=to_id, kind=kind, weight=weight)


def _build_entry(db: drevo.Drevo) -> dict[str, drevo.Node]:
    """Build one canonical CBT entry. Returns a name → Node mapping."""
    nodes = {
        "situation": db.create_node(
            _new("situation", "presentation-monday", "Big slide deck due Monday morning")
        ),
        "thought": db.create_node(
            _new(
                "thought",
                "i-will-freeze",
                "I will completely freeze up and embarrass myself",
            )
        ),
        "emotion_anxiety": db.create_node(
            _new("emotion", "anxiety-presentation", "anxious", intensity=8)
        ),
        "emotion_shame": db.create_node(
            _new("emotion", "shame-presentation", "ashamed", intensity=6)
        ),
        "distortion": db.create_node(
            _new(
                "cognitive_distortion",
                "fortune-telling-presentation",
                "Predicting catastrophe without evidence",
            )
        ),
        "rational": db.create_node(
            _new(
                "rational_response",
                "evidence-based-reframe",
                "I have prepared and run-throughs went well; freezing is unlikely",
            )
        ),
        "emotion_calm": db.create_node(_new("emotion", "calm-presentation", "calmer", intensity=3)),
    }

    # Build edges that link the entry into a chain the traversal tests
    # below can walk in either direction.
    db.create_edge(_edge(nodes["thought"].id, nodes["situation"].id, "triggered_by"))
    db.create_edge(_edge(nodes["thought"].id, nodes["emotion_anxiety"].id, "leads_to"))
    db.create_edge(_edge(nodes["thought"].id, nodes["emotion_shame"].id, "leads_to"))
    db.create_edge(_edge(nodes["rational"].id, nodes["distortion"].id, "challenges", weight=0.95))
    db.create_edge(_edge(nodes["thought"].id, nodes["rational"].id, "reframed_as"))
    db.create_edge(_edge(nodes["rational"].id, nodes["emotion_calm"].id, "leads_to"))
    db.create_edge(_edge(nodes["distortion"].id, nodes["thought"].id, "challenges"))

    return nodes


# ── scenario assertions ───────────────────────────────────────────────


def test_full_cbt_entry_lands_with_expected_kind_counts(disk_db: drevo.Drevo) -> None:
    """End-to-end build, then verify the per-kind node census."""
    _build_entry(disk_db)
    assert len(disk_db.list_nodes_by_kind("situation", limit=100, offset=0)) == 1
    assert len(disk_db.list_nodes_by_kind("thought", limit=100, offset=0)) == 1
    assert len(disk_db.list_nodes_by_kind("emotion", limit=100, offset=0)) == 3
    assert len(disk_db.list_nodes_by_kind("cognitive_distortion", limit=100, offset=0)) == 1
    assert len(disk_db.list_nodes_by_kind("rational_response", limit=100, offset=0)) == 1


def test_bfs_from_situation_reaches_calm_via_reframing_chain(
    disk_db: drevo.Drevo,
) -> None:
    """A deep BOTH-direction BFS from the situation surfaces every node
    on the rumination → reframing → calm chain.
    """
    nodes = _build_entry(disk_db)
    reached = disk_db.bfs(nodes["situation"].id, max_depth=5, direction=drevo.Direction.BOTH)
    titles = {n.title for n in reached}
    # Every node in the entry should be visited.
    assert "i-will-freeze" in titles
    assert "anxiety-presentation" in titles
    assert "shame-presentation" in titles
    assert "fortune-telling-presentation" in titles
    assert "evidence-based-reframe" in titles
    assert "calm-presentation" in titles


def test_shortest_path_thought_to_calm_runs_through_rational(
    disk_db: drevo.Drevo,
) -> None:
    """Thought → rational_response → emotion(calm). The shortest path
    must include the rational reframing node, otherwise the journal's
    "what helped me" projection breaks.
    """
    nodes = _build_entry(disk_db)
    path = disk_db.shortest_path(nodes["thought"].id, nodes["emotion_calm"].id)
    assert path is not None
    assert path == [nodes["thought"].id, nodes["rational"].id, nodes["emotion_calm"].id]


def test_fts_distortion_search_returns_fortune_telling(disk_db: drevo.Drevo) -> None:
    """An agent asking "which entries used fortune telling?" must find
    the entry via the body text indexed by FTS.
    """
    _build_entry(disk_db)
    hits = disk_db.search_fts("fortune", limit=10)
    assert any(h.node.kind == "cognitive_distortion" for h in hits), [
        (h.node.kind, h.node.title) for h in hits
    ]


def test_subgraph_from_thought_captures_reframing_neighborhood(
    disk_db: drevo.Drevo,
) -> None:
    """A 2-hop subgraph from the central thought must include the
    distortion, the rational response, both emotions, and the
    calm-after-reframe node — that's the journal-entry projection.
    """
    nodes = _build_entry(disk_db)
    sg = disk_db.subgraph(nodes["thought"].id, depth=2)
    kinds = sorted({n.kind for n in sg.nodes})
    assert kinds == [
        "cognitive_distortion",
        "emotion",
        "rational_response",
        "situation",
        "thought",
    ]


def test_entry_round_trips_through_close_and_reopen(tmp_db_path: str) -> None:
    """Durability check: write the full CBT entry, close, reopen,
    re-read the same per-kind census + FTS hit. If the kind index or
    FTS posting list fails to flush, this scenario fails loudly.
    """
    with drevo.Drevo.open(tmp_db_path) as db:
        _build_entry(db)
    with drevo.Drevo.open(tmp_db_path) as db2:
        # Per-kind census preserved.
        assert len(db2.list_nodes_by_kind("emotion", limit=100, offset=0)) == 3
        # Body-text FTS still finds the distortion.
        hits = db2.search_fts("catastrophe", limit=10)
        titles = {h.node.title for h in hits}
        assert "fortune-telling-presentation" in titles
