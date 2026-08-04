"""Unit tests for the `drevo` CLI argument parser and error handling.

Parser-shape + exit-code contract only (no real database work — that's the
integration round-trip test). Mirrors the argparse `build_parser()` / `main()`
pattern: `main` returns a nonzero code on failure rather than raising.
"""

from __future__ import annotations

import pytest
from drevo.__main__ import build_parser, main


def test_parser_accepts_each_subcommand() -> None:
    parser = build_parser()
    for argv in (
        ["dump", "db.redb", "out.graphml"],
        ["restore", "in.graphml", "db.redb"],
        ["compact", "db.redb"],
        ["shrink", "src.redb", "dst.redb"],
        ["migrate", "up", "db.redb"],
        ["migrate", "down", "db.redb"],
    ):
        args = parser.parse_args(argv)
        assert args.command == argv[0]
        assert callable(args.func)


def test_migrate_parser_captures_direction_and_flags() -> None:
    parser = build_parser()
    args = parser.parse_args(["migrate", "down", "db.redb", "--no-backup"])
    assert args.direction == "down"
    assert args.db == "db.redb"
    assert args.no_backup is True
    assert args.force is False


def test_migrate_parser_rejects_bad_direction() -> None:
    parser = build_parser()
    with pytest.raises(SystemExit):
        parser.parse_args(["migrate", "sideways", "db.redb"])


def test_main_migrate_returns_nonzero_on_missing_db(tmp_path) -> None:  # type: ignore[no-untyped-def]
    rc = main(["migrate", "up", str(tmp_path / "does-not-exist.redb")])
    assert rc == 1


def test_parser_requires_a_subcommand() -> None:
    parser = build_parser()
    with pytest.raises(SystemExit):
        parser.parse_args([])


def test_parser_rejects_missing_positional() -> None:
    parser = build_parser()
    with pytest.raises(SystemExit):
        parser.parse_args(["dump", "only-one-arg"])


def test_main_dump_returns_nonzero_on_missing_db(tmp_path) -> None:  # type: ignore[no-untyped-def]
    out = tmp_path / "out.graphml"
    rc = main(["dump", str(tmp_path / "does-not-exist.redb"), str(out)])
    assert rc == 1
    assert not out.exists()


def test_main_restore_returns_nonzero_on_missing_graphml(tmp_path) -> None:  # type: ignore[no-untyped-def]
    rc = main(["restore", str(tmp_path / "missing.graphml"), str(tmp_path / "db.redb")])
    assert rc == 1


def test_main_shrink_refuses_same_source_and_target(tmp_path) -> None:  # type: ignore[no-untyped-def]
    # Create a real (empty) db so the "source exists" check passes and we reach
    # the same-path guard.
    import drevo

    db_path = tmp_path / "graph.redb"
    with drevo.Drevo.open(str(db_path)):
        pass
    rc = main(["shrink", str(db_path), str(db_path)])
    assert rc == 1


def test_main_shrink_refuses_existing_target(tmp_path) -> None:  # type: ignore[no-untyped-def]
    import drevo

    src = tmp_path / "src.redb"
    with drevo.Drevo.open(str(src)):
        pass
    dst = tmp_path / "dst.redb"
    dst.write_text("already here")
    rc = main(["shrink", str(src), str(dst)])
    assert rc == 1
