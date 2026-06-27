"""Bolt drop-in of the Neo4j knowledge-graph MCP, pointed at drevo.

Same tools and Cypher as the Neo4j MCP, but connected to drevo's
Neo4j-compatible Bolt endpoint (a containerised ``drevo-server`` with
``DREVO_BOLT_PORT`` set). drevo speaks Bolt + the required Cypher subset
(``MERGE`` / ``datetime()`` / ``SET +=`` / map projection / ``labels()`` /
``type()`` / ``properties()`` / ``OPTIONAL MATCH`` / ``collect``), so this is a
genuine copy-and-swap drop-in.
"""

from __future__ import annotations

from drevo_mcp_bolt.graph import KnowledgeGraph

__all__ = ["KnowledgeGraph"]
__version__ = "0.1.0"
