"""Unit tests for `CALL drevo.path.reachable` from Python — every node within
a cost budget of one start node.

The fixture is a short street, Home → Corner → Square, with a slow side street
Home → Lane and a bus stop at the Square.
"""

from __future__ import annotations

import pytest
from faker import Faker

import drevo

_REACH = (
    "MATCH (a:Crossing {name: $from}) "
    "CALL drevo.path.reachable(a, {relTypes: ['STREET'], cost: 'walk_s', maxCost: $max}) "
    "YIELD node, cost RETURN node.name AS name, cost"
)


@pytest.fixture
def street(drevo_db: drevo.Drevo, fake: Faker) -> dict[str, str]:
    """Crossing names keyed by role."""
    names = {role: fake.unique.street_name() for role in ("home", "corner", "square", "lane")}
    drevo_db.execute(
        "CREATE (h:Crossing {name: $home}), (c:Crossing {name: $corner}), "
        "(s:Crossing:Stop {name: $square}), (l:Crossing {name: $lane}), "
        "(h)-[:STREET {walk_s: 120}]->(c), (c)-[:STREET {walk_s: 180}]->(s), "
        "(h)-[:STREET {walk_s: 600}]->(l)",
        names,
    )
    return names


def test_every_crossing_within_the_budget(drevo_db: drevo.Drevo, street: dict[str, str]) -> None:
    rows = drevo_db.execute(_REACH, {"from": street["home"], "max": 300}).rows
    assert [(r["name"], r["cost"]) for r in rows] == [
        (street["home"], 0.0),
        (street["corner"], 120.0),
        (street["square"], 300.0),
    ]


def test_targets_and_path(drevo_db: drevo.Drevo, street: dict[str, str]) -> None:
    rows = drevo_db.execute(
        "MATCH (a:Crossing {name: $from}) "
        "CALL drevo.path.reachable(a, {cost: 'walk_s', maxCost: 900, "
        "targets: {label: 'Stop'}, path: true}) YIELD node, cost, path "
        "RETURN node, cost, path",
        {"from": street["home"]},
    ).rows
    assert len(rows) == 1
    row = rows[0]
    assert isinstance(row["node"], drevo.CypherNode)
    assert row["node"].properties["name"] == street["square"]
    assert row["cost"] == 300.0
    assert [n.properties["name"] for n in row["path"].nodes] == [
        street["home"],
        street["corner"],
        street["square"],
    ]


def test_a_missing_budget_raises_a_cypher_error(
    drevo_db: drevo.Drevo, street: dict[str, str]
) -> None:
    with pytest.raises(drevo.CypherError, match="maxCost"):
        drevo_db.execute(
            "MATCH (a:Crossing {name: $from}) CALL drevo.path.reachable(a, {}) YIELD node RETURN node",
            {"from": street["home"]},
        )
