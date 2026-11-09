"""Unit tests for scoring retrieval strategies from Python:
`drevo.rag.evaluate` and `drevo.rag.evaluateSummary` through the Cypher API.

The fixture is a bug tracker: a ticket whose title holds a made-up word only
it carries (so BM25 finds it first), its crash log and the commit that fixes
it. The crash log and the commit share no words with the question, so only a
walk from the ticket reaches them. There is no embedder, so `'vector'` is not
among the default strategies.
"""

from __future__ import annotations

import pytest
from faker import Faker

import drevo

_EVALUATE = (
    "CALL drevo.rag.evaluate($questions, $config) "
    "YIELD question, strategy, hit, recall, precision, mrr, tokens, durationMs, "
    "nodeIds, recallRetrieved "
    "RETURN question, strategy, hit, recall, precision, mrr, tokens, durationMs, "
    "nodeIds, recallRetrieved"
)

_SUMMARY = (
    "CALL drevo.rag.evaluateSummary($questions, $config) "
    "YIELD strategy, questions, hitRate, meanRecall, meanPrecision, mrr, p50Ms, "
    "p95Ms, meanTokens "
    "RETURN strategy, questions, hitRate, meanRecall, meanPrecision, mrr, p50Ms, "
    "p95Ms, meanTokens"
)


@pytest.fixture
def tracker(drevo_db: drevo.Drevo, fake: Faker) -> dict[str, str]:
    """`commit -FIXES-> ticket -HAS_CRASH-> crash`."""
    word: str = fake.unique.lexify("zq??????").lower()
    names = {
        "word": word,
        "ticket": f"{word} {fake.unique.word()}",
        "crash": fake.unique.sentence(nb_words=4),
        "commit": fake.unique.sentence(nb_words=4),
    }
    drevo_db.execute(
        "CREATE (t:Ticket {name: $ticket}), (c:CrashLog {name: $crash}), "
        "(f:Commit {name: $commit}), (t)-[:HAS_CRASH]->(c), (f)-[:FIXES]->(t)",
        names,
    )
    return names


def _question(names: dict[str, str]) -> dict:
    return {
        "query": names["word"],
        "expected": [
            {"name": names["ticket"], "label": "Ticket"},
            {"name": names["crash"], "label": "CrashLog"},
            {"name": names["commit"], "label": "Commit"},
        ],
    }


def test_expansion_finds_what_search_alone_misses(
    drevo_db: drevo.Drevo, tracker: dict[str, str]
) -> None:
    rows = drevo_db.execute(
        _EVALUATE,
        {
            "questions": [_question(tracker)],
            "config": {"strategies": ["fts", "rag"], "k": 1},
        },
    ).rows
    by_strategy = {row["strategy"]: row for row in rows}
    assert list(by_strategy) == ["fts", "rag"]
    assert by_strategy["fts"]["recall"] == pytest.approx(1 / 3)
    assert by_strategy["rag"]["recall"] == 1.0
    for row in rows:
        assert row["question"] == tracker["word"]
        assert row["hit"] is True
        assert row["mrr"] == 1.0
        assert 0 < row["tokens"] <= 2000
        assert row["durationMs"] >= 0
        assert row["recallRetrieved"] >= row["recall"]
        assert all(isinstance(i, int) for i in row["nodeIds"])


def test_the_default_strategies_without_an_embedder(
    drevo_db: drevo.Drevo, tracker: dict[str, str]
) -> None:
    rows = drevo_db.execute(_SUMMARY, {"questions": [_question(tracker)], "config": None}).rows
    assert [row["strategy"] for row in rows] == ["fts", "hybrid", "hybrid+expand", "rag"]
    for row in rows:
        assert row["questions"] == 1
        assert row["hitRate"] == 1.0
        assert 0.0 <= row["p50Ms"] <= row["p95Ms"]


def test_errors_are_named(drevo_db: drevo.Drevo, tracker: dict[str, str], fake: Faker) -> None:
    missing: str = fake.unique.sentence(nb_words=3)
    with pytest.raises(drevo.CypherError, match="1 expected node not found"):
        drevo_db.execute(
            _EVALUATE,
            {
                "questions": [{"query": tracker["word"], "expected": [{"name": missing}]}],
                "config": None,
            },
        )
    with pytest.raises(drevo.CypherError, match="unknown strategy 'graph'"):
        drevo_db.execute(
            _EVALUATE,
            {"questions": [_question(tracker)], "config": {"strategies": ["graph"]}},
        )
    with pytest.raises(drevo.CypherError, match="needs a server-side embedder"):
        drevo_db.execute(
            _EVALUATE,
            {
                "questions": [_question(tracker)],
                "config": {"strategies": ["vector"], "label": "Ticket"},
            },
        )
