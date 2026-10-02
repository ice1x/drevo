"""Unit tests for `Drevo.execute` — Cypher from Python (issue #553).

One statement per call, autocommit: a write is committed when `execute`
returns. Results are a `CypherResult` with `.columns`, rows as dicts (by
iteration, index or `.rows`) and the write `.stats`. Graph values come back
as `CypherNode` / `CypherRelationship` / `CypherPath`, whose `uuid` is a
`uuid.UUID` like `Node.uuid`. Parameters are named (`$name`) and accept
None, bool, int, float, str, `uuid.UUID`, lists and str-keyed dicts.
"""

from __future__ import annotations

import math
import uuid
from typing import Any

import pytest
from faker import Faker
from hypothesis import given, settings
from hypothesis import strategies as st

import drevo

# ── Basic shape ──────────────────────────────────────────────────────


def test_returns_columns_and_rows_as_dicts(drevo_db: drevo.Drevo) -> None:
    result = drevo_db.execute("RETURN 1 AS one, 'two' AS two")
    assert result.columns == ["one", "two"]
    assert len(result) == 1
    assert list(result) == [{"one": 1, "two": "two"}]
    assert result[0]["two"] == "two"
    assert result.rows == [{"one": 1, "two": "two"}]


def test_a_write_is_committed_and_counted(drevo_db: drevo.Drevo, fake: Faker) -> None:
    name = fake.name()
    created = drevo_db.execute("CREATE (:Person {name: $name})", {"name": name})
    assert created.columns == []
    assert list(created) == []
    assert created.stats["nodes_created"] == 1

    found = drevo_db.execute("MATCH (p:Person) RETURN p.name AS name")
    assert [row["name"] for row in found] == [name]


def test_params_are_optional_and_named(drevo_db: drevo.Drevo) -> None:
    assert drevo_db.execute("RETURN 5 AS v").rows == [{"v": 5}]
    assert drevo_db.execute("RETURN $x + $y AS v", {"x": 2, "y": 3}).rows == [{"v": 5}]


def test_empty_result_keeps_its_columns(drevo_db: drevo.Drevo) -> None:
    result = drevo_db.execute("MATCH (n:Nothing) RETURN n.x AS x")
    assert result.columns == ["x"]
    assert len(result) == 0


# ── Graph values ────────────────────────────────────────────────────


def test_nodes_relationships_and_paths(drevo_db: drevo.Drevo, fake: Faker) -> None:
    a, b = fake.first_name(), fake.first_name()
    drevo_db.execute(
        "CREATE (:Person {name: $a})-[:KNOWS {since: 2020}]->(:Person {name: $b})",
        {"a": a, "b": b},
    )
    row = drevo_db.execute("MATCH p = (x:Person)-[r:KNOWS]->(y:Person) RETURN x, r, y, p")[0]

    x, r, y, p = row["x"], row["r"], row["y"], row["p"]
    assert isinstance(x, drevo.CypherNode)
    assert x.labels == ["Person"]
    assert x.properties == {"name": a}
    assert isinstance(x.id, int)
    assert isinstance(x.uuid, uuid.UUID)

    assert isinstance(r, drevo.CypherRelationship)
    assert r.type == "KNOWS"
    assert r.properties == {"since": 2020}
    assert (r.start_id, r.end_id) == (x.id, y.id)
    assert isinstance(r.uuid, uuid.UUID)

    assert isinstance(p, drevo.CypherPath)
    assert [n.id for n in p.nodes] == [x.id, y.id]
    assert [rel.id for rel in p.relationships] == [r.id]
    assert len(p) == 1


def test_cypher_node_matches_the_crud_api(drevo_db: drevo.Drevo) -> None:
    node = drevo_db.create_node(drevo.NewNode(kind="note", title="hello"))
    row = drevo_db.execute("MATCH (n:note) RETURN n")[0]
    assert row["n"].id == node.id
    assert row["n"].uuid == node.uuid
    assert row["n"].properties["title"] == "hello"


def test_untitled_nodes_have_no_placeholder_title(drevo_db: drevo.Drevo) -> None:
    drevo_db.execute("CREATE (:Probe {x: 1})")
    node = drevo_db.execute("MATCH (n:Probe) RETURN n")[0]["n"]
    assert node.properties == {"x": 1}


# ── Parameters round-trip ───────────────────────────────────────────


def test_uuid_parameters_become_strings(drevo_db: drevo.Drevo) -> None:
    value = uuid.uuid4()
    assert drevo_db.execute("RETURN $u AS u", {"u": value}).rows == [{"u": str(value)}]


@pytest.mark.parametrize(
    "value",
    [None, True, False, 0, -7, 2**62, 1.5, -0.25, "", "ünïcødé", [], [1, "a", None], {}],
)
def test_scalar_and_container_parameters_round_trip(drevo_db: drevo.Drevo, value: Any) -> None:
    assert drevo_db.execute("RETURN $v AS v", {"v": value})[0]["v"] == value


def test_nested_map_parameter_round_trips(drevo_db: drevo.Drevo) -> None:
    value = {"a": [1, {"b": None}], "c": {"d": "e"}}
    assert drevo_db.execute("RETURN $v AS v", {"v": value})[0]["v"] == value


def test_nan_parameter_comes_back_nan(drevo_db: drevo.Drevo) -> None:
    assert math.isnan(drevo_db.execute("RETURN $v AS v", {"v": float("nan")})[0]["v"])


_json_like = st.recursive(
    st.none()
    | st.booleans()
    | st.integers(min_value=-(2**63), max_value=2**63 - 1)
    | st.floats(allow_nan=False)
    | st.text(),
    lambda children: st.lists(children, max_size=4)
    | st.dictionaries(st.text(max_size=8), children, max_size=4),
    max_leaves=12,
)


@settings(max_examples=60, deadline=None)
@given(value=_json_like)
def test_any_json_like_parameter_round_trips(value: Any) -> None:
    with drevo.Drevo.open_in_memory() as db:
        assert db.execute("RETURN $v AS v", {"v": value})[0]["v"] == value


@pytest.mark.parametrize(
    "bad",
    [object(), b"bytes", {1: "non-str key"}, 2**64, {1.5}],
    ids=["object", "bytes", "int-key", "too-big", "set"],
)
def test_unsupported_parameter_types_are_rejected(drevo_db: drevo.Drevo, bad: Any) -> None:
    with pytest.raises((TypeError, ValueError, OverflowError)):
        drevo_db.execute("RETURN $v AS v", {"v": bad})


# ── Errors ──────────────────────────────────────────────────────────


def test_syntax_error(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(drevo.CypherSyntaxError) as info:
        drevo_db.execute("MATCH (n RETURN n")
    assert isinstance(info.value, drevo.CypherError)
    assert isinstance(info.value, drevo.DrevoError)


def test_missing_parameter(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(drevo.ParameterMissingError) as info:
        drevo_db.execute("RETURN $absent AS v")
    assert "absent" in str(info.value)
    assert isinstance(info.value, drevo.CypherError)


def test_semantic_error_is_a_cypher_error(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(drevo.CypherError):
        drevo_db.execute("RETURN undefined_variable AS v")


def test_storage_errors_keep_their_existing_types(drevo_db: drevo.Drevo) -> None:
    drevo_db.execute("CREATE (:note {title: 'dup'})")
    with pytest.raises(drevo.DuplicateTitleError):
        drevo_db.execute("CREATE (:note {title: 'dup'})")


def test_params_must_be_a_dict(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(TypeError):
        drevo_db.execute("RETURN 1 AS v", [1, 2])  # type: ignore[arg-type]


def test_closed_handle_raises(drevo_db: drevo.Drevo) -> None:
    db = drevo.Drevo.open_in_memory()
    db.close()
    with pytest.raises(RuntimeError):
        db.execute("RETURN 1 AS v")
