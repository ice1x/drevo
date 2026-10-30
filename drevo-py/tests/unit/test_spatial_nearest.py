"""Unit tests for `CALL drevo.spatial.nearest` from Python — the nodes nearest
to a latitude/longitude, from a point index.

The fixture is three tram stops in Sofia and a depot that is not a stop.
"""

from __future__ import annotations

import pytest
from faker import Faker

import drevo

_NEAREST = (
    "CALL drevo.spatial.nearest('Stop', {lat: 42.6977, lon: 23.3219}, $options) "
    "YIELD node, meters RETURN node.name AS name, meters"
)


@pytest.fixture
def stops(drevo_db: drevo.Drevo, fake: Faker) -> dict[str, str]:
    """Stop names keyed by role: Opera ~750 m from the centre, Lions Bridge
    ~1.1 km, the airport ~7 km; the depot is next to the centre."""
    names = {role: fake.unique.street_name() for role in ("opera", "lions", "airport", "depot")}
    drevo_db.execute("CREATE POINT INDEX stops FOR (s:Stop) ON (s.lat, s.lon)")
    drevo_db.execute(
        "CREATE (:Stop {name: $opera, tram: true, lat: 42.6935, lon: 23.3290}), "
        "(:Stop {name: $lions, tram: false, lat: 42.7075, lon: 23.3216}), "
        "(:Stop {name: $airport, tram: false, lat: 42.6952, lon: 23.4062}), "
        "(:Depot {name: $depot, lat: 42.6978, lon: 23.3220})",
        names,
    )
    return names


def test_the_nearest_stops_first(drevo_db: drevo.Drevo, stops: dict[str, str]) -> None:
    rows = drevo_db.execute(_NEAREST, {"options": {"k": 2}}).rows
    assert [r["name"] for r in rows] == [stops["opera"], stops["lions"]]
    assert 700 < rows[0]["meters"] < 800
    assert rows[0]["meters"] < rows[1]["meters"]


def test_where_and_max_meters(drevo_db: drevo.Drevo, stops: dict[str, str]) -> None:
    rows = drevo_db.execute(_NEAREST, {"options": {"k": 5, "where": {"tram": False}}}).rows
    assert [r["name"] for r in rows] == [stops["lions"], stops["airport"]]
    assert drevo_db.execute(_NEAREST, {"options": {"maxMeters": 500}}).rows == []


def test_a_point_off_the_globe_raises_a_cypher_error(
    drevo_db: drevo.Drevo, stops: dict[str, str]
) -> None:
    with pytest.raises(drevo.CypherError, match="latitude"):
        drevo_db.execute(
            "CALL drevo.spatial.nearest('Stop', {lat: 95.0, lon: 23.0}) YIELD node RETURN node"
        )
