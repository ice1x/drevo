"""Unit tests for `Drevo.search_fts` (00118).

The FTS surface is small — query string + limit, returns
`list[ScoredNode]` — but the contract has subtle edges (empty query,
limit truncation, no-match) that the integration suite (00119) cannot
exercise efficiently. One assertion per case.
"""

from __future__ import annotations

import pytest

import drevo


@pytest.fixture
def fts_corpus(drevo_db: drevo.Drevo) -> drevo.Drevo:
    """Three nodes with distinctive bodies; same handle returned for
    chained assertions in individual tests.
    """
    drevo_db.create_node(drevo.NewNode(kind="note", title="alpha", body="redb embedded graph"))
    drevo_db.create_node(drevo.NewNode(kind="note", title="beta", body="graph database in rust"))
    drevo_db.create_node(drevo.NewNode(kind="note", title="gamma", body="unrelated content here"))
    return drevo_db


def test_search_fts_returns_scored_node_instances(fts_corpus: drevo.Drevo) -> None:
    hits = fts_corpus.search_fts("graph", 10)
    assert all(isinstance(h, drevo.ScoredNode) for h in hits)


def test_search_fts_finds_matching_body(fts_corpus: drevo.Drevo) -> None:
    hits = fts_corpus.search_fts("graph", 10)
    titles = {h.node.title for h in hits}
    assert titles == {"alpha", "beta"}


def test_search_fts_respects_limit(fts_corpus: drevo.Drevo) -> None:
    hits = fts_corpus.search_fts("graph", 1)
    assert len(hits) == 1


def test_search_fts_no_match_returns_empty_list(fts_corpus: drevo.Drevo) -> None:
    """A query that matches nothing returns `[]`, not `None`."""
    assert fts_corpus.search_fts("zzz-no-such-token", 10) == []


def test_search_fts_score_is_finite_float(fts_corpus: drevo.Drevo) -> None:
    """Every hit ships a finite TF-IDF score (no `nan` / `inf`)."""
    import math

    hits = fts_corpus.search_fts("graph", 10)
    for h in hits:
        assert math.isfinite(h.score)
