"""MCP server exposing a project knowledge graph backed by drevo over Bolt.

A Bolt drop-in of the Neo4j knowledge-graph MCP: identical tools and Cypher,
but pointed at drevo's Neo4j-compatible Bolt endpoint (a containerised
`drevo-server` with `DREVO_BOLT_PORT` set). Connect a Neo4j instance or a drevo
container — the tool surface is the same.
"""

from __future__ import annotations

import argparse
import json
import os
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from typing import Any

from mcp.server.fastmcp import FastMCP

from drevo_mcp_bolt.graph import KnowledgeGraph

# ── Configuration ─────────────────────────────────────────────────────
# Point at drevo's Bolt endpoint by default (the container sets
# DREVO_BOLT_PORT=7687). drevo's Bolt runs without auth, so the username /
# password are accepted and ignored — they only matter against real Neo4j.

_BOLT_URI = os.getenv("DREVO_BOLT_URL", "bolt://localhost:7687")
_BOLT_USER = os.getenv("DREVO_BOLT_USER", "neo4j")
_BOLT_PASS = os.getenv("DREVO_BOLT_PASSWORD", "drevo")
_BOLT_DB = os.getenv("DREVO_BOLT_DATABASE", "neo4j")

kg = KnowledgeGraph(uri=_BOLT_URI, username=_BOLT_USER, password=_BOLT_PASS, database=_BOLT_DB)


@asynccontextmanager
async def lifespan(_server: FastMCP) -> AsyncIterator[None]:
    await kg.connect()
    try:
        yield
    finally:
        await kg.close()


mcp = FastMCP(
    "drevo-knowledge-graph",
    instructions=(
        "Knowledge Graph MCP — store and query project knowledge, domain "
        "models, and schema migrations in drevo over Bolt (Neo4j-compatible)."
    ),
    lifespan=lifespan,
)


def _json(obj: Any) -> str:
    """Serialise arbitrary Bolt results to JSON."""

    def default(o: Any) -> Any:
        if hasattr(o, "isoformat"):
            return o.isoformat()
        return str(o)

    return json.dumps(obj, indent=2, default=default, ensure_ascii=False)


# ── Tools: Entities ───────────────────────────────────────────────────


@mcp.tool()
async def create_entity(
    name: str,
    entity_type: str,
    project: str,
    observations: list[str] | None = None,
    properties: dict[str, Any] | None = None,
) -> str:
    """Create or update a knowledge entity in the graph."""
    result = await kg.create_entity(name, entity_type, project, observations, properties)
    return _json(result)


@mcp.tool()
async def add_observations(name: str, project: str, observations: list[str]) -> str:
    """Append new observations to an existing entity."""
    result = await kg.add_observations(name, project, observations)
    return _json(result)


@mcp.tool()
async def delete_entity(name: str, project: str) -> str:
    """Delete an entity and all its relationships."""
    deleted = await kg.delete_entity(name, project)
    return _json({"deleted": deleted})


# ── Tools: Relationships ─────────────────────────────────────────────


@mcp.tool()
async def create_relationship(
    from_entity: str,
    to_entity: str,
    relation_type: str,
    project: str,
    properties: dict[str, Any] | None = None,
) -> str:
    """Create a typed relationship between two entities."""
    result = await kg.create_relationship(
        from_entity, to_entity, relation_type, project, properties
    )
    return _json(result)


@mcp.tool()
async def delete_relationship(
    from_entity: str, to_entity: str, relation_type: str, project: str
) -> str:
    """Delete a relationship between two entities."""
    deleted = await kg.delete_relationship(from_entity, to_entity, relation_type, project)
    return _json({"deleted": deleted})


# ── Tools: Queries ────────────────────────────────────────────────────


@mcp.tool()
async def get_entity(name: str, project: str) -> str:
    """Get an entity with all its incoming and outgoing relationships."""
    result = await kg.get_entity(name, project)
    return _json(result)


@mcp.tool()
async def search_knowledge(query: str, project: str | None = None) -> str:
    """Search the knowledge graph by text (entity names and observations)."""
    results = await kg.search(query, project)
    return _json(results)


@mcp.tool()
async def get_project_graph(project: str) -> str:
    """Get the complete knowledge graph for a project."""
    result = await kg.get_project_graph(project)
    return _json(result)


@mcp.tool()
async def list_projects() -> str:
    """List all projects stored in the knowledge graph."""
    projects = await kg.list_projects()
    return _json(projects)


# ── Tools: Migrations ────────────────────────────────────────────────


@mcp.tool()
async def add_migration(
    project: str,
    description: str,
    cypher_up: str,
    cypher_down: str | None = None,
    version: str | None = None,
) -> str:
    """Record a graph schema/data migration for a project."""
    result = await kg.add_migration(project, description, cypher_up, cypher_down, version)
    return _json(result)


@mcp.tool()
async def get_migrations(project: str) -> str:
    """Get the full migration history for a project."""
    results = await kg.get_migrations(project)
    return _json(results)


@mcp.tool()
async def apply_migration(project: str, seq: int) -> str:
    """Execute a pending migration and mark it as applied."""
    result = await kg.apply_migration(project, seq)
    return _json(result)


# ── Tools: Raw Cypher ─────────────────────────────────────────────────


@mcp.tool()
async def run_cypher(query: str, params: dict[str, Any] | None = None) -> str:
    """Execute a Cypher query against the knowledge graph."""
    results = await kg.run_cypher(query, params)
    return _json(results)


# ── Entrypoint ────────────────────────────────────────────────────────


def main() -> None:
    parser = argparse.ArgumentParser(description="drevo Knowledge Graph MCP Server (Bolt)")
    parser.add_argument("--db-url", default=_BOLT_URI, help="drevo Bolt connection URI")
    parser.add_argument("--username", default=_BOLT_USER)
    parser.add_argument("--password", default=_BOLT_PASS)
    parser.add_argument("--database", default=_BOLT_DB)
    parser.add_argument("--transport", default="stdio", choices=["stdio", "sse", "streamable-http"])
    args = parser.parse_args()

    kg.uri = args.db_url
    kg.username = args.username
    kg.password = args.password
    kg.database = args.database

    mcp.run(transport=args.transport)


if __name__ == "__main__":
    main()
