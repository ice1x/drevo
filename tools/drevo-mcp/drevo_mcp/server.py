"""FastMCP server: read-only MCP tools backed by drevo's HTTP API.

Run with ``python -m drevo_mcp`` (or the ``drevo-mcp`` console script). The
server speaks MCP over stdio; an AI client spawns it as a subprocess. Every
tool delegates to :class:`drevo_mcp.client.DrevoHttpClient`, so the network
logic is unit-tested in one place and the tool bodies stay one-liners.
"""

from __future__ import annotations

from typing import Any, Callable

from fastmcp import FastMCP

from .client import DrevoHttpClient

mcp: FastMCP = FastMCP("drevo")

_client: DrevoHttpClient | None = None


def client() -> DrevoHttpClient:
    """Return the process-wide HTTP client, created lazily on first use."""
    global _client
    if _client is None:
        _client = DrevoHttpClient()
    return _client


def health() -> dict[str, Any]:
    """Liveness probe of the drevo server. Returns ``{"status": "ok"}``."""
    return client().health()


def node_get(node_id: int) -> dict[str, Any] | None:
    """Fetch a single node by numeric id; ``null`` if it does not exist."""
    return client().node_get(node_id)


def list_nodes_by_kind(kind: str, limit: int = 50, offset: int = 0) -> dict[str, Any]:
    """Page through nodes of a given kind (label). Returns ``{"nodes": [...]}``."""
    return client().list_nodes_by_kind(kind, limit=limit, offset=offset)


def search_fts(query: str, limit: int = 10) -> dict[str, Any]:
    """Full-text search node titles/bodies. Returns BM25-scored ``{"results": [...]}``."""
    return client().search_fts(query, limit=limit)


def neighbors(
    node_id: int, direction: str = "both", kind: str | None = None, depth: int = 1
) -> dict[str, Any]:
    """Neighbours of a node up to ``depth`` hops. Returns ``{"nodes": [...]}``.

    ``direction`` is ``"outgoing"`` | ``"incoming"`` | ``"both"``; ``kind``
    optionally filters by edge kind.
    """
    return client().neighbors(node_id, direction=direction, kind=kind, depth=depth)


def subgraph(node_id: int, depth: int = 1) -> dict[str, Any]:
    """Subgraph within ``depth`` hops of a node. Returns ``{"nodes": [...], "edges": [...]}``."""
    return client().subgraph(node_id, depth=depth)


def shortest_path(from_id: int, to_id: int) -> dict[str, Any]:
    """Dijkstra shortest path between two nodes. Returns ``{"path": [ids] | null}``."""
    return client().shortest_path(from_id, to_id)


def count_nodes() -> dict[str, int]:
    """Total number of nodes in the graph. Returns ``{"count": n}``.

    Note: drevo's HTTP API has no dedicated count endpoint, so this downloads
    the full ``/export/json`` dump and counts its ``nodes`` array — fine for
    modest graphs; a future ``/stats`` endpoint would make this O(1).
    """
    dump = client().export_json()
    nodes = dump.get("nodes", [])
    return {"count": len(nodes)}


# Register every tool with the FastMCP instance. We call ``mcp.tool(fn)``
# (the non-decorator form) and discard the returned Tool object so the
# module-level names stay plain, directly-callable functions for unit tests.
_TOOLS: tuple[Callable[..., Any], ...] = (
    health,
    node_get,
    list_nodes_by_kind,
    search_fts,
    neighbors,
    subgraph,
    shortest_path,
    count_nodes,
)
for _fn in _TOOLS:
    mcp.tool(_fn)


def main() -> None:
    """Run the MCP server over stdio (blocks until stdin EOF)."""
    mcp.run()


if __name__ == "__main__":
    main()
