"""Unit tests for `Drevo.weighted_path` — the cheapest route by a
relationship cost, with an optional A* heuristic over node coordinates.

The fixture is a small street grid: car routes follow one-way `ROAD`s by
`car_s`, walking routes use `ROAD`s and footpath `PATH`s in either
direction by `walk_s`.
"""

from __future__ import annotations

import pytest
from faker import Faker

import drevo

# Junction coordinates (lat, lon), three junctions per row:
#   A  B  C
#   D  E  F
_GRID = {
    "A": (42.6980, 23.3200),
    "B": (42.6980, 23.3230),
    "C": (42.6980, 23.3260),
    "D": (42.6960, 23.3200),
    "E": (42.6960, 23.3230),
    "F": (42.6960, 23.3260),
}

_CAR = {"rel_types": ["ROAD"], "direction": drevo.Direction.OUT, "cost": "car_s"}
_WALK = {
    "rel_types": ["ROAD", "PATH"],
    "direction": drevo.Direction.BOTH,
    "cost": "walk_s",
}


@pytest.fixture
def streets(drevo_db: drevo.Drevo, fake: Faker) -> dict[str, drevo.Node]:
    """The grid above. Car streets: A→B→C one way, A↔D, B↔E, C↔F, D↔E,
    E↔F, each 60 s except B–E (90 s) and A–D (120 s); every street is 300 s
    on foot. A footpath A→E is 100 s on foot and closed to cars.
    """
    nodes = {
        name: drevo_db.create_node(
            drevo.NewNode(
                kind="Junction",
                title=fake.unique.street_name(),
                properties={"name": name, "lat": lat, "lon": lon},
            )
        )
        for name, (lat, lon) in _GRID.items()
    }

    def road(a: str, b: str, car_s: float, *, two_way: bool = True) -> None:
        pairs = [(a, b), (b, a)] if two_way else [(a, b)]
        for x, y in pairs:
            drevo_db.create_edge(
                drevo.NewEdge(
                    from_id=nodes[x].id,
                    to_id=nodes[y].id,
                    kind="ROAD",
                    properties={"car_s": car_s, "walk_s": 300.0},
                )
            )

    road("A", "B", 60.0, two_way=False)
    road("B", "C", 60.0, two_way=False)
    road("A", "D", 120.0)
    road("B", "E", 90.0)
    road("C", "F", 60.0)
    road("D", "E", 60.0)
    road("E", "F", 60.0)
    drevo_db.create_edge(
        drevo.NewEdge(
            from_id=nodes["A"].id,
            to_id=nodes["E"].id,
            kind="PATH",
            properties={"walk_s": 100.0},
        )
    )
    return nodes


def _names(streets: dict[str, drevo.Node], ids: list[int]) -> list[str]:
    by_id = {n.id: name for name, n in streets.items()}
    return [by_id[i] for i in ids]


def test_car_route_follows_one_way_streets(
    drevo_db: drevo.Drevo, streets: dict[str, drevo.Node]
) -> None:
    route = drevo_db.weighted_path(streets["A"].id, streets["F"].id, **_CAR)
    assert route is not None
    assert _names(streets, route.nodes) == ["A", "B", "C", "F"]
    assert route.cost == pytest.approx(180.0)
    assert len(route.edges) == len(route.nodes) - 1


def test_car_route_back_goes_around_the_one_way_block(
    drevo_db: drevo.Drevo, streets: dict[str, drevo.Node]
) -> None:
    route = drevo_db.weighted_path(streets["C"].id, streets["A"].id, **_CAR)
    assert route is not None
    assert _names(streets, route.nodes) == ["C", "F", "E", "D", "A"]
    assert route.cost == pytest.approx(300.0)


def test_walking_route_takes_the_footpath_in_any_direction(
    drevo_db: drevo.Drevo, streets: dict[str, drevo.Node]
) -> None:
    there = drevo_db.weighted_path(streets["A"].id, streets["F"].id, **_WALK)
    back = drevo_db.weighted_path(streets["F"].id, streets["A"].id, **_WALK)
    assert there is not None and back is not None
    assert _names(streets, there.nodes) == ["A", "E", "F"]
    assert _names(streets, back.nodes) == ["F", "E", "A"]
    assert there.cost == pytest.approx(400.0) == back.cost


def test_a_star_finds_the_same_cost_as_dijkstra(
    drevo_db: drevo.Drevo, streets: dict[str, drevo.Node]
) -> None:
    heuristic = drevo.GeoHeuristic(lat="lat", lon="lon", max_speed_mps=50.0)
    for a in streets.values():
        for b in streets.values():
            plain = drevo_db.weighted_path(a.id, b.id, **_CAR)
            guided = drevo_db.weighted_path(a.id, b.id, heuristic=heuristic, **_CAR)
            assert (plain is None) == (guided is None)
            if plain is not None and guided is not None:
                assert guided.cost == pytest.approx(plain.cost)


def test_max_cost_drops_a_route_that_is_too_expensive(
    drevo_db: drevo.Drevo, streets: dict[str, drevo.Node]
) -> None:
    a, f = streets["A"].id, streets["F"].id
    assert drevo_db.weighted_path(a, f, max_cost=400.0, **_WALK) is not None
    assert drevo_db.weighted_path(a, f, max_cost=399.0, **_WALK) is None


def test_default_cost_is_the_relationship_weight(
    drevo_db: drevo.Drevo, streets: dict[str, drevo.Node]
) -> None:
    """Without `cost`, each relationship costs its `weight` (1.0 here)."""
    route = drevo_db.weighted_path(streets["A"].id, streets["F"].id)
    assert route is not None
    assert route.cost == pytest.approx(2.0)


def test_unreachable_and_missing_nodes_give_none(
    drevo_db: drevo.Drevo, streets: dict[str, drevo.Node], fake: Faker
) -> None:
    island = drevo_db.create_node(drevo.NewNode(kind="Junction", title=fake.street_name()))
    assert drevo_db.weighted_path(streets["A"].id, island.id, **_CAR) is None
    assert drevo_db.weighted_path(streets["A"].id, 10**9, **_CAR) is None


def test_same_start_and_end_is_a_zero_cost_route(
    drevo_db: drevo.Drevo, streets: dict[str, drevo.Node]
) -> None:
    route = drevo_db.weighted_path(streets["B"].id, streets["B"].id, **_CAR)
    assert route is not None
    assert route.nodes == [streets["B"].id]
    assert route.edges == []
    assert route.cost == 0.0


def test_negative_cost_raises_value_error(
    drevo_db: drevo.Drevo, streets: dict[str, drevo.Node]
) -> None:
    drevo_db.create_edge(
        drevo.NewEdge(
            from_id=streets["C"].id,
            to_id=streets["A"].id,
            kind="ROAD",
            properties={"car_s": -5.0},
        )
    )
    with pytest.raises(ValueError, match="non-negative"):
        drevo_db.weighted_path(streets["C"].id, streets["A"].id, **_CAR)


@pytest.mark.parametrize(
    "kwargs",
    [
        {"max_cost": -1.0},
        {"max_cost": float("nan")},
        {"heuristic": drevo.GeoHeuristic(lat="lat", lon="lon", max_speed_mps=0.0)},
    ],
)
def test_bad_options_raise_value_error(
    drevo_db: drevo.Drevo, streets: dict[str, drevo.Node], kwargs: dict[str, object]
) -> None:
    with pytest.raises(ValueError):
        drevo_db.weighted_path(streets["A"].id, streets["F"].id, **kwargs)


def test_matches_the_cypher_procedure(
    drevo_db: drevo.Drevo, streets: dict[str, drevo.Node]
) -> None:
    result = drevo_db.execute(
        "MATCH (a), (b) WHERE id(a) = $a AND id(b) = $b "
        "CALL drevo.path.weighted(a, b, "
        "{relTypes: ['ROAD'], direction: 'OUTGOING', cost: 'car_s'}) "
        "YIELD cost RETURN cost",
        {"a": streets["C"].id, "b": streets["A"].id},
    )
    route = drevo_db.weighted_path(streets["C"].id, streets["A"].id, **_CAR)
    assert route is not None
    assert [row["cost"] for row in result.rows] == [pytest.approx(route.cost)]


def test_geo_heuristic_and_weighted_path_repr() -> None:
    heuristic = drevo.GeoHeuristic(lat="y", lon="x", max_speed_mps=13.9)
    assert (heuristic.lat, heuristic.lon, heuristic.max_speed_mps) == ("y", "x", 13.9)
    assert "GeoHeuristic" in repr(heuristic)
