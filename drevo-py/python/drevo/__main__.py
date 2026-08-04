"""Command-line interface for drevo backups and maintenance.

Run as ``python -m drevo <command>`` or, once installed, ``drevo <command>``.

Commands (all safety-first — ``dump`` only reads; ``restore``/``shrink`` never
overwrite their source):

* ``dump <db> <out.graphml>``   — write a faithful, portable GraphML backup.
* ``restore <in.graphml> <db>`` — import a GraphML dump into ``db`` (created if
  absent); prints an import report.
* ``compact <db>``              — reclaim redb free pages in place.
* ``shrink <db> <out_db>``      — dump ``db`` and re-import into a fresh
  ``out_db`` (the robust way to shed copy-on-write high-water-mark bloat).

The heavy lifting lives in Rust (``drevo.Drevo.export_graphml`` /
``import_graphml`` / ``compact``); this module is just the argument-parsing and
reporting "handles".
"""

from __future__ import annotations

import argparse
import os
import sys
import tempfile
from collections.abc import Sequence

from . import Drevo


def _human_bytes(n: int | None) -> str:
    """Render a byte count as a compact human string (``None`` -> ``"?"``)."""
    if n is None:
        return "?"
    size = float(n)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if size < 1024.0 or unit == "TiB":
            return f"{size:.1f} {unit}" if unit != "B" else f"{int(size)} B"
        size /= 1024.0
    return f"{n} B"


def _cmd_dump(args: argparse.Namespace) -> int:
    src: str = args.db
    out: str = args.out
    if not os.path.exists(src):
        print(f"error: database not found: {src}", file=sys.stderr)
        return 1
    with Drevo.open(src) as db:
        db.export_graphml_to_path(out)
    size = os.path.getsize(out) if os.path.exists(out) else None
    print(f"wrote GraphML backup: {out} ({_human_bytes(size)})")
    return 0


def _cmd_restore(args: argparse.Namespace) -> int:
    graphml: str = args.graphml
    dst: str = args.db
    if not os.path.exists(graphml):
        print(f"error: GraphML file not found: {graphml}", file=sys.stderr)
        return 1
    with Drevo.open(dst) as db:
        report = db.import_graphml_from_path(graphml)
    print(
        f"restored into {dst}: "
        f"{report.nodes_imported} nodes, {report.edges_imported} edges imported "
        f"({report.nodes_skipped} nodes / {report.edges_skipped} edges skipped)"
    )
    return 0


def _cmd_compact(args: argparse.Namespace) -> int:
    db_path: str = args.db
    if not os.path.exists(db_path):
        print(f"error: database not found: {db_path}", file=sys.stderr)
        return 1
    with Drevo.open(db_path) as db:
        report = db.compact()
    print(
        f"compacted {db_path}: "
        f"{_human_bytes(report.bytes_before)} -> {_human_bytes(report.bytes_after)} "
        f"(reclaimed {_human_bytes(report.bytes_reclaimed)})"
    )
    return 0


def _cmd_shrink(args: argparse.Namespace) -> int:
    src: str = args.db
    dst: str = args.out_db
    if not os.path.exists(src):
        print(f"error: database not found: {src}", file=sys.stderr)
        return 1
    if os.path.abspath(src) == os.path.abspath(dst):
        print(
            "error: <out_db> must differ from <db> (shrink never overwrites the source)",
            file=sys.stderr,
        )
        return 1
    if os.path.exists(dst):
        print(f"error: refusing to overwrite existing file: {dst}", file=sys.stderr)
        return 1

    before = os.path.getsize(src)
    # Dump the source to a temporary GraphML, then import into a fresh DB.
    fd, tmp = tempfile.mkstemp(suffix=".graphml")
    os.close(fd)
    try:
        with Drevo.open(src) as db:
            db.export_graphml_to_path(tmp)
        with Drevo.open(dst) as fresh:
            report = fresh.import_graphml_from_path(tmp)
    finally:
        try:
            os.remove(tmp)
        except OSError:
            pass

    after = os.path.getsize(dst) if os.path.exists(dst) else None
    print(
        f"shrank {src} -> {dst}: "
        f"{_human_bytes(before)} -> {_human_bytes(after)} "
        f"({report.nodes_imported} nodes, {report.edges_imported} edges)"
    )
    return 0


def build_parser() -> argparse.ArgumentParser:
    """Construct the ``drevo`` argument parser."""
    parser = argparse.ArgumentParser(
        prog="drevo",
        description="drevo backups and maintenance (GraphML dump/restore, compact, shrink).",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_dump = sub.add_parser("dump", help="write a GraphML backup of a database")
    p_dump.add_argument("db", help="path to the drevo database to back up")
    p_dump.add_argument("out", help="output GraphML file path")
    p_dump.set_defaults(func=_cmd_dump)

    p_restore = sub.add_parser("restore", help="import a GraphML dump into a database")
    p_restore.add_argument("graphml", help="input GraphML file path")
    p_restore.add_argument("db", help="target database path (created if absent)")
    p_restore.set_defaults(func=_cmd_restore)

    p_compact = sub.add_parser("compact", help="reclaim free pages in place")
    p_compact.add_argument("db", help="path to the drevo database to compact")
    p_compact.set_defaults(func=_cmd_compact)

    p_shrink = sub.add_parser("shrink", help="dump + re-import into a fresh database to shed bloat")
    p_shrink.add_argument("db", help="source database path (never modified)")
    p_shrink.add_argument("out_db", help="fresh output database path (must not exist)")
    p_shrink.set_defaults(func=_cmd_shrink)

    return parser


def main(argv: Sequence[str] | None = None) -> int:
    """Entry point. Returns a process exit code (0 = success)."""
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        result: int = args.func(args)
        return result
    except Exception as exc:  # noqa: BLE001 — CLI boundary: report, don't traceback
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
