"""Unit tests for ``DrevoHttpClient`` — every method against a mocked HTTP layer.

``pytest_httpx`` intercepts the ``httpx`` transport, so these exercise the real
client (URL building, query params, body, error mapping) without a live server.
"""

from __future__ import annotations

import json

import pytest
from faker import Faker
from pytest_httpx import HTTPXMock

from drevo_mcp.client import DrevoHttpClient, DrevoHttpError

BASE = "http://drevo.test:8080"
fake = Faker()
Faker.seed(1234)


@pytest.fixture
def client() -> DrevoHttpClient:
    return DrevoHttpClient(BASE)


def test_base_url_strips_trailing_slash() -> None:
    assert DrevoHttpClient("http://x:8080/").base_url == "http://x:8080"


def test_base_url_from_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("DREVO_HTTP_URL", "http://env-host:9000")
    assert DrevoHttpClient().base_url == "http://env-host:9000"


def test_health(httpx_mock: HTTPXMock, client: DrevoHttpClient) -> None:
    httpx_mock.add_response(json={"status": "ok"})
    assert client.health() == {"status": "ok"}
    assert str(httpx_mock.get_request().url) == f"{BASE}/health"


def test_node_get_found(httpx_mock: HTTPXMock, client: DrevoHttpClient) -> None:
    node = {"id": 7, "kind": "person", "title": fake.name()}
    httpx_mock.add_response(json=node)
    assert client.node_get(7) == node
    assert httpx_mock.get_request().url.path == "/nodes/7"


def test_node_get_missing_returns_none(httpx_mock: HTTPXMock, client: DrevoHttpClient) -> None:
    httpx_mock.add_response(status_code=404, json={"error": "node not found", "status": 404})
    assert client.node_get(999) is None


def test_non_404_error_raises(httpx_mock: HTTPXMock, client: DrevoHttpClient) -> None:
    httpx_mock.add_response(status_code=400, json={"error": "bad id", "status": 400})
    with pytest.raises(DrevoHttpError) as excinfo:
        client.node_get(1)
    assert excinfo.value.status == 400
    assert "bad id" in excinfo.value.message


def test_list_nodes_by_kind_params(httpx_mock: HTTPXMock, client: DrevoHttpClient) -> None:
    httpx_mock.add_response(json={"nodes": []})
    client.list_nodes_by_kind("task", limit=10, offset=20)
    req = httpx_mock.get_request()
    assert req.url.path == "/nodes"
    assert req.url.params["kind"] == "task"
    assert req.url.params["limit"] == "10"
    assert req.url.params["offset"] == "20"


def test_search_fts_posts_body(httpx_mock: HTTPXMock, client: DrevoHttpClient) -> None:
    httpx_mock.add_response(json={"results": [{"node": {"id": 1}, "score": 2.5}]})
    out = client.search_fts("alice", limit=5)
    assert out["results"][0]["score"] == 2.5
    req = httpx_mock.get_request()
    assert req.method == "POST"
    assert json.loads(req.content) == {"query": "alice", "limit": 5}


def test_neighbors_omits_none_kind(httpx_mock: HTTPXMock, client: DrevoHttpClient) -> None:
    httpx_mock.add_response(json={"nodes": []})
    client.neighbors(3, direction="outgoing", depth=2)
    req = httpx_mock.get_request()
    assert req.url.path == "/nodes/3/neighbors"
    assert req.url.params["direction"] == "outgoing"
    assert req.url.params["depth"] == "2"
    assert "kind" not in req.url.params


def test_shortest_path_params(httpx_mock: HTTPXMock, client: DrevoHttpClient) -> None:
    httpx_mock.add_response(json={"path": [1, 4, 9]})
    assert client.shortest_path(1, 9) == {"path": [1, 4, 9]}
    req = httpx_mock.get_request()
    assert req.url.path == "/paths/shortest"
    assert req.url.params["from"] == "1"
    assert req.url.params["to"] == "9"


def test_subgraph(httpx_mock: HTTPXMock, client: DrevoHttpClient) -> None:
    httpx_mock.add_response(json={"nodes": [{"id": 1}], "edges": []})
    out = client.subgraph(1, depth=3)
    assert out["nodes"][0]["id"] == 1
    assert httpx_mock.get_request().url.params["depth"] == "3"
