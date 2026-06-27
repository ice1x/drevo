"""Tests for the FastMCP tool layer.

The tools are thin pass-throughs to the HTTP client, so we (1) inject a stub
client and assert each tool delegates + maps the result, and (2) assert every
tool is actually registered on the FastMCP instance.
"""

from __future__ import annotations

import asyncio
from typing import Any

import pytest

import drevo_mcp.server as server

EXPECTED_TOOLS = {
    "health",
    "node_get",
    "list_nodes_by_kind",
    "search_fts",
    "neighbors",
    "subgraph",
    "shortest_path",
    "count_nodes",
}


class StubClient:
    """Records calls and returns canned payloads in the HTTP client's shape."""

    def __init__(self) -> None:
        self.calls: list[tuple[Any, ...]] = []

    def health(self) -> dict[str, Any]:
        self.calls.append(("health",))
        return {"status": "ok"}

    def node_get(self, node_id: int) -> dict[str, Any] | None:
        self.calls.append(("node_get", node_id))
        return {"id": node_id} if node_id else None

    def list_nodes_by_kind(self, kind: str, limit: int = 50, offset: int = 0) -> dict[str, Any]:
        self.calls.append(("list", kind, limit, offset))
        return {"nodes": []}

    def search_fts(self, query: str, limit: int = 10) -> dict[str, Any]:
        self.calls.append(("fts", query, limit))
        return {"results": []}

    def neighbors(
        self, node_id: int, direction: str = "both", kind: str | None = None, depth: int = 1
    ) -> dict[str, Any]:
        self.calls.append(("neighbors", node_id, direction, kind, depth))
        return {"nodes": []}

    def subgraph(self, node_id: int, depth: int = 1) -> dict[str, Any]:
        self.calls.append(("subgraph", node_id, depth))
        return {"nodes": [], "edges": []}

    def shortest_path(self, from_id: int, to_id: int) -> dict[str, Any]:
        self.calls.append(("sp", from_id, to_id))
        return {"path": [from_id, to_id]}

    def export_json(self) -> dict[str, Any]:
        self.calls.append(("export",))
        return {"nodes": [{"id": 1}, {"id": 2}, {"id": 3}], "edges": []}


@pytest.fixture
def stub(monkeypatch: pytest.MonkeyPatch) -> StubClient:
    s = StubClient()
    monkeypatch.setattr(server, "_client", s)
    return s


def test_health_delegates(stub: StubClient) -> None:
    assert server.health() == {"status": "ok"}
    assert ("health",) in stub.calls


def test_node_get_delegates(stub: StubClient) -> None:
    assert server.node_get(7) == {"id": 7}
    assert server.node_get(0) is None


def test_search_fts_passes_limit(stub: StubClient) -> None:
    server.search_fts("alice", limit=5)
    assert ("fts", "alice", 5) in stub.calls


def test_neighbors_defaults(stub: StubClient) -> None:
    server.neighbors(2)
    assert ("neighbors", 2, "both", None, 1) in stub.calls


def test_shortest_path_delegates(stub: StubClient) -> None:
    assert server.shortest_path(1, 9) == {"path": [1, 9]}


def test_count_nodes_counts_export_dump(stub: StubClient) -> None:
    assert server.count_nodes() == {"count": 3}
    assert ("export",) in stub.calls


def test_all_tools_registered() -> None:
    tools = asyncio.run(server.mcp.list_tools())
    assert EXPECTED_TOOLS <= {tool.name for tool in tools}
