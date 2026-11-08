"""Unit tests for `CALL drevo.pagerank.personalized` from Python —
PageRank whose random walk jumps back to seed nodes — and the config map
`drevo.pagerank` accepts.

The graph is a chain of people `p0 -> p1 -> p2` plus an unrelated pair
`q0 -> q1`; names come from faker, structure is fixed.
"""

from __future__ import annotations

import pytest
from faker import Faker

import drevo

_PPR = (
    "MATCH (s:Person {title: $seed}) "
    "CALL drevo.pagerank.personalized(s, $config) YIELD node, score "
    "RETURN node.title AS title, score"
)


@pytest.fixture
def people(drevo_db: drevo.Drevo, fake: Faker) -> list[str]:
    """Five people: a chain p0 -> p1 -> p2 and a separate pair q0 -> q1."""
    names = [fake.unique.name() for _ in range(5)]
    drevo_db.execute(
        "CREATE (p0:Person {title: $p0}), (p1:Person {title: $p1}), "
        "(p2:Person {title: $p2}), (q0:Person {title: $q0}), "
        "(q1:Person {title: $q1}), "
        "(p0)-[:KNOWS]->(p1), (p1)-[:KNOWS]->(p2), (q0)-[:KNOWS]->(q1)",
        dict(zip(["p0", "p1", "p2", "q0", "q1"], names)),
    )
    return names


def test_scores_are_floats_best_first_and_sum_to_one(
    drevo_db: drevo.Drevo, people: list[str]
) -> None:
    rows = drevo_db.execute(_PPR, {"seed": people[0], "config": {}}).rows
    assert [r["title"] for r in rows][:3] == people[:3]
    assert all(isinstance(r["score"], float) for r in rows)
    assert sum(r["score"] for r in rows) == pytest.approx(1.0, abs=1e-6)


def test_nodes_the_seed_cannot_reach_are_not_returned(
    drevo_db: drevo.Drevo, people: list[str]
) -> None:
    rows = drevo_db.execute(_PPR, {"seed": people[0], "config": None}).rows
    assert {r["title"] for r in rows} == set(people[:3])
    assert all(r["score"] > 0.0 for r in rows)


def test_limit_and_direction(drevo_db: drevo.Drevo, people: list[str]) -> None:
    rows = drevo_db.execute(
        _PPR, {"seed": people[2], "config": {"direction": "incoming", "limit": 2}}
    ).rows
    assert [r["title"] for r in rows] == [people[2], people[1]]


def test_a_node_id_list_works_as_seeds(drevo_db: drevo.Drevo, people: list[str]) -> None:
    ids = drevo_db.execute(
        "MATCH (p:Person) WHERE p.title IN $names RETURN id(p) AS id",
        {"names": [people[0], people[3]]},
    ).rows
    rows = drevo_db.execute(
        "CALL drevo.pagerank.personalized($ids, {minScore: 0.01}) YIELD node, score "
        "RETURN node.title AS title, score",
        {"ids": [r["id"] for r in ids]},
    ).rows
    assert {people[0], people[3]} <= {r["title"] for r in rows}


def test_errors_raise_cypher_error(drevo_db: drevo.Drevo, people: list[str]) -> None:
    with pytest.raises(drevo.CypherError, match="at least one node"):
        drevo_db.execute("CALL drevo.pagerank.personalized([])")
    with pytest.raises(drevo.CypherError, match="unknown config key `top`"):
        drevo_db.execute(_PPR, {"seed": people[0], "config": {"top": 3}})


def test_global_pagerank_takes_a_config(drevo_db: drevo.Drevo, people: list[str]) -> None:
    plain = drevo_db.execute(
        "CALL drevo.pagerank() YIELD node, score RETURN node.title AS title, score"
    ).rows
    configured = drevo_db.execute(
        "CALL drevo.pagerank({}) YIELD node, score RETURN node.title AS title, score"
    ).rows
    assert configured == plain
    top = drevo_db.execute(
        "CALL drevo.pagerank({limit: 1}) YIELD node, score RETURN node.title AS title"
    ).rows
    assert [r["title"] for r in top] == [plain[0]["title"]]
