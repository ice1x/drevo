#!/usr/bin/env python3
"""Cross-database baseline: drevo vs Memgraph, same GraphML, same Cypher, same
Bolt client (`docs/native-core-baseline.md`, RFC #307 Phase 0).

Both databases are measured through the SAME code path — this script, the
`neo4j` Python driver, localhost Bolt — so client/transport overhead cancels
out. The drevo side is a locally built `drevo-server` (today's production KV
path over Bolt); the Memgraph side is the official `memgraph/memgraph` Docker
image with a label index and a label+property index created for its densest
label, mirroring the native drevo indexes the in-process baseline uses.

Usage:
    python scripts/memgraph_baseline_bench.py \
        --graphml ~/drevo_backups/<snapshot>.graphml \
        --drevo bolt://127.0.0.1:7690 \
        --memgraph bolt://127.0.0.1:7687 \
        --load-memgraph --iters 30

The script derives the workload parameters (densest kind, mid-selectivity
property pair, highest-out-degree hub) from the data itself, exactly like
`benches/real_data_baseline_bench.rs`, asserts row parity between the two
databases for every measured query, and prints a Markdown results table.
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
import time
import xml.etree.ElementTree as ET
from collections import defaultdict

from neo4j import GraphDatabase

NS = "{http://graphml.graphdrawing.org/xmlns}"


def parse_graphml(path: str):
    """Parse a drevo GraphML export into plain node/edge dicts."""
    key_names: dict[str, str] = {}
    nodes: list[dict] = []
    edges: list[dict] = []
    for _event, el in ET.iterparse(path, events=("end",)):
        tag = el.tag.removeprefix(NS)
        if tag == "key":
            key_names[el.get("id")] = el.get("attr.name")
        elif tag == "node":
            data = {
                key_names.get(d.get("key"), d.get("key")): d.text or ""
                for d in el.findall(f"{NS}data")
            }
            nodes.append({"gid": el.get("id"), **data})
            el.clear()
        elif tag == "edge":
            data = {
                key_names.get(d.get("key"), d.get("key")): d.text or ""
                for d in el.findall(f"{NS}data")
            }
            edges.append(
                {"source": el.get("source"), "target": el.get("target"), **data}
            )
            el.clear()
    return nodes, edges


def scalar_props(raw: str):
    """Flatten a drevo `properties` JSON string into Bolt-storable values.

    Returns `(props, labels)` where `labels` is the `_labels` secondary-label
    list. Scalars and lists of scalars are kept; nested objects are dropped
    (none of the measured queries touch them).
    """
    props: dict = {}
    labels: list[str] = []
    if not raw:
        return props, labels
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError:
        return props, labels
    if not isinstance(parsed, dict):
        return props, labels
    for k, v in parsed.items():
        if k == "_labels":
            if isinstance(v, list):
                labels = [s for s in v if isinstance(s, str)]
            continue
        if isinstance(v, (str, int, float, bool)):
            props[k] = v
        elif isinstance(v, list) and all(
            isinstance(x, (str, int, float, bool)) for x in v
        ):
            props[k] = v
    return props, labels


def esc(ident: str) -> str:
    """Backtick-escape a label / relationship-type identifier."""
    return "`" + ident.replace("`", "``") + "`"


def load_memgraph(driver, nodes, edges):
    """Wipe Memgraph and load the parsed graph, batched by label set / kind."""
    by_labels: dict[tuple, list[dict]] = defaultdict(list)
    for n in nodes:
        props, extra = scalar_props(n.get("properties", ""))
        row = {
            "title": n.get("title", ""),
            "body": n.get("body", ""),
            "uuid": n.get("uuid", ""),
            **props,
        }
        for key in ("created_at", "updated_at"):
            if n.get(key):
                row[key] = int(n[key])
        label_set = tuple(sorted({n.get("kind", "Node"), *extra}))
        by_labels[label_set].append({"gid": n["gid"], "props": row})

    with driver.session() as s:
        s.run("MATCH (n) DETACH DELETE n").consume()
        gid_to_internal: dict[str, int] = {}
        for label_set, rows in by_labels.items():
            labels = "".join(f":{esc(l)}" for l in label_set)
            for i in range(0, len(rows), 200):
                batch = rows[i : i + 200]
                for rec in s.run(
                    f"UNWIND $rows AS row CREATE (n{labels}) SET n = row.props "
                    "RETURN row.gid AS gid, id(n) AS iid",
                    rows=batch,
                ):
                    gid_to_internal[rec["gid"]] = rec["iid"]

        by_kind: dict[str, list[dict]] = defaultdict(list)
        for e in edges:
            props, _ = scalar_props(e.get("properties", ""))
            if e.get("weight"):
                props["weight"] = float(e["weight"])
            by_kind[e.get("kind", "RELATED")].append(
                {
                    "a": gid_to_internal[e["source"]],
                    "b": gid_to_internal[e["target"]],
                    "props": props,
                }
            )
        for kind, rows in by_kind.items():
            for i in range(0, len(rows), 500):
                s.run(
                    "UNWIND $rows AS row "
                    "MATCH (a) WHERE id(a) = row.a "
                    "MATCH (b) WHERE id(b) = row.b "
                    f"CREATE (a)-[r:{esc(kind)}]->(b) SET r = row.props",
                    rows=rows[i : i + 500],
                ).consume()
        total = s.run("MATCH (n) RETURN count(n) AS c").single()["c"]
        print(f"memgraph loaded: {total} nodes, {len(edges)} edges", file=sys.stderr)


def derive_params(nodes, edges):
    """Densest kind, mid-selectivity (key, value) pair, highest-out-degree hub —
    the same derivation as `benches/real_data_baseline_bench.rs`."""
    kind_freq: dict[str, int] = defaultdict(int)
    prop_freq: dict[tuple, int] = defaultdict(int)
    for n in nodes:
        kind_freq[n.get("kind", "Node")] += 1
        props, _ = scalar_props(n.get("properties", ""))
        for k, v in props.items():
            if isinstance(v, str):
                prop_freq[(k, v)] += 1
    top_kind = max(kind_freq, key=kind_freq.get)
    target = max(len(nodes) // 100, 2)
    candidates = sorted(
        (pair for pair, c in prop_freq.items() if c >= 2),
        key=lambda pair: (abs(prop_freq[pair] - target), pair),
    )
    prop_pair = candidates[0] if candidates else None

    out_deg: dict[str, int] = defaultdict(int)
    for e in edges:
        out_deg[e["source"]] += 1
    hub = nodes[0]
    hub_deg = -1
    for n in sorted(nodes, key=lambda n: int(n["gid"].lstrip("n"))):
        if out_deg[n["gid"]] > hub_deg:
            hub_deg = out_deg[n["gid"]]
            hub = n
    print(
        f"workload params: top_kind={top_kind!r} ({kind_freq[top_kind]} nodes), "
        f"prop_pair={prop_pair!r}, hub title={hub['title']!r} "
        f"(out-degree {hub_deg})",
        file=sys.stderr,
    )
    return top_kind, prop_pair, hub["title"]


def internal_id_by_title(driver, title: str) -> int:
    with driver.session() as s:
        rec = s.run(
            "MATCH (n {title: $t}) RETURN id(n) AS iid", t=title
        ).single()
        if rec is None:
            raise SystemExit(f"hub node {title!r} not found")
        return rec["iid"]


def bench(driver, query: str, iters: int):
    """Run `query` `iters` times (3 warmups), return (first row value, median s)."""
    with driver.session() as s:
        value = s.run(query).single()[0]
        for _ in range(2):
            s.run(query).single()
        times = []
        for _ in range(iters):
            t0 = time.perf_counter()
            s.run(query).single()
            times.append(time.perf_counter() - t0)
    return value, statistics.median(times)


def fmt(seconds: float) -> str:
    if seconds < 1e-3:
        return f"{seconds * 1e6:.1f} µs"
    if seconds < 1.0:
        return f"{seconds * 1e3:.2f} ms"
    return f"{seconds:.3f} s"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--graphml", required=True)
    ap.add_argument("--drevo", default="bolt://127.0.0.1:7690")
    ap.add_argument("--memgraph", default="bolt://127.0.0.1:7687")
    ap.add_argument("--iters", type=int, default=30)
    ap.add_argument(
        "--load-memgraph",
        action="store_true",
        help="wipe Memgraph and load the GraphML before benchmarking",
    )
    args = ap.parse_args()

    nodes, edges = parse_graphml(args.graphml)
    print(f"parsed {len(nodes)} nodes, {len(edges)} edges", file=sys.stderr)
    top_kind, prop_pair, hub_title = derive_params(nodes, edges)

    mg = GraphDatabase.driver(args.memgraph, auth=("", ""))
    dv = GraphDatabase.driver(args.drevo, auth=("", ""))

    if args.load_memgraph:
        load_memgraph(mg, nodes, edges)
        with mg.session() as s:
            # Mirror the native drevo indexes: label index + label+property
            # index on the densest label.
            s.run(f"CREATE INDEX ON :{esc(top_kind)}").consume()
            if prop_pair:
                s.run(f"CREATE INDEX ON :{esc(top_kind)}({esc(prop_pair[0])})").consume()

    hub_mg = internal_id_by_title(mg, hub_title)
    hub_dv = internal_id_by_title(dv, hub_title)

    workloads = [("bolt_roundtrip", "RETURN 1", "RETURN 1")]

    def both(name: str, q: str):
        workloads.append((name, q, q))

    both("count_all_nodes", "MATCH (n) RETURN count(*)")
    both("label_scan_count", f"MATCH (n:{esc(top_kind)}) RETURN count(*)")
    if prop_pair:
        k, v = prop_pair
        lit = v.replace("\\", "\\\\").replace("'", "\\'")
        both("property_equality_count", f"MATCH (n {{{k}: '{lit}'}}) RETURN count(*)")
        both(
            "labelled_property_equality",
            f"MATCH (n:{esc(top_kind)} {{{k}: '{lit}'}}) RETURN count(*)",
        )
    workloads.append(
        (
            "one_hop_from_hub_cypher",
            f"MATCH (a)-->(b) WHERE id(a) = {hub_dv} RETURN count(b)",
            f"MATCH (a)-->(b) WHERE id(a) = {hub_mg} RETURN count(b)",
        )
    )

    print("\n| Workload | drevo (Bolt) | Memgraph (Bolt) |")
    print("|---|---:|---:|")
    for name, q_dv, q_mg in workloads:
        val_dv, med_dv = bench(dv, q_dv, args.iters)
        val_mg, med_mg = bench(mg, q_mg, args.iters)
        assert val_dv == val_mg, (
            f"row parity broken on {name}: drevo={val_dv!r} memgraph={val_mg!r}"
        )
        print(f"| {name} (rows agree: {val_dv}) | {fmt(med_dv)} | {fmt(med_mg)} |")

    mg.close()
    dv.close()


if __name__ == "__main__":
    main()
