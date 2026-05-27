"""Shared fixtures for the Phase 16 task `00120` end-to-end suite.

The e2e tier always exercises the on-disk redb backend through
``drevo.Drevo.open(tempfile)`` — same boundary as the 00119
integration suite, but each scenario drives a full domain workflow
top-to-bottom rather than a single cross-component invariant.

Fixtures own the temp directory life-cycle so individual tests stay
free of cleanup boilerplate. The deterministic embedder lives here
because every scenario that touches ``drevo.rag.ingest_documents``
wants the same byte-stable vectors — keeping it in one place stops
test drift.
"""

from __future__ import annotations

import hashlib
import os
import tempfile
from collections.abc import Iterator
from typing import Callable

import pytest

import drevo


@pytest.fixture
def tmp_db_path() -> Iterator[str]:
    """Yield a fresh database path inside a per-test temp directory.

    The directory is removed on scope exit. Scenarios that want to
    reopen the same path within one test (durability sub-checks) hold
    onto ``path`` after the first ``open(...)`` returns and call
    ``Drevo.open(path)`` again once they have closed the original
    handle — the on-disk lockfile is released by ``close()``.
    """
    with tempfile.TemporaryDirectory(prefix="drevo-e2e-") as td:
        yield os.path.join(td, "graph.drevo")


@pytest.fixture
def disk_db(tmp_db_path: str) -> Iterator[drevo.Drevo]:
    """One-shot disk-backed ``Drevo`` handle.

    Scenarios that don't need to assert reopen semantics use this so
    the body of the test reads as straight domain code.
    """
    with drevo.Drevo.open(tmp_db_path) as db:
        yield db


@pytest.fixture
def deterministic_embedder() -> Callable[[list[str]], list[list[float]]]:
    """Stub embedder used by every RAG-flavoured scenario.

    Strategy:
      * SHA-256 the input string.
      * Slice the digest into 8 little-endian ``u32`` words.
      * Normalise each word to a ``float`` in ``[0.0, 1.0]``.

    Yields a deterministic 8-dimensional vector per string. The
    Phase 12 vector index (task ``00075``) is not yet wired into the
    Python surface, so the embedder output only needs to round-trip
    through node properties — actual similarity is not exercised here.
    Keeping the function tiny (no numpy dependency) means the e2e
    suite stays runnable inside a bare wheel install.
    """

    def embed(texts: list[str]) -> list[list[float]]:
        out: list[list[float]] = []
        for text in texts:
            digest = hashlib.sha256(text.encode("utf-8")).digest()
            vec = [
                int.from_bytes(digest[i : i + 4], "little") / float(2**32) for i in range(0, 32, 4)
            ]
            out.append(vec)
        return out

    return embed


def must_get_node(db: drevo.Drevo, node_id: int) -> drevo.Node:
    """Look up a node by id and assert it exists.

    Scenarios know every id was just created in the same test, so a
    ``None`` return here is a programmer error, not a recoverable
    state. The helper exists so ``mypy --strict`` can narrow
    ``Optional[Node]`` to ``Node`` without per-call-site assertions
    sprinkled through every scenario.
    """
    node = db.get_node(node_id)
    assert node is not None, f"expected node id={node_id} to exist"
    return node
