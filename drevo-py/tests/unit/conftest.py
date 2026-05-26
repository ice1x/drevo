"""Shared fixtures for the Phase 16 task `00118` unit-test suite.

These fixtures favour *cheap*, *deterministic*, *focused* dependencies:

* `drevo_db` — an in-memory drevo handle. Each test gets a fresh one.
  The in-memory backend is the standard "unit-level boundary" for
  embedded databases: no disk I/O, no lockfile, no cleanup, but still
  the *real* storage code path. Pure `unittest.mock.Mock`-only tests
  belong further down (`mock_drevo`).
* `mock_drevo` — a `unittest.mock.Mock` typed as a `drevo.Drevo`. Used
  by `drevo.rag` algorithmic tests where we want to assert on the
  method-call signature without paying for real storage.
* `det_embedder` — a deterministic toy embedder (hash + bag-of-chars).
  Lets the MMR / ingest tests assert exact floats without depending
  on an external model.
* `connected_chain` — a 4-node linear chain (`a → b → c → d`) in the
  in-memory backend; the BFS / Retriever tests reuse it.

RFC §8 + the `drevo-tdd` skill (§"Unit tests") informs the *boundary*
this suite operates against — exactly one Python-level public symbol
per test, where possible.
"""

from __future__ import annotations

import hashlib
from typing import Any, Callable
from unittest.mock import MagicMock

import pytest

import drevo


# ── handle fixtures ──────────────────────────────────────────────────


@pytest.fixture
def drevo_db():
    """Yield a fresh in-memory drevo handle; close on scope exit.

    The in-memory backend is the canonical unit-level storage stand-in
    — real code paths, zero disk, zero lockfile. A test that needs
    behaviour the in-memory backend lacks (durability, file lock
    contention, recovery) belongs in `tests/integration/` (00119), not
    here.
    """
    with drevo.Drevo.open_in_memory() as db:
        yield db


@pytest.fixture
def mock_drevo():
    """A `unittest.mock.MagicMock` typed as a `drevo.Drevo`.

    Used by `drevo.rag` tests that exercise the *dispatch* layer (e.g.
    `Retriever._resolve_seeds` calling `search_fts` for a `str` seed).
    The point is to assert exactly which `Drevo.*` method gets called
    with which args — not to verify storage semantics, which is the
    in-memory fixture's job.
    """
    m = MagicMock(spec=drevo.Drevo)
    return m


# ── deterministic embedder ───────────────────────────────────────────


def _hash_embed(text: str, dim: int = 8) -> list[float]:
    """Deterministic toy embedder: SHA-256 of the text, taken `dim`
    bytes at a time, normalised to [-1, 1]. Pure-Python, no NumPy,
    constant across runs and Python versions.
    """
    digest = hashlib.sha256(text.encode("utf-8")).digest()
    if dim > len(digest):
        # Stretch via repeated rehash so callers can ask for arbitrary
        # dims; we never need more than ~16 in the unit suite anyway.
        digest = (digest * ((dim // len(digest)) + 1))[:dim]
    return [(b - 128) / 128.0 for b in digest[:dim]]


@pytest.fixture
def det_embedder() -> Callable[[list[str]], list[list[float]]]:
    """Deterministic embedder for ingest + MMR unit tests.

    A test that wants a specific embedding shape can call this fixture
    and assert on the bytes-derived vector. Production embedders
    (OpenAI, sentence-transformers, local LLMs) live above this layer
    and never appear in the unit suite.
    """

    def embed(texts: list[str]) -> list[list[float]]:
        return [_hash_embed(t) for t in texts]

    return embed


@pytest.fixture
def orthogonal_embedder() -> Callable[[list[str]], list[list[float]]]:
    """Embedder that returns one-hot vectors keyed by the input string.

    Used by the MMR diversity tests: distinct strings get orthogonal
    embeddings (cosine = 0), identical strings get identical vectors
    (cosine = 1). Lets us assert "different-titled items are picked
    over similar-titled items" in closed form.
    """
    table: dict[str, list[float]] = {}

    def embed(texts: list[str]) -> list[list[float]]:
        out: list[list[float]] = []
        for t in texts:
            if t not in table:
                vec = [0.0] * (len(table) + 1)
                for old, old_vec in table.items():
                    table[old] = old_vec + [0.0]
                vec[len(table)] = 1.0
                table[t] = vec
            out.append(list(table[t]))
        # Pad earlier vectors to the current dim if the alphabet grew.
        max_dim = max((len(v) for v in out), default=0)
        return [v + [0.0] * (max_dim - len(v)) for v in out]

    return embed


# ── shared graph topologies ──────────────────────────────────────────


@pytest.fixture
def connected_chain(drevo_db: drevo.Drevo):
    """A 4-node linear chain: a → b → c → d (kind=note, edge=links_to).

    Returned as a `dict[str, drevo.Node]` so tests can pull e.g.
    `chain["a"]` for the BFS root without depending on insertion
    order.
    """
    nodes = {
        name: drevo_db.create_node(drevo.NewNode(kind="note", title=name))
        for name in ("a", "b", "c", "d")
    }
    edges = [
        ("a", "b"),
        ("b", "c"),
        ("c", "d"),
    ]
    for src, dst in edges:
        drevo_db.create_edge(
            drevo.NewEdge(from_id=nodes[src].id, to_id=nodes[dst].id, kind="links_to")
        )
    return nodes


@pytest.fixture
def mixed_kind_neighbourhood(drevo_db: drevo.Drevo):
    """Root (kind=task) wired to one node of every helper kind.

    Returns ``(root, others)`` where `others: dict[str, drevo.Node]`
    keys by kind. Used by `expand_neighborhood`'s kind-filter tests.
    """
    root = drevo_db.create_node(drevo.NewNode(kind="task", title="root"))
    others = {
        kind: drevo_db.create_node(drevo.NewNode(kind=kind, title=f"{kind}-leaf"))
        for kind in ("note", "tag", "person")
    }
    for kind, node in others.items():
        drevo_db.create_edge(drevo.NewEdge(from_id=root.id, to_id=node.id, kind="relates_to"))
    return root, others


# ── candidate factory for MMR ────────────────────────────────────────


@pytest.fixture
def make_scored() -> Callable[[str, float], Any]:
    """Construct a duck-typed MMR candidate (`.node.title`, `.score`).

    `MMRReranker.rerank` only reads `c.node.title` and `c.score`, so
    the unit suite uses a tiny stand-in rather than pulling
    `drevo.ScoredNode` (which would require a real Node).
    """

    class _StubNode:
        def __init__(self, title: str) -> None:
            self.title = title

    class _StubScored:
        def __init__(self, title: str, score: float) -> None:
            self.node = _StubNode(title)
            self.score = float(score)

        def __repr__(self) -> str:
            return f"<Stub {self.node.title} score={self.score}>"

    def make(title: str, score: float) -> Any:
        return _StubScored(title, score)

    return make
