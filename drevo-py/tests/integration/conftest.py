"""Shared fixtures for the Phase 16 task `00119` integration-test suite.

The integration tier always exercises the *disk* redb backend — that is
the boundary the suite was built to defend. Fixtures own the temp
directory life-cycle so individual tests stay free of cleanup
boilerplate.

Design notes:

* `tmp_db_path` returns a fresh, unused path inside a per-test temp
  directory. Tests that need to close and reopen the same database
  reuse the path across multiple `Drevo.open(path)` calls.
* `disk_db` is a one-shot convenience: open once, yield, close on
  scope exit. Used by tests that don't need to assert reopen
  semantics.
* `tag_corpus` is the shared fixture for pagination + listing tests.
  20 rows is enough to drive `(limit=5, offset=k)` paging without
  ballooning test time.
"""

from __future__ import annotations

import os
import tempfile
from collections.abc import Iterator
from typing import Callable

import pytest

import drevo


@pytest.fixture
def tmp_db_path() -> Iterator[str]:
    """Yield a fresh database path inside a per-test temp directory.

    The directory is removed on scope exit. Tests that want to assert
    durability open the same `path` multiple times via
    `drevo.Drevo.open(path)`; the lockfile is released when the handle
    is closed, so successive opens succeed.
    """
    with tempfile.TemporaryDirectory(prefix="drevo-it-") as td:
        yield os.path.join(td, "graph.drevo")


@pytest.fixture
def disk_db(tmp_db_path: str) -> Iterator[drevo.Drevo]:
    """One-shot disk-backed `Drevo` handle.

    For tests that don't care about reopen semantics — they just want
    the real redb backend behind the public Python surface (no
    in-memory shortcut).
    """
    with drevo.Drevo.open(tmp_db_path) as db:
        yield db


@pytest.fixture
def make_tag_corpus(disk_db: drevo.Drevo) -> Callable[[int, str], list[drevo.Node]]:
    """Factory that creates `count` nodes of the given kind.

    Titles are zero-padded so lexicographic order = insertion order =
    creation `id` order. Tests assert against this stable order when
    reasoning about pagination.
    """

    def make(count: int, kind: str = "tag") -> list[drevo.Node]:
        return [
            disk_db.create_node(drevo.NewNode(kind=kind, title=f"{kind}-{i:03d}"))
            for i in range(count)
        ]

    return make
