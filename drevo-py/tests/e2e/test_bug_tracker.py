"""End-to-end scenario: bug + regression tracker.

Mirrors ``tests/scenario_bug_tracker.rs`` at the Python surface.

Domain model:
    Node kinds: project, bug, component, person, release, test_case
    Edge kinds: affects, owns, reported_by, fixed_in, regresses,
                covers, reproduces

The scenario builds a small ledger: two bugs against one shared
component, one of them a regression against a prior release, with
test-case coverage and a triage assignment. The projections asserted
are the ones a triage engineer drives during an on-call rotation
(open bugs per component, regressions per release, coverage gap).
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


def _build_tracker(db: drevo.Drevo) -> dict[str, drevo.Node]:
    n: dict[str, drevo.Node] = {}

    n["project"] = db.create_node(_new("project", "drevo-tracker", "Bug tracker for drevo"))

    n["comp_fts"] = db.create_node(_new("component", "search-fts", area="indexing"))
    n["comp_traversal"] = db.create_node(_new("component", "traversal", area="query"))

    n["release_v1"] = db.create_node(_new("release", "v1.0.0", channel="stable"))
    n["release_v1_1"] = db.create_node(_new("release", "v1.1.0", channel="stable"))

    n["bug_fts_recall"] = db.create_node(
        _new(
            "bug",
            "bug-fts-misses-stop-words",
            "FTS search misses results containing common stop words",
            severity="high",
            status="open",
        )
    )
    n["bug_traversal_regression"] = db.create_node(
        _new(
            "bug",
            "bug-bfs-edge-kind-regression",
            "BFS edge-kind filter regression after refactor",
            severity="critical",
            status="open",
            regression=True,
        )
    )

    n["person_triager"] = db.create_node(_new("person", "ivy-rourke", role="qa-lead"))
    n["person_reporter"] = db.create_node(_new("person", "matt-yip", role="user"))
    n["person_owner"] = db.create_node(_new("person", "noor-akin", role="engineer"))

    n["test_fts_stopwords"] = db.create_node(_new("test_case", "tc-fts-stopwords", automated=True))
    n["test_bfs_filter"] = db.create_node(_new("test_case", "tc-bfs-edge-filter", automated=True))

    # Bug → component (affects)
    db.create_edge(_edge(n["bug_fts_recall"].id, n["comp_fts"].id, "affects"))
    db.create_edge(_edge(n["bug_traversal_regression"].id, n["comp_traversal"].id, "affects"))

    # Owner / reporter wiring
    db.create_edge(_edge(n["person_owner"].id, n["bug_fts_recall"].id, "owns"))
    db.create_edge(_edge(n["person_owner"].id, n["bug_traversal_regression"].id, "owns"))
    db.create_edge(_edge(n["bug_fts_recall"].id, n["person_reporter"].id, "reported_by"))
    db.create_edge(_edge(n["bug_traversal_regression"].id, n["person_reporter"].id, "reported_by"))

    # Regression marker: the traversal bug regresses v1.0.0
    db.create_edge(_edge(n["bug_traversal_regression"].id, n["release_v1"].id, "regresses"))

    # Fixes shipped (planned for v1.1.0)
    db.create_edge(_edge(n["bug_fts_recall"].id, n["release_v1_1"].id, "fixed_in"))
    db.create_edge(_edge(n["bug_traversal_regression"].id, n["release_v1_1"].id, "fixed_in"))

    # Coverage: test_case covers bug + reproduces it
    db.create_edge(_edge(n["test_fts_stopwords"].id, n["bug_fts_recall"].id, "covers"))
    db.create_edge(_edge(n["test_fts_stopwords"].id, n["bug_fts_recall"].id, "reproduces"))
    db.create_edge(_edge(n["test_bfs_filter"].id, n["bug_traversal_regression"].id, "covers"))

    # Triage rollup
    db.create_edge(_edge(n["project"].id, n["bug_fts_recall"].id, "affects"))
    db.create_edge(_edge(n["project"].id, n["bug_traversal_regression"].id, "affects"))

    return n


def test_tracker_census_matches_definition(disk_db: drevo.Drevo) -> None:
    _build_tracker(disk_db)
    assert len(disk_db.list_nodes_by_kind("project", limit=10, offset=0)) == 1
    assert len(disk_db.list_nodes_by_kind("bug", limit=10, offset=0)) == 2
    assert len(disk_db.list_nodes_by_kind("component", limit=10, offset=0)) == 2
    assert len(disk_db.list_nodes_by_kind("release", limit=10, offset=0)) == 2
    assert len(disk_db.list_nodes_by_kind("test_case", limit=10, offset=0)) == 2


def test_bugs_affecting_traversal_component(disk_db: drevo.Drevo) -> None:
    """Inbound ``affects`` edges on the component answer "what bugs
    are open against this code path?".
    """
    n = _build_tracker(disk_db)
    inbound = disk_db.edges_of(n["comp_traversal"].id, drevo.Direction.IN)
    bug_titles = {must_get_node(disk_db, e.from_id).title for e in inbound if e.kind == "affects"}
    assert bug_titles == {"bug-bfs-edge-kind-regression"}


def test_regression_bugs_per_release(disk_db: drevo.Drevo) -> None:
    """Inbound ``regresses`` edges on a release surface the regressions
    that release introduced.
    """
    n = _build_tracker(disk_db)
    inbound = disk_db.edges_of(n["release_v1"].id, drevo.Direction.IN)
    regression_titles = {
        must_get_node(disk_db, e.from_id).title for e in inbound if e.kind == "regresses"
    }
    assert regression_titles == {"bug-bfs-edge-kind-regression"}


def test_bugs_fixed_in_release_v1_1(disk_db: drevo.Drevo) -> None:
    """Inbound ``fixed_in`` edges on the release form the changelog."""
    n = _build_tracker(disk_db)
    inbound = disk_db.edges_of(n["release_v1_1"].id, drevo.Direction.IN)
    fixed = {must_get_node(disk_db, e.from_id).title for e in inbound if e.kind == "fixed_in"}
    assert fixed == {"bug-fts-misses-stop-words", "bug-bfs-edge-kind-regression"}


def test_uncovered_bug_detected_via_inverse_lookup(disk_db: drevo.Drevo) -> None:
    """A bug is "uncovered" if it has no inbound ``covers`` edge.
    Both bugs in this scenario *are* covered — assert that the inverse
    check finds zero gaps. Inserting a new bug without a test should
    surface; we verify that too.
    """
    n = _build_tracker(disk_db)
    for bug_key in ("bug_fts_recall", "bug_traversal_regression"):
        inbound = disk_db.edges_of(n[bug_key].id, drevo.Direction.IN)
        assert any(e.kind == "covers" for e in inbound)

    # Add an uncovered bug; the projection must immediately reflect the gap.
    new_bug = disk_db.create_node(
        _new("bug", "bug-no-coverage", "No test yet", severity="low", status="open")
    )
    inbound = disk_db.edges_of(new_bug.id, drevo.Direction.IN)
    assert not any(e.kind == "covers" for e in inbound)


def test_fts_finds_bug_by_body_phrase(disk_db: drevo.Drevo) -> None:
    """Body-text FTS recovers the bug — the "search the bug tracker"
    feature in an actual UI.
    """
    _build_tracker(disk_db)
    hits = disk_db.search_fts("regression", limit=10)
    titles = {h.node.title for h in hits}
    assert "bug-bfs-edge-kind-regression" in titles


def test_tracker_round_trips_through_reopen(tmp_db_path: str) -> None:
    """Persist mid-triage; reopen; the regression projection still
    points the triager at the right release.
    """
    with drevo.Drevo.open(tmp_db_path) as db:
        n = _build_tracker(db)
        v1_id = n["release_v1"].id
    with drevo.Drevo.open(tmp_db_path) as db2:
        inbound = db2.edges_of(v1_id, drevo.Direction.IN)
        assert any(e.kind == "regresses" for e in inbound)
