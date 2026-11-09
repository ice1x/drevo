"""Unit tests for entity resolution from Python: `drevo.entity.candidates`,
`drevo.entity.merge`, `drevo.entity.aliases`, the `resolve` option of
`drevo.memory.rememberEntity` and `drevo.entity.normalise`, through the
Cypher API.

The fixture is a CBT journal's people: a therapist written down once in full
and once with initials, and a client named by faker.
"""

from __future__ import annotations

from typing import Any

import pytest
from faker import Faker
from hypothesis import given, settings
from hypothesis import strategies as st

import drevo

_CANDIDATES = (
    "CALL drevo.entity.candidates('Entity', $config) "
    "YIELD a, b, nameScore, vectorScore, score, reasons "
    "RETURN a.name AS a, b.name AS b, nameScore, vectorScore, score, reasons"
)

_MERGE = (
    "MATCH (k:Entity {name: $keep}) "
    "MATCH (d:Entity) WHERE d.name IN $drops "
    "WITH k, collect(d) AS ds "
    "CALL drevo.entity.merge(k, ds, $config) "
    "YIELD kept, merged, relationshipsMoved, propertiesMerged "
    "RETURN kept.name AS kept, kept.aliases AS aliases, merged, relationshipsMoved, "
    "propertiesMerged"
)


def _remember(db: drevo.Drevo, name: str, kind: str = "PERSON", options: Any = None) -> Any:
    [row] = db.execute(
        "CALL drevo.memory.rememberEntity(null, $name, $kind, null, $options) YIELD node "
        "RETURN id(node) AS id, node.name AS name",
        {"name": name, "kind": kind, "options": options},
    ).rows
    return row


@pytest.fixture
def journal(drevo_db: drevo.Drevo, fake: Faker) -> dict[str, str]:
    """«Анна Петровна Смирнова», «А. П. Смирнова» and a faker client."""
    names = {
        "full": "Анна Петровна Смирнова",
        "initials": "А. П. Смирнова",
        "client": fake.unique.name(),
    }
    for name in names.values():
        _remember(drevo_db, name)
    return names


def test_candidates_pair_the_two_spellings(drevo_db: drevo.Drevo, journal: dict[str, str]) -> None:
    rows = drevo_db.execute(_CANDIDATES, {"config": {"blocking": "token"}}).rows
    pair = [r for r in rows if {r["a"], r["b"]} == {journal["full"], journal["initials"]}]
    assert len(pair) == 1, rows
    assert isinstance(pair[0]["nameScore"], float)
    assert pair[0]["vectorScore"] is None
    assert all(isinstance(reason, str) for reason in pair[0]["reasons"])
    assert "shared_token:смирнова" in pair[0]["reasons"]


def test_merge_records_the_dropped_name_as_an_alias(
    drevo_db: drevo.Drevo, journal: dict[str, str]
) -> None:
    [row] = drevo_db.execute(
        _MERGE, {"keep": journal["full"], "drops": [journal["initials"]], "config": None}
    ).rows
    assert row["kept"] == journal["full"]
    assert row["aliases"] == [journal["initials"]]
    assert row["merged"] == 1
    [found] = drevo_db.execute(
        "CALL drevo.entity.aliases($name) YIELD entity, aliases "
        "RETURN entity.name AS name, aliases",
        {"name": "а.п. смирнова"},
    ).rows
    assert found == {"name": journal["full"], "aliases": [journal["initials"]]}


def test_merge_of_different_types_is_refused(drevo_db: drevo.Drevo, fake: Faker) -> None:
    city = fake.unique.city()
    keep = _remember(drevo_db, city, "PERSON")["id"]
    place = _remember(drevo_db, city, "LOCATION")["id"]
    with pytest.raises(drevo.CypherError, match="cannot merge nodes of different types"):
        drevo_db.execute(
            "CALL drevo.entity.merge($keep, [$place]) YIELD merged RETURN merged",
            {"keep": keep, "place": place},
        )


def test_remember_with_resolve_alias_reuses_the_kept_node(
    drevo_db: drevo.Drevo, journal: dict[str, str]
) -> None:
    drevo_db.execute(
        _MERGE, {"keep": journal["full"], "drops": [journal["initials"]], "config": None}
    )
    kept = _remember(drevo_db, journal["full"])["id"]
    again = _remember(drevo_db, "А.П. Смирнова", options={"resolve": "alias"})
    assert again["id"] == kept


@settings(max_examples=60, deadline=None)
@given(name=st.text(max_size=40))
def test_normalise_is_idempotent(name: str) -> None:
    with drevo.Drevo.open_in_memory() as db:
        [row] = db.execute(
            "WITH drevo.entity.normalise($name) AS once "
            "RETURN once, drevo.entity.normalise(once) AS twice",
            {"name": name},
        ).rows
    assert row["once"] == row["twice"]


@settings(max_examples=25, deadline=None)
@given(
    links=st.lists(
        st.tuples(st.sampled_from(["keep", "drop"]), st.sampled_from(["KNOWS", "TREATS"])),
        max_size=8,
    )
)
def test_merge_keeps_every_relationship_but_duplicates(links: list[tuple[str, str]]) -> None:
    """Relationships to one neighbour: after the merge the kept node has one
    per distinct type, and `relationshipsMoved` counts the moved ones."""
    with drevo.Drevo.open_in_memory() as db:
        for name in ["Анна Смирнова", "А. Смирнова", "Клиент"]:
            _remember(db, name)
        for owner, kind in links:
            name = "Анна Смирнова" if owner == "keep" else "А. Смирнова"
            db.execute(
                f"MATCH (o:Entity {{name: $name}}), (c:Entity {{name: 'Клиент'}}) "
                f"CREATE (o)-[:{kind}]->(c)",
                {"name": name},
            )
        [row] = db.execute(
            _MERGE, {"keep": "Анна Смирнова", "drops": ["А. Смирнова"], "config": None}
        ).rows
        [after] = db.execute(
            "MATCH (:Entity {name: 'Анна Смирнова'})-[r]->(:Entity {name: 'Клиент'}) "
            "RETURN count(r) AS n"
        ).rows
    keep_kinds = {kind for owner, kind in links if owner == "keep"}
    drop_kinds = {kind for owner, kind in links if owner == "drop"}
    keep_count = sum(1 for owner, _ in links if owner == "keep")
    assert after["n"] == keep_count + len(drop_kinds - keep_kinds)
    assert row["relationshipsMoved"] == len(drop_kinds - keep_kinds)
