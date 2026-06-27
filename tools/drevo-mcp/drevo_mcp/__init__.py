"""External FastMCP server exposing a running ``drevo-server`` as MCP tools.

This package is an **MCP client of drevo's HTTP API**, not of the redb file:
it speaks the Model Context Protocol over stdio to an AI client (Claude
Desktop / Claude Code / Cline) and translates each tool call into an HTTP
request against a running ``drevo-server`` (default ``http://localhost:8080``,
override with ``DREVO_HTTP_URL``). Because it never opens ``drevo.redb``
directly, it does not contend with the server for redb's single-process file
lock — the embedded Rust ``drevo-mcp`` binary's core limitation.
"""

from __future__ import annotations

from .client import DrevoHttpClient, DrevoHttpError

__all__ = ["DrevoHttpClient", "DrevoHttpError"]
__version__ = "0.1.0"
