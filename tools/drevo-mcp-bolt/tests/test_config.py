"""Always-on unit checks (no Bolt server needed)."""

from __future__ import annotations

import pytest

import drevo_mcp_bolt.server as server
from drevo_mcp_bolt.graph import KnowledgeGraph


def test_drv_raises_when_not_connected() -> None:
    kg = KnowledgeGraph(uri="bolt://x", username="u", password="p")
    with pytest.raises(RuntimeError):
        _ = kg._drv


def test_server_default_uri_targets_drevo_bolt_port() -> None:
    # Defaults point at the drevo container's Bolt port, not a generic Neo4j.
    assert server._BOLT_URI.endswith(":7687")
    assert server.kg.uri.endswith(":7687")
