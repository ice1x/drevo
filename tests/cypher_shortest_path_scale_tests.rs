//! `shortestPath` / `allShortestPaths` at scale — issue #546.
//!
//! On the live knowledge graph (3.4k nodes) a single
//! `MATCH p = shortestPath((a)-[*..4]-(b)) WHERE a.name = … RETURN …` never
//! finished: the search enumerated every *trail* up to the bound (no visited
//! set, `O(deg^depth)`), once per `(source, target)` pair — so each
//! unreachable target paid a full depth-4 enumeration of the source's
//! component.
//!
//! These tests lock both halves of the fix:
//!
//! - **scale:** a dense component plus many unreachable targets answers in
//!   well under the timeout (the search is one breadth-first pass per source,
//!   shared by all of its targets);
//! - **exactness:** on seeded random multigraphs (parallel edges, self-loops,
//!   two relationship types) every pair's `length(p)` equals an independent
//!   BFS distance, and `allShortestPaths` yields exactly as many rows as there
//!   are distinct minimum-length paths (counted by a path-count DP) — for
//!   directed, undirected and type-filtered patterns.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use drevo::cypher::executor::{execute_on_engine as execute, Value};
use drevo::cypher::parser::parse;
use drevo::native::NativeGraph;

fn run(source: &str, drevo: &NativeGraph) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new())
        .unwrap_or_else(|e| panic!("execute `{source}`: {e:?}"))
        .rows
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Integer(n) => *n,
        other => panic!("expected Integer, got {other:?}"),
    }
}

/// Deterministic xorshift, so every run builds the same graphs.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// An edge of the reference model: `(from, to, type)`.
type ModelEdge = (usize, usize, &'static str);

/// Build `n` nodes `(:N {i})` plus `edges` in one `NativeGraph`.
fn build(n: usize, edges: &[ModelEdge]) -> NativeGraph {
    let db = NativeGraph::new();
    run(
        &format!("UNWIND range(0, {}) AS i CREATE (:N {{i: i}})", n - 1),
        &db,
    );
    for (from, to, ty) in edges {
        run(
            &format!("MATCH (a:N {{i: {from}}}), (b:N {{i: {to}}}) CREATE (a)-[:{ty}]->(b)"),
            &db,
        );
    }
    db
}

/// Reference: for every source, BFS distance (≤ `upper`) and the number of
/// distinct minimum-length paths to each reachable node. Parallel edges are
/// distinct paths, as they are distinct relationship sequences in Cypher.
fn reference(
    n: usize,
    edges: &[ModelEdge],
    undirected: bool,
    ty: Option<&str>,
    upper: usize,
) -> BTreeMap<(usize, usize), (usize, u64)> {
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (from, to, t) in edges {
        if ty.is_some_and(|want| want != *t) {
            continue;
        }
        adj[*from].push(*to);
        if undirected && from != to {
            adj[*to].push(*from);
        }
    }
    let mut out = BTreeMap::new();
    for s in 0..n {
        let mut dist = vec![usize::MAX; n];
        let mut count = vec![0u64; n];
        dist[s] = 0;
        count[s] = 1;
        let mut queue = VecDeque::from([s]);
        while let Some(u) = queue.pop_front() {
            if dist[u] == upper {
                continue;
            }
            for &v in &adj[u] {
                if dist[v] == usize::MAX {
                    dist[v] = dist[u] + 1;
                    queue.push_back(v);
                }
                if dist[v] == dist[u] + 1 {
                    count[v] += count[u];
                }
            }
        }
        for t in 0..n {
            if t != s && dist[t] != usize::MAX {
                out.insert((s, t), (dist[t], count[t]));
            }
        }
    }
    out
}

fn random_edges(rng: &mut Rng, n: usize, m: usize) -> Vec<ModelEdge> {
    (0..m)
        .map(|_| {
            let ty = if rng.below(3) == 0 { "S" } else { "R" };
            (rng.below(n), rng.below(n), ty)
        })
        .collect()
}

/// Every pair's `shortestPath` length and `allShortestPaths` row count agree
/// with the reference model.
fn assert_matches_reference(arrow: &str, undirected: bool, ty: Option<&str>) {
    let mut rng = Rng(0x5EED_0546);
    for round in 0..12 {
        let n = 9;
        let edges = random_edges(&mut rng, n, 14);
        let db = build(n, &edges);
        let expected = reference(n, &edges, undirected, ty, 4);

        let rel = match ty {
            Some(t) => format!(":{t}*..4"),
            None => "*..4".to_string(),
        };
        let pattern = arrow.replace("REL", &rel);

        let single = run(
            &format!(
                "MATCH p = shortestPath((a:N){pattern}(b:N)) WHERE a.i <> b.i \
                 RETURN a.i AS a, b.i AS b, length(p) AS l"
            ),
            &db,
        );
        let got: BTreeMap<(usize, usize), usize> = single
            .iter()
            .map(|r| {
                (
                    (int(&r[0]) as usize, int(&r[1]) as usize),
                    int(&r[2]) as usize,
                )
            })
            .collect();
        assert_eq!(single.len(), got.len(), "one row per pair (round {round})");
        let want: BTreeMap<(usize, usize), usize> =
            expected.iter().map(|(k, (d, _))| (*k, *d)).collect();
        assert_eq!(
            got, want,
            "shortestPath lengths, {pattern}, round {round}: {edges:?}"
        );

        let all = run(
            &format!(
                "MATCH p = allShortestPaths((a:N){pattern}(b:N)) WHERE a.i <> b.i \
                 RETURN a.i AS a, b.i AS b, count(p) AS c, min(length(p)) AS l"
            ),
            &db,
        );
        let got: BTreeMap<(usize, usize), (usize, u64)> = all
            .iter()
            .map(|r| {
                (
                    (int(&r[0]) as usize, int(&r[1]) as usize),
                    (int(&r[3]) as usize, int(&r[2]) as u64),
                )
            })
            .collect();
        assert_eq!(
            got, expected,
            "allShortestPaths, {pattern}, round {round}: {edges:?}"
        );
    }
}

#[test]
fn directed_shortest_paths_match_the_reference() {
    assert_matches_reference("-[REL]->", false, None);
}

#[test]
fn undirected_shortest_paths_match_the_reference() {
    assert_matches_reference("-[REL]-", true, None);
}

#[test]
fn type_filtered_shortest_paths_match_the_reference() {
    assert_matches_reference("-[REL]->", false, Some("R"));
}

#[test]
fn incoming_shortest_paths_match_the_reversed_reference() {
    // `(a)<-[*]-(b)` from a is `(b)-[*]->(a)`: compare against the directed
    // reference with the pair swapped.
    let mut rng = Rng(0xBAC_0546);
    for _ in 0..8 {
        let n = 9;
        let edges = random_edges(&mut rng, n, 14);
        let db = build(n, &edges);
        let expected = reference(n, &edges, false, None, 4);
        let rows = run(
            "MATCH p = shortestPath((a:N)<-[*..4]-(b:N)) WHERE a.i <> b.i \
             RETURN a.i AS a, b.i AS b, length(p) AS l",
            &db,
        );
        let got: BTreeMap<(usize, usize), usize> = rows
            .iter()
            .map(|r| {
                (
                    (int(&r[1]) as usize, int(&r[0]) as usize),
                    int(&r[2]) as usize,
                )
            })
            .collect();
        let want: BTreeMap<(usize, usize), usize> =
            expected.iter().map(|(k, (d, _))| (*k, *d)).collect();
        assert_eq!(got, want, "{edges:?}");
    }
}

/// The live-incident shape: one source inside a dense component, targets
/// that are mostly unreachable. Before #546 each unreachable target paid a
/// full depth-4 trail enumeration of the component; now it is one BFS.
#[test]
fn many_unreachable_targets_do_not_blow_up() {
    let dense = 120;
    let isolated = 300;
    let mut rng = Rng(0xD3_0546);
    let mut edges: Vec<ModelEdge> = Vec::new();
    for u in 0..dense {
        for _ in 0..8 {
            edges.push((u, rng.below(dense), "R"));
        }
    }
    let db = build(dense + isolated, &edges);
    let expected = reference(dense + isolated, &edges, true, None, 4)
        .keys()
        .filter(|(s, _)| *s == 0)
        .count() as i64;

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let rows = run(
            "MATCH p = shortestPath((a:N {i: 0})-[*..4]-(b:N)) WHERE b.i <> 0 \
             RETURN count(p) AS c",
            &db,
        );
        let _ = tx.send(int(&rows[0][0]));
    });
    let got = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("shortestPath over 420 nodes must finish (#546)");
    assert_eq!(got, expected);
}
