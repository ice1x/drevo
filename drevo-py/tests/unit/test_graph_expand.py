"""Unit tests for `CALL drevo.graph.expand` from Python — a breadth-first walk
from seed nodes inside the database, each node returned once at its shortest
distance with the relationship that reached it.

The fixture is a small story outline: a chapter that features a hero, who
knows a mentor, who lives in a town.
"""

from __future__ import annotations

import pytest
from faker import Faker

import drevo

_EXPAND = (
    "MATCH (c:Chapter {title: $chapter}) "
    "CALL drevo.graph.expand(c, $config) "
    "YIELD node, distance, via, truncated "
    "RETURN node.title AS title, distance, via, truncated"
)


@pytest.fixture
def story(drevo_db: drevo.Drevo, fake: Faker) -> dict[str, str]:
    """`chapter -FEATURES-> hero -KNOWS-> mentor -LIVES_IN-> town`."""
    names = {
        "chapter": f"Chapter {fake.unique.word()}",
        "hero": fake.unique.first_name(),
        "mentor": fake.unique.first_name(),
        "town": fake.unique.city(),
    }
    drevo_db.execute(
        "CREATE (:Chapter {title: $chapter})-[:FEATURES]->"
        "(:Character {title: $hero})-[:KNOWS]->"
        "(:Character {title: $mentor})-[:LIVES_IN]->(:Place {title: $town})",
        names,
    )
    return names


def test_two_hops_return_each_node_once_with_its_distance(
    drevo_db: drevo.Drevo, story: dict[str, str]
) -> None:
    rows = drevo_db.execute(_EXPAND, {"chapter": story["chapter"], "config": {"hops": 2}}).rows
    assert [(r["title"], r["distance"]) for r in rows] == [
        (story["chapter"], 0),
        (story["hero"], 1),
        (story["mentor"], 2),
    ]
    assert rows[0]["via"] is None
    assert all(isinstance(r["distance"], int) for r in rows)
    assert all(r["truncated"] is False for r in rows)


def test_via_is_a_dict_naming_the_relationship(
    drevo_db: drevo.Drevo, story: dict[str, str]
) -> None:
    rows = drevo_db.execute(_EXPAND, {"chapter": story["chapter"], "config": None}).rows
    via = rows[1]["via"]
    assert via["relType"] == "FEATURES"
    assert via["direction"] == "out"
    assert isinstance(via["fromId"], int)
    assert isinstance(via["relId"], int)


def test_labels_and_max_nodes(drevo_db: drevo.Drevo, story: dict[str, str]) -> None:
    rows = drevo_db.execute(
        _EXPAND,
        {"chapter": story["chapter"], "config": {"hops": 3, "labels": ["Character"]}},
    ).rows
    assert [r["title"] for r in rows] == [story["chapter"], story["hero"], story["mentor"]]

    rows = drevo_db.execute(
        _EXPAND, {"chapter": story["chapter"], "config": {"hops": 3, "maxNodes": 2}}
    ).rows
    assert len(rows) == 2
    assert all(r["truncated"] is True for r in rows)


def test_an_unknown_config_key_raises(drevo_db: drevo.Drevo, story: dict[str, str]) -> None:
    with pytest.raises(drevo.CypherError, match="depth"):
        drevo_db.execute(_EXPAND, {"chapter": story["chapter"], "config": {"depth": 2}})


def test_hops_above_the_server_limit_raise(drevo_db: drevo.Drevo, story: dict[str, str]) -> None:
    with pytest.raises(drevo.CypherError, match="DREVO_EXPAND_MAX_HOPS"):
        drevo_db.execute(_EXPAND, {"chapter": story["chapter"], "config": {"hops": 7}})
