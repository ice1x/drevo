"""Command-line entry point for the Neo4j → drevo migration tool.

Three single-purpose phases, each its own command:

    # 1. dump a live Neo4j to a local JSON-Lines file (via Bolt, no APOC)
    python -m neo4j_to_drevo dump \\
        --neo4j-uri bolt://localhost:7687 --neo4j-user neo4j --out graph.json

    # 2. dry run — read + map + validate, report what WOULD happen, write nothing
    python -m neo4j_to_drevo import --apoc-json graph.json --drevo-path graph.redb --dry-run

    # 3. import for real
    python -m neo4j_to_drevo import --apoc-json graph.json --drevo-path graph.redb

`import` reads from either an offline APOC dump (`--apoc-json`) or a live
Bolt connection (`--neo4j-uri`). The Bolt password may also come from the
`DREVO_NEO4J_PASSWORD` environment variable.

`main()` accepts injectable `source_factory` / `db_factory` so every phase
is unit-testable without a real Neo4j or disk file.
"""

from __future__ import annotations

import argparse
import os
import sys
from typing import Any, Callable

from ._engine import MigrationConfig, migrate


def _add_credential_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--neo4j-user", default=None, help="Neo4j username (with --neo4j-uri)")
    parser.add_argument(
        "--neo4j-password",
        default=None,
        help="Neo4j password (falls back to $DREVO_NEO4J_PASSWORD)",
    )
    parser.add_argument("--neo4j-database", default=None, help="Source database name (optional)")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="python -m neo4j_to_drevo",
        description="Migrate a Neo4j graph into a drevo database, in three phases.",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    # ── phase 1: dump ────────────────────────────────────────────────
    dump = sub.add_parser("dump", help="dump a live Neo4j (Bolt) to a local JSON-Lines file")
    dump.add_argument("--neo4j-uri", required=True, help="Bolt URI of the source database")
    _add_credential_args(dump)
    dump.add_argument("--out", required=True, help="Destination dump file (JSON Lines)")

    # ── phases 2 & 3: import (+ --dry-run) ───────────────────────────
    imp = sub.add_parser("import", help="import a dump or live Neo4j into drevo")
    source = imp.add_mutually_exclusive_group(required=True)
    source.add_argument("--apoc-json", default=None, help="Path to an apoc.export.json.all dump")
    source.add_argument("--neo4j-uri", default=None, help="Bolt URI, e.g. bolt://localhost:7687")
    _add_credential_args(imp)

    target = imp.add_mutually_exclusive_group()
    target.add_argument("--drevo-path", default=None, help="Destination drevo database path")
    target.add_argument(
        "--in-memory", action="store_true", help="Import into an ephemeral in-memory drevo"
    )
    imp.add_argument(
        "--dry-run",
        action="store_true",
        help="Read + map + validate and report; write nothing to the target",
    )
    imp.add_argument("--default-kind", default="node", help="kind for label-less nodes")
    imp.add_argument(
        "--on-error",
        choices=("raise", "skip"),
        default="raise",
        help="dangling-edge policy: fail loud (raise) or continue (skip)",
    )
    return parser


# ── source / target construction (overridable in tests) ──────────────


def _live_source(ns: argparse.Namespace) -> Any:
    from .bolt import Neo4jSource

    if not ns.neo4j_user:
        raise ValueError("--neo4j-user is required when connecting to a live --neo4j-uri")
    password = ns.neo4j_password or os.environ.get("DREVO_NEO4J_PASSWORD", "")
    return Neo4jSource.connect(ns.neo4j_uri, ns.neo4j_user, password, database=ns.neo4j_database)


def _default_source_factory(ns: argparse.Namespace) -> Any:
    if getattr(ns, "apoc_json", None) is not None:
        from .apoc import ApocJsonSource

        return ApocJsonSource(ns.apoc_json)
    return _live_source(ns)


def _default_db_factory(ns: argparse.Namespace) -> Any:
    import drevo

    if ns.in_memory:
        return drevo.Drevo.open_in_memory()
    return drevo.Drevo.open(ns.drevo_path)


# ── phase runners ────────────────────────────────────────────────────


def _close(obj: Any) -> None:
    close = getattr(obj, "close", None)
    if callable(close):
        close()


def _run_dump(ns: argparse.Namespace, source_factory: Callable[[argparse.Namespace], Any]) -> int:
    from .apoc import write_apoc_json

    source = None
    try:
        source = source_factory(ns)
        nodes, edges = write_apoc_json(source, ns.out)
        print(f"dump complete: wrote {nodes} nodes + {edges} relationships to {ns.out}")
        return 0
    except Exception as exc:  # noqa: BLE001 - top-level CLI guard
        print(f"dump failed: {exc}", file=sys.stderr)
        return 1
    finally:
        _close(source)


def _run_import(
    ns: argparse.Namespace,
    source_factory: Callable[[argparse.Namespace], Any],
    db_factory: Callable[[argparse.Namespace], Any],
) -> int:
    if not ns.dry_run and not ns.in_memory and ns.drevo_path is None:
        print("import failed: a target is required (--drevo-path or --in-memory)", file=sys.stderr)
        return 1

    config = MigrationConfig(default_kind=ns.default_kind, on_error=ns.on_error)
    source = None
    try:
        source = source_factory(ns)
        if ns.dry_run:
            import drevo

            scratch = drevo.Drevo.open_in_memory()  # throwaway: validates, never persisted
            try:
                report = migrate(source, scratch, config=config)
            finally:
                scratch.close()
            target = "in-memory" if ns.in_memory else (ns.drevo_path or "<unset>")
            print(f"[dry run] would import: {report.summary()}")
            print(f"[dry run] nothing written to {target}")
            if report.errors:
                print("\n".join(f"  - {e}" for e in report.errors), file=sys.stderr)
            return 0

        db = db_factory(ns)
        report = migrate(source, db, config=config)
        print(f"migration complete: {report.summary()}")
        if report.errors:
            print("\n".join(f"  - {e}" for e in report.errors), file=sys.stderr)
        return 0
    except Exception as exc:  # noqa: BLE001 - top-level CLI guard
        print(f"migration failed: {exc}", file=sys.stderr)
        return 1
    finally:
        _close(source)


def main(
    argv: list[str] | None = None,
    *,
    source_factory: Callable[[argparse.Namespace], Any] = _default_source_factory,
    db_factory: Callable[[argparse.Namespace], Any] = _default_db_factory,
) -> int:
    """Parse `argv`, dispatch to the chosen phase, return an exit code."""
    ns = build_parser().parse_args(argv)
    if ns.command == "dump":
        return _run_dump(ns, source_factory)
    return _run_import(ns, source_factory, db_factory)
