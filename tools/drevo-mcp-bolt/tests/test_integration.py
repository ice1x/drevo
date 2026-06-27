"""End-to-end: the drop-in's tools against a real drevo Bolt server.

Spawns a local ``drevo-server`` with Bolt enabled and drives the knowledge-graph
operations through the same ``KnowledgeGraph`` the MCP tools use. Skipped when
the ``drevo-server`` binary is not built (e.g. a pure-Python CI cell).

Synchronous (spawn the server, then run the async client work via
``asyncio.run``) — driving a blocking subprocess from inside a pytest-asyncio
event loop is unreliable.
"""

from __future__ import annotations

import asyncio
import os
import pathlib
import socket
import subprocess
import time

import pytest

from drevo_mcp_bolt.graph import KnowledgeGraph


def _find_server() -> str | None:
    # Use the most recently built binary of the two profiles — a stale build
    # (e.g. a release binary predating the Bolt wiring) would silently ignore
    # DREVO_BOLT_PORT and never open the Bolt listener.
    root = pathlib.Path(__file__).resolve().parents[3]
    candidates = [root / "target" / p / "drevo-server" for p in ("release", "debug")]
    existing = [c for c in candidates if c.exists()]
    if not existing:
        return None
    return str(max(existing, key=lambda c: c.stat().st_mtime))


def _two_free_ports() -> tuple[int, int]:
    # Hold both sockets open at once so the OS hands back two *distinct* ports.
    s1 = socket.socket()
    s2 = socket.socket()
    s1.bind(("127.0.0.1", 0))
    s2.bind(("127.0.0.1", 0))
    p1, p2 = int(s1.getsockname()[1]), int(s2.getsockname()[1])
    s1.close()
    s2.close()
    return p1, p2


async def _exercise(bolt_port: int) -> None:
    kg = KnowledgeGraph(uri=f"bolt://127.0.0.1:{bolt_port}", username="neo4j", password="x")
    await kg.connect()
    try:
        ent = await kg.create_entity("alice", "Service", "proj", ["likes graphs"], {"team": "core"})
        assert ent["name"] == "alice"
        assert "Entity" in ent["labels"]
        assert ent["team"] == "core"  # SET += merged
        assert str(ent["created_at"]).endswith("Z")  # datetime() ISO string

        got = await kg.get_entity("alice", "proj")
        assert got["entity"]["name"] == "alice"

        found = await kg.search("alice", "proj")
        assert any(e["name"] == "alice" for e in found)

        rel = await kg.create_relationship("alice", "alice", "KNOWS", "proj")
        assert rel["type"] == "KNOWS"

        assert "proj" in await kg.list_projects()
        assert await kg.delete_entity("alice", "proj") is True
    finally:
        await kg.close()


_SERVER = _find_server()


@pytest.mark.skipif(_SERVER is None, reason="drevo-server binary not built")
def test_dropin_crud_over_drevo_bolt(tmp_path: pathlib.Path) -> None:
    http_port, bolt_port = _two_free_ports()
    env = {
        **os.environ,
        "DREVO_HOST": "127.0.0.1",
        "DREVO_PORT": str(http_port),
        "DREVO_BOLT_PORT": str(bolt_port),
        "DREVO_DATA_DIR": str(tmp_path),
    }
    assert _SERVER is not None
    log_path = tmp_path / "drevo-server.log"
    log = log_path.open("wb")
    proc = subprocess.Popen([_SERVER], env=env, stdout=log, stderr=subprocess.STDOUT)
    try:
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                log.flush()
                raise AssertionError(
                    f"drevo-server exited early (code {proc.returncode}):\n"
                    f"{log_path.read_text(errors='replace')}"
                )
            try:
                with socket.create_connection(("127.0.0.1", bolt_port), timeout=0.2):
                    break
            except OSError:
                time.sleep(0.1)
        else:
            log.flush()
            raise AssertionError(
                f"bolt port {bolt_port} never opened:\n{log_path.read_text(errors='replace')}"
            )
        asyncio.run(_exercise(bolt_port))
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        log.close()
