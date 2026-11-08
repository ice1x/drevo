"""Unit tests for `CALL drevo.search.hybrid` from Python — BM25 and vector
retrieval fused with Reciprocal Rank Fusion inside the database.

The embedded handle has no text embedder, so `mode: 'auto'` runs the BM25
leg alone and says so in the `mode` column; an explicit `'hybrid'` raises.
"""

from __future__ import annotations

import pytest
from faker import Faker

import drevo

_HYBRID = (
    "CALL drevo.search.hybrid($q, 'Doc', 'embedding', $k, $config) "
    "YIELD node, score, ftsRank, vectorRank, mode "
    "RETURN node.title AS title, score, ftsRank, vectorRank, mode"
)


@pytest.fixture
def word(drevo_db: drevo.Drevo, fake: Faker) -> str:
    """A made-up word in the titles of two documents, and a third without it."""
    made_up: str = fake.unique.lexify("zq??????").lower()
    drevo_db.execute(
        "CREATE (:Doc {title: $a, embedding: [1.0, 0.0]}), "
        "(:Doc {title: $b, embedding: [0.0, 1.0]}), "
        "(:Doc {title: $c, embedding: [0.5, 0.5]})",
        {
            "a": f"{made_up} {fake.unique.last_name()}",
            "b": f"{made_up} {fake.unique.city()} {fake.unique.last_name()}",
            "c": fake.unique.city(),
        },
    )
    return made_up


def test_auto_without_an_embedder_runs_bm25(drevo_db: drevo.Drevo, word: str) -> None:
    rows = drevo_db.execute(_HYBRID, {"q": word, "k": 5, "config": {}}).rows
    assert len(rows) == 2
    assert all(word in r["title"] for r in rows)
    assert [r["ftsRank"] for r in rows] == [1, 2]
    assert all(isinstance(r["ftsRank"], int) for r in rows)
    assert all(r["vectorRank"] is None for r in rows)
    assert all(isinstance(r["score"], float) for r in rows)
    assert {r["mode"] for r in rows} == {"fts"}
    assert rows[0]["score"] > rows[1]["score"]


def test_k_limits_the_rows(drevo_db: drevo.Drevo, word: str) -> None:
    rows = drevo_db.execute(_HYBRID, {"q": word, "k": 1, "config": None}).rows
    assert len(rows) == 1
    assert rows[0]["ftsRank"] == 1


def test_hybrid_without_an_embedder_raises(drevo_db: drevo.Drevo, word: str) -> None:
    with pytest.raises(drevo.CypherError, match="semantic embedding"):
        drevo_db.execute(_HYBRID, {"q": word, "k": 5, "config": {"mode": "hybrid"}})


def test_an_unknown_config_key_raises(drevo_db: drevo.Drevo, word: str) -> None:
    with pytest.raises(drevo.CypherError, match="topK"):
        drevo_db.execute(_HYBRID, {"q": word, "k": 5, "config": {"topK": 3}})
