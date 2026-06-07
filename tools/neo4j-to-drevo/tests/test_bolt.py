"""Unit tests for the `Neo4jSource` live-Bolt adapter.

The adapter is the only part that touches the real `neo4j` driver. These
tests **mock the driver entirely** — they assert the adapter issues the
expected node/relationship Cypher, maps driver records to the
source-agnostic records the engine consumes, and closes the driver. No
Neo4j server, no `neo4j` package required (the driver is a `MagicMock`).
"""

from __future__ import annotations

from typing import Any
from unittest.mock import MagicMock

from neo4j_to_drevo import SourceNode, SourceRelationship
from neo4j_to_drevo.bolt import Neo4jSource


def _fake_neo4j_node(element_id: str, labels: list[str], props: dict[str, Any]) -> Any:
    n = MagicMock()
    n.element_id = element_id
    n.labels = frozenset(labels)
    n.items.return_value = list(props.items())
    return n


def _fake_neo4j_rel(
    element_id: str, rtype: str, start: str, end: str, props: dict[str, Any]
) -> Any:
    r = MagicMock()
    r.element_id = element_id
    r.type = rtype
    r.start_node = MagicMock(element_id=start)
    r.end_node = MagicMock(element_id=end)
    r.items.return_value = list(props.items())
    return r


def _driver_yielding(node_rows: list[Any], rel_rows: list[Any]) -> Any:
    driver = MagicMock()

    def run(query: str, **_: Any) -> list[dict[str, Any]]:
        if "()-[" in query or "-[r" in query:
            return [{"r": r} for r in rel_rows]
        return [{"n": n} for n in node_rows]

    session = MagicMock()
    session.run.side_effect = run
    session.__enter__.return_value = session
    session.__exit__.return_value = False
    driver.session.return_value = session
    return driver


def test_nodes_mapped_to_source_nodes() -> None:
    node_rows = [
        _fake_neo4j_node("4:db:1", ["Person"], {"name": "Ada", "age": 36}),
        _fake_neo4j_node("4:db:2", ["Company", "Org"], {"name": "Acme"}),
    ]
    source = Neo4jSource(_driver_yielding(node_rows, []))

    out = list(source.nodes())

    assert out == [
        SourceNode(id="4:db:1", labels=["Person"], properties={"name": "Ada", "age": 36}),
        SourceNode(id="4:db:2", labels=["Company", "Org"], properties={"name": "Acme"}),
    ]


def test_relationships_mapped_to_source_relationships() -> None:
    rel_rows = [_fake_neo4j_rel("5:db:9", "WORKS_AT", "4:db:1", "4:db:2", {"since": 2020})]
    source = Neo4jSource(_driver_yielding([], rel_rows))

    out = list(source.relationships())

    assert out == [
        SourceRelationship(
            id="5:db:9", type="WORKS_AT", start="4:db:1", end="4:db:2", properties={"since": 2020}
        )
    ]


def test_close_closes_the_driver() -> None:
    driver = _driver_yielding([], [])
    Neo4jSource(driver).close()
    driver.close.assert_called_once()


def test_context_manager_closes_driver() -> None:
    driver = _driver_yielding([], [])
    with Neo4jSource(driver) as source:
        assert source is not None
    driver.close.assert_called_once()


def test_node_query_is_a_full_scan_returning_n() -> None:
    driver = _driver_yielding([], [])
    list(Neo4jSource(driver).nodes())
    called_query = driver.session.return_value.run.call_args[0][0]
    assert "MATCH (n)" in called_query
    assert "RETURN n" in called_query


def test_relationship_query_returns_r() -> None:
    driver = _driver_yielding([], [])
    list(Neo4jSource(driver).relationships())
    called_query = driver.session.return_value.run.call_args[0][0]
    assert "-[r" in called_query
    assert "RETURN r" in called_query
