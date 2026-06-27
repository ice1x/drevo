"""Thin HTTP client over the drevo HTTP API.

Every method maps one-to-one onto a ``drevo-server`` endpoint (see
``src/api.rs``). Errors are surfaced as :class:`DrevoHttpError` carrying the
server's ``{"error": ..., "status": ...}`` body. This is the only module that
touches the network, so the FastMCP tool layer stays trivial and testable.
"""

from __future__ import annotations

import os
from typing import Any, cast

import httpx

DEFAULT_BASE_URL = "http://localhost:8080"
DEFAULT_TIMEOUT = 30.0


class DrevoHttpError(RuntimeError):
    """A non-2xx response from ``drevo-server``.

    Carries the HTTP ``status`` code and the server's human-readable
    ``message`` (the ``error`` field of the JSON body when present).
    """

    def __init__(self, status: int, message: str) -> None:
        super().__init__(f"drevo HTTP {status}: {message}")
        self.status = status
        self.message = message


class DrevoHttpClient:
    """Synchronous client for a running ``drevo-server``.

    The base URL comes from the ``base_url`` argument, else the
    ``DREVO_HTTP_URL`` environment variable, else ``http://localhost:8080``.
    A custom ``httpx.Client`` may be injected (used by tests).
    """

    def __init__(
        self,
        base_url: str | None = None,
        *,
        timeout: float = DEFAULT_TIMEOUT,
        client: httpx.Client | None = None,
    ) -> None:
        resolved = base_url or os.environ.get("DREVO_HTTP_URL", DEFAULT_BASE_URL)
        self.base_url = resolved.rstrip("/")
        self._client = client if client is not None else httpx.Client(timeout=timeout)

    # ── transport ───────────────────────────────────────────────────────────
    def _request(self, method: str, path: str, **kwargs: Any) -> Any:
        response = self._client.request(method, f"{self.base_url}{path}", **kwargs)
        if response.status_code >= 400:
            message = response.text
            try:
                body = response.json()
            except ValueError:
                body = None
            if isinstance(body, dict) and "error" in body:
                message = str(body["error"])
            raise DrevoHttpError(response.status_code, message)
        if response.status_code == 204 or not response.content:
            return None
        return response.json()

    def _get(self, path: str, params: dict[str, Any] | None = None) -> Any:
        # Drop None-valued params so optional filters (e.g. edge `kind`) are
        # omitted entirely rather than sent as an empty `kind=` that would
        # match nothing — httpx serialises None as an empty value, not absence.
        if params is not None:
            params = {key: value for key, value in params.items() if value is not None}
        return self._request("GET", path, params=params)

    def _post(self, path: str, json: dict[str, Any]) -> Any:
        return self._request("POST", path, json=json)

    # ── endpoints (mirror src/api.rs) ───────────────────────────────────────
    def health(self) -> dict[str, Any]:
        """GET /health — ``{"status": "ok"}`` while serving."""
        return cast("dict[str, Any]", self._get("/health"))

    def node_get(self, node_id: int) -> dict[str, Any] | None:
        """GET /nodes/{id} — the full node, or ``None`` if it does not exist."""
        try:
            return cast("dict[str, Any]", self._get(f"/nodes/{node_id}"))
        except DrevoHttpError as err:
            if err.status == 404:
                return None
            raise

    def list_nodes_by_kind(self, kind: str, limit: int = 50, offset: int = 0) -> dict[str, Any]:
        """GET /nodes?kind=&limit=&offset= — ``{"nodes": [...]}``."""
        params = {"kind": kind, "limit": limit, "offset": offset}
        return cast("dict[str, Any]", self._get("/nodes", params))

    def search_fts(self, query: str, limit: int = 10) -> dict[str, Any]:
        """POST /search/fts — ``{"results": [{"node": ..., "score": ...}]}``."""
        return cast("dict[str, Any]", self._post("/search/fts", {"query": query, "limit": limit}))

    def neighbors(
        self,
        node_id: int,
        direction: str = "both",
        kind: str | None = None,
        depth: int = 1,
    ) -> dict[str, Any]:
        """GET /nodes/{id}/neighbors — ``{"nodes": [...]}``."""
        params = {"direction": direction, "kind": kind, "depth": depth}
        return cast("dict[str, Any]", self._get(f"/nodes/{node_id}/neighbors", params))

    def subgraph(self, node_id: int, depth: int = 1) -> dict[str, Any]:
        """GET /nodes/{id}/subgraph — ``{"nodes": [...], "edges": [...]}``."""
        return cast("dict[str, Any]", self._get(f"/nodes/{node_id}/subgraph", {"depth": depth}))

    def shortest_path(self, from_id: int, to_id: int) -> dict[str, Any]:
        """GET /paths/shortest?from=&to= — ``{"path": [ids] | null}``."""
        return cast("dict[str, Any]", self._get("/paths/shortest", {"from": from_id, "to": to_id}))

    def export_json(self) -> dict[str, Any]:
        """GET /export/json — the full ``drevo-json-v1`` dump."""
        return cast("dict[str, Any]", self._get("/export/json"))

    def close(self) -> None:
        self._client.close()
