"""Unit tests for context traces from Python: `drevo.rag.context` with a
`session` records a `:ContextTrace`, and `drevo.rag.traces`,
`drevo.rag.explain`, `drevo.rag.replay` and `drevo.rag.pruneTraces` read,
explain, re-run and prune it, through the Cypher API.

The fixture is a CBT journal whose entry title holds a made-up word only it
carries, so BM25 finds it first.
"""

from __future__ import annotations

import json

import pytest
from faker import Faker

import drevo

_CONTEXT = (
    "CALL drevo.rag.context($q, $config) "
    "YIELD text, tokens, nodeIds, traceId "
    "RETURN text, tokens, nodeIds, traceId"
)

_EXPLAIN = (
    "CALL drevo.rag.explain($id) "
    "YIELD node, status, rank, distance, seedScore, pprScore, tokens, reason "
    "RETURN id(node) AS node, status, rank, distance, seedScore, pprScore, tokens, reason"
)


@pytest.fixture
def journal(drevo_db: drevo.Drevo, fake: Faker) -> dict[str, str]:
    """`person -WROTE-> entry -CONTAINS-> thought -IS_A-> distortion`."""
    word: str = fake.unique.lexify("zq??????").lower()
    names = {
        "word": word,
        "session": fake.unique.uuid4(),
        "person": fake.unique.first_name(),
        "entry": f"{word} {fake.unique.day_of_week()}",
        "thought": fake.unique.sentence(nb_words=5),
        "distortion": fake.unique.word().capitalize(),
    }
    drevo_db.execute(
        "CREATE (:Person {title: $person})-[:WROTE]->"
        "(:Entry {title: $entry})-[:CONTAINS]->"
        "(:Thought {title: $thought})-[:IS_A]->(:Distortion {title: $distortion})",
        names,
    )
    return names


def _context(db: drevo.Drevo, query: str, config: dict | None = None) -> dict:
    rows = db.execute(_CONTEXT, {"q": query, "config": config}).rows
    assert len(rows) == 1
    return rows[0]


def _trace_count(db: drevo.Drevo) -> int:
    return db.execute("MATCH (t:ContextTrace) RETURN count(t) AS n").rows[0]["n"]


def test_a_session_records_a_trace(drevo_db: drevo.Drevo, journal: dict[str, str]) -> None:
    drevo_db.execute(
        "CALL drevo.memory.addMessage($s, 'user', $text) YIELD node RETURN node",
        {"s": journal["session"], "text": f"what about {journal['word']}?"},
    )
    row = _context(drevo_db, journal["word"], {"session": journal["session"]})
    assert isinstance(row["traceId"], str)
    assert _trace_count(drevo_db) == 1
    trace = drevo_db.execute(
        "MATCH (t:ContextTrace)-[:INITIATED_BY]->(m:Message) WHERE t.id = $id "
        "RETURN t.session AS session, t.query AS query, t.text AS text, "
        "t.tokens AS tokens, t.config AS config, m.text AS message",
        {"id": row["traceId"]},
    ).rows
    assert len(trace) == 1
    assert trace[0]["session"] == journal["session"]
    assert trace[0]["query"] == journal["word"]
    assert trace[0]["text"] == row["text"]
    assert trace[0]["tokens"] == row["tokens"]
    assert trace[0]["message"] == f"what about {journal['word']}?"
    config = json.loads(trace[0]["config"])
    assert "session" not in config
    assert "DREVO_" not in trace[0]["config"]


def test_without_a_session_nothing_is_recorded(
    drevo_db: drevo.Drevo, journal: dict[str, str]
) -> None:
    row = _context(drevo_db, journal["word"])
    assert row["traceId"] is None
    assert _trace_count(drevo_db) == 0


def test_explain_matches_the_context(drevo_db: drevo.Drevo, journal: dict[str, str]) -> None:
    row = _context(drevo_db, journal["word"], {"trace": True})
    rows = drevo_db.execute(_EXPLAIN, {"id": row["traceId"]}).rows
    assert [r["rank"] for r in rows] == list(range(1, len(rows) + 1))
    included = [r for r in rows if r["status"] == "included"]
    assert [r["node"] for r in included] == row["nodeIds"]
    assert sum(r["tokens"] for r in included) == row["tokens"]
    assert included[0]["distance"] == 0
    assert isinstance(included[0]["seedScore"], float)
    assert all(r["reason"] is None for r in included)


def test_traces_lists_the_newest_first(drevo_db: drevo.Drevo, journal: dict[str, str]) -> None:
    ids = [
        _context(drevo_db, journal["word"], {"session": journal["session"]})["traceId"]
        for _ in range(3)
    ]
    listed = drevo_db.execute(
        "CALL drevo.rag.traces($s, 2) YIELD trace RETURN trace.id AS id",
        {"s": journal["session"]},
    ).rows
    assert [r["id"] for r in listed] == ids[::-1][:2]


def test_replay_reports_whether_the_text_changed(
    drevo_db: drevo.Drevo, journal: dict[str, str], fake: Faker
) -> None:
    row = _context(drevo_db, journal["word"], {"trace": True, "hops": 1})
    replay = "CALL drevo.rag.replay($id) YIELD text, changed RETURN text, changed"
    same = drevo_db.execute(replay, {"id": row["traceId"]}).rows[0]
    assert same == {"text": row["text"], "changed": False}
    newcomer = fake.unique.first_name()
    drevo_db.execute(
        "MATCH (e:Entry {title: $entry}) CREATE (e)-[:MENTIONS]->(:Person {title: $name})",
        {"entry": journal["entry"], "name": newcomer},
    )
    after = drevo_db.execute(replay, {"id": row["traceId"]}).rows[0]
    assert after["changed"] is True
    assert newcomer in after["text"]
    assert _trace_count(drevo_db) == 1


def test_prune_deletes_older_traces(drevo_db: drevo.Drevo, journal: dict[str, str]) -> None:
    old = _context(drevo_db, journal["word"], {"trace": True})["traceId"]
    _context(drevo_db, journal["word"], {"trace": True})
    drevo_db.execute(
        "MATCH (t:ContextTrace) WHERE t.id = $id SET t.created_at = '2020-01-01T00:00:00.000Z'",
        {"id": old},
    )
    deleted = drevo_db.execute(
        "CALL drevo.rag.pruneTraces('2021-01-01') YIELD deleted RETURN deleted"
    ).rows[0]["deleted"]
    assert deleted == 1
    assert _trace_count(drevo_db) == 1


def test_errors_name_the_problem(drevo_db: drevo.Drevo, journal: dict[str, str]) -> None:
    with pytest.raises(drevo.CypherError, match="no context trace with id"):
        drevo_db.execute(_EXPLAIN, {"id": "missing"})
    with pytest.raises(drevo.CypherError, match="olderThan must be an ISO-8601"):
        drevo_db.execute("CALL drevo.rag.pruneTraces('last week')")
    with pytest.raises(drevo.CypherError, match="trace must be a boolean"):
        _context(drevo_db, journal["word"], {"trace": "yes"})
