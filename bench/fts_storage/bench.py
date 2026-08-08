#!/usr/bin/env python3
"""FTS-storage benchmark harness (#275 follow-up).

Measures a single installed `drevo` build across the dimensions that the
posting-list FTS rewrite trades off — on-disk size vs. write throughput — plus
search latency. Version-independent: it uses only the stable drevo-py API
(`create_node(s)`, `import_graphml_from_path`, `search_fts`), so the SAME script
runs against every drevo build; the caller labels which build produced each run.

Emits one JSON object to stdout. See README.md for methodology + results.

Usage:
    python bench.py --label per-pair --graphml ~/drevo_backups/<kg>.graphml \
                    --workdir /tmp/drevo_bench
"""
from __future__ import annotations

import argparse
import json
import os
import statistics
import sys
import time

import drevo

# Shared vocabulary → every node contributes the SAME trigrams, so all posting
# lists collide. This is the worst case for posting-list read-modify-write: the
# Nth insert touches a list of size ~N. Deliberately probes the write
# amplification the per-pair format did not have.
_SHARED = "anxious deadlines mentoring graph vectors embeddings semantic search relationships"


def _body(i: int) -> str:
    # A little per-node uniqueness (the id) on top of the shared vocabulary.
    return f"note {i} {_SHARED}"


def bench_incremental_write(n: int) -> float:
    """Nodes/sec creating N nodes ONE AT A TIME (in-memory, no fsync — isolates
    the indexing algorithm cost). This is where posting-list RMW regresses vs.
    the per-pair format's independent puts."""
    db = drevo.Drevo.open_in_memory()
    t0 = time.perf_counter()
    for i in range(n):
        db.create_node(drevo.NewNode(kind="n", title=f"t{i}", body=_body(i)))
    dt = time.perf_counter() - t0
    return n / dt


def bench_batch_write(n: int) -> float:
    """Nodes/sec via a single create_nodes() call (grouped indexing — the fast
    path). In-memory."""
    db = drevo.Drevo.open_in_memory()
    nodes = [drevo.NewNode(kind="n", title=f"t{i}", body=_body(i)) for i in range(n)]
    t0 = time.perf_counter()
    db.create_nodes(nodes)
    dt = time.perf_counter() - t0
    return n / dt


def bench_import_and_size(graphml: str, db_path: str) -> tuple[float, int, int, int]:
    """Import a real GraphML backup into a fresh on-disk DB. Returns
    (import_seconds, file_bytes, nodes, edges) — import time is the bulk-write
    throughput signal; file size is the storage signal."""
    if os.path.exists(db_path):
        os.remove(db_path)
    db = drevo.Drevo.open(db_path)
    t0 = time.perf_counter()
    rep = db.import_graphml_from_path(graphml)
    dt = time.perf_counter() - t0
    db.close()
    size = os.path.getsize(db_path)
    return dt, size, rep.nodes_imported, rep.edges_imported


def bench_search(db_path: str, queries: list[str], reps: int = 20) -> float:
    """Median search_fts latency in milliseconds over `queries` x `reps`."""
    db = drevo.Drevo.open(db_path)
    samples: list[float] = []
    for _ in range(reps):
        for q in queries:
            t0 = time.perf_counter()
            db.search_fts(q, 10)
            samples.append((time.perf_counter() - t0) * 1000.0)
    return statistics.median(samples)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", required=True, help="drevo build label (e.g. per-pair)")
    ap.add_argument("--graphml", required=True, help="real KG GraphML backup")
    ap.add_argument("--workdir", default="/tmp/drevo_bench")
    ap.add_argument("--incr-n", type=int, default=2000)
    ap.add_argument("--batch-n", type=int, default=2000)
    args = ap.parse_args(argv)
    os.makedirs(args.workdir, exist_ok=True)
    db_path = os.path.join(args.workdir, f"{args.label}.redb")

    version = getattr(drevo, "__version__", "?")
    import_s, size, nodes, edges = bench_import_and_size(args.graphml, db_path)
    result = {
        "label": args.label,
        "drevo_version": version,
        "graphml": os.path.basename(args.graphml),
        "nodes": nodes,
        "edges": edges,
        "file_bytes": size,
        "file_mib": round(size / (1024 * 1024), 1),
        "import_seconds": round(import_s, 2),
        "import_nodes_per_sec": round(nodes / import_s, 1) if import_s else None,
        "incr_write_nodes_per_sec": round(bench_incremental_write(args.incr_n), 1),
        "batch_write_nodes_per_sec": round(bench_batch_write(args.batch_n), 1),
        "search_median_ms": round(bench_search(db_path, ["error", "graph", "test", "the"]), 3),
    }
    json.dump(result, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
