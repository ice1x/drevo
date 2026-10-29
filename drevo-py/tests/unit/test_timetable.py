"""Unit tests for `CALL drevo.path.timetable` from Python — public-transport
journeys over a timetable stored on hop relationships between stops.

The fixture is one bus line, Central → Market → Park (trips 11 and 12), and
a footpath from Park to the Zoo stop.
"""

from __future__ import annotations

import pytest
from faker import Faker

import drevo

_PLAN = (
    "MATCH (a:Stop {name: $from}), (d:Stop {name: $to}) "
    "CALL drevo.path.timetable([{node: a, cost: $access}], [{node: d, cost: 60}], "
    "{departAt: $at, days: [{offset: 0, services: [1]}], "
    "walk: {relTypes: ['WALK'], cost: 's'}, minTransfer: 60}) "
    "YIELD arriveAt, departAt, transfers, legs "
    "RETURN arriveAt, departAt, transfers, legs"
)


@pytest.fixture
def stops(drevo_db: drevo.Drevo, fake: Faker) -> dict[str, str]:
    """Stop names keyed by role, with trips 11 (08:00) and 12 (08:10)."""
    names = {role: fake.unique.street_name() for role in ("central", "market", "park", "zoo")}
    drevo_db.execute(
        "CREATE (a:Stop {name: $central}), (b:Stop {name: $market}), "
        "(c:Stop {name: $park}), (d:Stop {name: $zoo}), "
        "(a)-[:HOP {dep: [28800, 29400], arr: [29100, 29700], trip: [11, 12], svc: [1, 1]}]->(b), "
        "(b)-[:HOP {dep: [29160, 29760], arr: [29400, 30000], trip: [11, 12], svc: [1, 1]}]->(c), "
        "(c)-[:WALK {s: 180}]->(d)",
        names,
    )
    return names


def test_a_ride_then_a_walk(drevo_db: drevo.Drevo, stops: dict[str, str]) -> None:
    rows = drevo_db.execute(
        _PLAN, {"from": stops["central"], "to": stops["zoo"], "access": 240, "at": 28500}
    ).rows
    assert len(rows) == 1
    row = rows[0]
    assert (row["arriveAt"], row["departAt"], row["transfers"]) == (29640, 28560, 0)
    legs = row["legs"]
    assert [leg["mode"] for leg in legs] == ["access", "ride", "walk", "egress"]
    access, ride, walk, egress = legs
    assert access["from"] is None and egress["to"] is None
    assert isinstance(ride["from"], drevo.CypherNode)
    assert ride["from"].properties["name"] == stops["central"]
    assert ride["to"].properties["name"] == stops["park"]
    assert (ride["trip"], ride["depart"], ride["arrive"]) == (11, 28800, 29400)
    assert [h.type for h in ride["hops"]] == ["HOP", "HOP"]
    assert all(isinstance(h, drevo.CypherRelationship) for h in ride["hops"])
    assert walk["trip"] is None
    assert [h.type for h in walk["hops"]] == ["WALK"]
    assert (walk["depart"], walk["arrive"]) == (29400, 29580)


def test_missing_the_first_trip_takes_the_next(
    drevo_db: drevo.Drevo, stops: dict[str, str]
) -> None:
    rows = drevo_db.execute(
        _PLAN, {"from": stops["central"], "to": stops["park"], "access": 240, "at": 28700}
    ).rows
    assert [(r["arriveAt"], r["legs"][1]["trip"]) for r in rows] == [(30060, 12)]


def test_no_service_back_gives_no_rows(drevo_db: drevo.Drevo, stops: dict[str, str]) -> None:
    rows = drevo_db.execute(
        _PLAN, {"from": stops["park"], "to": stops["central"], "access": 0, "at": 28000}
    ).rows
    assert rows == []


def test_bad_options_raise_a_cypher_error(drevo_db: drevo.Drevo, stops: dict[str, str]) -> None:
    with pytest.raises(drevo.CypherError, match="departAt"):
        drevo_db.execute(
            "MATCH (a:Stop {name: $n}) "
            "CALL drevo.path.timetable([{node: a, cost: 0}], [], {days: []}) "
            "YIELD arriveAt RETURN arriveAt",
            {"n": stops["central"]},
        )
