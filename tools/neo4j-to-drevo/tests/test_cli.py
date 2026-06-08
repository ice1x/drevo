"""Unit tests for the `neo4j_to_drevo` three-phase CLI (dump / import / dry-run).

Orchestration is tested with **injected factories** so no real Neo4j
connection or disk file is needed.
"""

from __future__ import annotations

import json
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest

import drevo
from neo4j_to_drevo import SourceNode, SourceRelationship
from neo4j_to_drevo.cli import build_parser, main


class _FakeSource:
    def __init__(self) -> None:
        self._nodes = [
            SourceNode(id="a", labels=["Person"], properties={"name": "Ann"}),
            SourceNode(id="b", labels=["Person"], properties={"name": "Bo"}),
        ]
        self._rels = [SourceRelationship(id="r", type="KNOWS", start="a", end="b", properties={})]
        self.closed = False

    def nodes(self) -> Iterator[SourceNode]:
        return iter(self._nodes)

    def relationships(self) -> Iterator[SourceRelationship]:
        return iter(self._rels)

    def close(self) -> None:
        self.closed = True


def _boom(_ns: Any) -> Any:
    raise AssertionError("factory must not be called")


# ── parser ───────────────────────────────────────────────────────────


def test_a_subcommand_is_required() -> None:
    with pytest.raises(SystemExit):
        build_parser().parse_args([])


def test_dump_parses() -> None:
    ns = build_parser().parse_args(
        ["dump", "--neo4j-uri", "bolt://x", "--neo4j-user", "neo4j", "--out", "g.json"]
    )
    assert ns.command == "dump"
    assert ns.out == "g.json"


def test_import_parses_apoc_and_dry_run() -> None:
    ns = build_parser().parse_args(
        ["import", "--apoc-json", "g.json", "--drevo-path", "g.redb", "--dry-run"]
    )
    assert ns.command == "import"
    assert ns.apoc_json == "g.json"
    assert ns.dry_run is True


def test_import_rejects_both_sources() -> None:
    with pytest.raises(SystemExit):
        build_parser().parse_args(
            ["import", "--apoc-json", "g.json", "--neo4j-uri", "bolt://x", "--in-memory"]
        )


def test_import_requires_a_source() -> None:
    with pytest.raises(SystemExit):
        build_parser().parse_args(["import", "--in-memory"])


# ── phase 1: dump ────────────────────────────────────────────────────


def test_dump_writes_file_and_reports(tmp_path: Path, capsys: Any) -> None:
    src = _FakeSource()
    out = tmp_path / "graph.json"

    code = main(
        ["dump", "--neo4j-uri", "bolt://x", "--neo4j-user", "neo4j", "--out", str(out)],
        source_factory=lambda _ns: src,
    )

    assert code == 0
    lines = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    assert sum(1 for o in lines if o["type"] == "node") == 2
    assert sum(1 for o in lines if o["type"] == "relationship") == 1
    assert "dump complete" in capsys.readouterr().out
    assert src.closed is True


# ── phase 2: dry run ─────────────────────────────────────────────────


def test_dry_run_writes_nothing_and_never_touches_db_factory(capsys: Any) -> None:
    src = _FakeSource()

    code = main(
        ["import", "--apoc-json", "ignored.json", "--drevo-path", "would-be.redb", "--dry-run"],
        source_factory=lambda _ns: src,
        db_factory=_boom,  # must NOT be called on a dry run
    )

    assert code == 0
    out = capsys.readouterr().out
    assert "dry run" in out.lower()
    assert "would-be.redb" in out
    assert src.closed is True


# ── phase 3: import ──────────────────────────────────────────────────


def test_import_runs_migration_and_reports(capsys: Any) -> None:
    src = _FakeSource()
    db = drevo.Drevo.open_in_memory()

    code = main(
        ["import", "--apoc-json", "ignored.json", "--in-memory"],
        source_factory=lambda _ns: src,
        db_factory=lambda _ns: db,
    )

    assert code == 0
    out = capsys.readouterr().out
    assert "migration complete" in out
    assert "2 nodes" in out
    assert src.closed is True
    db.close()


def test_import_returns_nonzero_on_failure(capsys: Any) -> None:
    def boom(_ns: Any) -> Any:
        raise RuntimeError("connection refused")

    code = main(
        ["import", "--neo4j-uri", "bolt://x", "--neo4j-user", "neo4j", "--in-memory"],
        source_factory=boom,
        db_factory=lambda _ns: drevo.Drevo.open_in_memory(),
    )
    assert code != 0
    assert "connection refused" in capsys.readouterr().err
