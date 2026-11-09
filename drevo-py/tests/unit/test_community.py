"""Unit tests for community summaries from Python: `drevo.community.summarize`,
`drevo.community.query`, `drevo.community.of`, `drevo.community.stale` and the
`scope` of `drevo.rag.context`, through the Cypher API.

The fixture is an IT task manager with two teams of faker-named people, each
working on its own tickets, and one ticket of one team depending on a ticket
of the other.
"""

from __future__ import annotations

from typing import Any

import pytest
from faker import Faker

import drevo

_SUMMARIZE = (
    "CALL drevo.community.summarize($config) "
    "YIELD community, size, summary, keywords, mode "
    "RETURN community.id AS id, size, summary, keywords, mode"
)


@pytest.fixture
def teams(drevo_db: drevo.Drevo, fake: Faker) -> dict[str, Any]:
    """Two teams of three people and three tickets each, one bridge."""
    names: dict[str, Any] = {}
    for team, topic in (("backend", "database"), ("mobile", "release")):
        people = [fake.unique.first_name() for _ in range(3)]
        tickets = [f"{team.upper()}-{fake.unique.random_int(100, 999)} {topic}" for _ in range(3)]
        drevo_db.execute(
            "CREATE (a:Person {name: $p0}), (b:Person {name: $p1}), (c:Person {name: $p2}), "
            "(t0:Ticket {title: $t0}), (t1:Ticket {title: $t1}), (t2:Ticket {title: $t2}), "
            "(a)-[:WORKS_ON]->(t0), (a)-[:WORKS_ON]->(t1), (a)-[:WORKS_ON]->(t2), "
            "(b)-[:WORKS_ON]->(t0), (b)-[:WORKS_ON]->(t1), (c)-[:WORKS_ON]->(t2), "
            "(c)-[:WORKS_ON]->(t1), (a)-[:MENTORS]->(b), (a)-[:MENTORS]->(c), "
            "(t0)-[:BLOCKS]->(t2)",
            {
                "p0": people[0],
                "p1": people[1],
                "p2": people[2],
                "t0": tickets[0],
                "t1": tickets[1],
                "t2": tickets[2],
            },
        )
        names[team] = {"people": people, "tickets": tickets}
    drevo_db.execute(
        "MATCH (m:Ticket {title: $mobile}), (b:Ticket {title: $backend}) "
        "CREATE (m)-[:DEPENDS_ON]->(b)",
        {"mobile": names["mobile"]["tickets"][0], "backend": names["backend"]["tickets"][1]},
    )
    return names


def _summarize(db: drevo.Drevo, config: dict[str, Any] | None = None) -> list[dict[str, Any]]:
    return db.execute(_SUMMARIZE, {"config": config}).rows


def test_summarize_finds_the_two_teams(drevo_db: drevo.Drevo, teams: dict[str, Any]) -> None:
    rows = _summarize(drevo_db, {"mode": "extractive"})
    assert [(r["id"], r["size"]) for r in rows] == [("default-0", 6), ("default-1", 6)]
    for row in rows:
        assert row["mode"] == "extractive"
        assert row["summary"].startswith("6 members. Person: ")
        assert isinstance(row["keywords"], list) and 0 < len(row["keywords"]) <= 10
    members = drevo_db.execute(
        "MATCH (p:Person)-[:MEMBER_OF]->(c:Community {id: 'default-0'}) RETURN p.name AS name"
    ).rows
    assert sorted(r["name"] for r in members) == sorted(teams["backend"]["people"])
    [bridge] = drevo_db.execute(
        "MATCH (:Community)-[r:RELATED_COMMUNITY]->(:Community) RETURN r.weight AS w"
    ).rows
    assert bridge["w"] == 1


def test_summaries_are_deterministic_and_within_budget(
    drevo_db: drevo.Drevo, teams: dict[str, Any]
) -> None:
    first = _summarize(drevo_db, {"mode": "extractive", "tokenBudget": 40})
    again = _summarize(drevo_db, {"mode": "extractive", "tokenBudget": 40})
    assert [r["summary"] for r in first] == [r["summary"] for r in again]
    for row in first:
        assert -(-len(row["summary"]) // 4) <= 40


def test_query_and_of(drevo_db: drevo.Drevo, teams: dict[str, Any]) -> None:
    _summarize(drevo_db, {"mode": "extractive"})
    [best] = drevo_db.execute(
        "CALL drevo.community.query('release', 1) YIELD community, score, summary "
        "RETURN community.id AS id, score, summary"
    ).rows
    assert best["id"] == "default-1"
    assert best["score"] > 0
    rows = drevo_db.execute(
        "MATCH (p:Person {name: $name}) CALL drevo.community.of(p) YIELD community, level "
        "RETURN community.id AS id, level",
        {"name": teams["mobile"]["people"][0]},
    ).rows
    assert rows == [{"id": "default-1", "level": "default"}]


def test_stale_after_a_new_ticket(
    drevo_db: drevo.Drevo, teams: dict[str, Any], fake: Faker
) -> None:
    _summarize(drevo_db, {"mode": "extractive"})
    drevo_db.execute(
        "MATCH (p:Person {name: $name}) CREATE (p)-[:WORKS_ON]->(:Ticket {title: $title})",
        {"name": teams["backend"]["people"][0], "title": fake.unique.sentence(nb_words=4)},
    )
    rows = drevo_db.execute(
        "CALL drevo.community.stale() YIELD community, size, changed "
        "RETURN community.id AS id, size, changed ORDER BY id"
    ).rows
    assert rows == [
        {"id": "default-0", "size": 7, "changed": True},
        {"id": "default-1", "size": 6, "changed": False},
    ]


def test_global_scope_of_a_context(drevo_db: drevo.Drevo, teams: dict[str, Any]) -> None:
    _summarize(drevo_db, {"mode": "extractive"})
    [row] = drevo_db.execute(
        "CALL drevo.rag.context($q, {scope: 'global', format: 'triples'}) "
        "YIELD text, scope RETURN text, scope",
        {"q": "What is this project about?"},
    ).rows
    assert row["scope"] == "global"
    assert row["text"].startswith("community default-")


def test_errors_name_the_problem(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(drevo.CypherError, match="mode must be 'auto', 'llm' or 'extractive'"):
        _summarize(drevo_db, {"mode": "poetic"})
    with pytest.raises(drevo.CypherError, match=r"drevo\.extract\.status\(\)"):
        _summarize(drevo_db, {"mode": "llm"})


def test_async_stores_a_community_job(drevo_db: drevo.Drevo, teams: dict[str, Any]) -> None:
    [row] = drevo_db.execute(
        "CALL drevo.community.summarize($config) "
        "YIELD community, size, summary, keywords, mode, jobId "
        "RETURN community, size, summary, keywords, mode, jobId",
        {"config": {"async": True, "mode": "extractive"}},
    ).rows
    assert isinstance(row["jobId"], str) and row["jobId"]
    for column in ("community", "size", "summary", "keywords", "mode"):
        assert row[column] is None, column
    # No scheduler runs on this handle: the job waits, queued, and nothing
    # is written yet.
    [job] = drevo_db.execute(
        "MATCH (j:IngestJob {id: $id}) RETURN j.pipeline AS pipeline, j.state AS state",
        {"id": row["jobId"]},
    ).rows
    assert job == {"pipeline": "community", "state": "queued"}
    assert drevo_db.execute("MATCH (c:Community) RETURN count(c) AS n").rows == [{"n": 0}]
