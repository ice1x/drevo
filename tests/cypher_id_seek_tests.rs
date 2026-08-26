//! `id()`-seek pushdown (RFC `docs/rfc-native-core.md`, #307 — the executor
//! gap surfaced by the real-data baseline): a conjunctive `WHERE id(n) = X` /
//! `WHERE id(n) IN [...]` must resolve the pattern variable through
//! [`GraphEngine::get_node`] point lookups instead of enumerating every node.
//!
//! The optimisation is observable through the seam: these tests wrap a
//! [`NativeGraph`] in a [`CountingEngine`] that counts `all_nodes` full scans
//! and `get_node` point lookups, so "the scan was skipped" is an assertion,
//! not a timing guess. Correctness stays pinned by running every query's
//! row-level expectations too (and the differential corpus covers KV/native
//! parity for the same shapes).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use drevo::cypher::executor::{execute_on_engine, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::engine::GraphEngine;
use drevo::model::{Direction, Edge, EdgePatch, NewEdge, NewNode, Node, NodePatch};
use drevo::native::NativeGraph;
use drevo_core::dump::{Dump, ImportReport};
use drevo_core::error::Result;

/// A [`GraphEngine`] decorator that counts full scans and point lookups.
struct CountingEngine {
    inner: NativeGraph,
    all_nodes_calls: AtomicUsize,
    get_node_calls: AtomicUsize,
}

impl CountingEngine {
    fn new(inner: NativeGraph) -> Self {
        CountingEngine {
            inner,
            all_nodes_calls: AtomicUsize::new(0),
            get_node_calls: AtomicUsize::new(0),
        }
    }

    fn full_scans(&self) -> usize {
        self.all_nodes_calls.load(Ordering::SeqCst)
    }

    fn point_lookups(&self) -> usize {
        self.get_node_calls.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.all_nodes_calls.store(0, Ordering::SeqCst);
        self.get_node_calls.store(0, Ordering::SeqCst);
    }
}

impl GraphEngine for CountingEngine {
    fn create_node(&self, new_node: NewNode) -> Result<Node> {
        self.inner.create_node(new_node)
    }
    fn get_node(&self, id: u64) -> Result<Option<Arc<Node>>> {
        self.get_node_calls.fetch_add(1, Ordering::SeqCst);
        GraphEngine::get_node(&self.inner, id)
    }
    fn update_node(&self, id: u64, patch: NodePatch) -> Result<Node> {
        self.inner.update_node(id, patch)
    }
    fn delete_node(&self, id: u64) -> Result<()> {
        GraphEngine::delete_node(&self.inner, id)
    }
    fn create_edge(&self, new_edge: NewEdge) -> Result<Edge> {
        self.inner.create_edge(new_edge)
    }
    fn get_edge(&self, id: u64) -> Result<Option<Edge>> {
        GraphEngine::get_edge(&self.inner, id)
    }
    fn update_edge(&self, id: u64, patch: EdgePatch) -> Result<Edge> {
        self.inner.update_edge(id, patch)
    }
    fn delete_edge(&self, id: u64) -> Result<()> {
        GraphEngine::delete_edge(&self.inner, id)
    }
    fn neighbor_ids(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<u64>> {
        GraphEngine::neighbor_ids(&self.inner, node_id, direction, kind)
    }
    fn neighbors(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> Result<Vec<Arc<Node>>> {
        GraphEngine::neighbors(&self.inner, node_id, direction, kind)
    }
    fn edges_of(&self, node_id: u64, direction: Direction) -> Result<Vec<Edge>> {
        GraphEngine::edges_of(&self.inner, node_id, direction)
    }
    fn all_nodes(&self) -> Result<Vec<Arc<Node>>> {
        self.all_nodes_calls.fetch_add(1, Ordering::SeqCst);
        GraphEngine::all_nodes(&self.inner)
    }
    fn all_edges(&self) -> Result<Vec<Edge>> {
        GraphEngine::all_edges(&self.inner)
    }
    fn nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Arc<Node>>> {
        GraphEngine::nodes_by_kind(&self.inner, kind, limit, offset)
    }
    fn export_dump(&self) -> Result<Dump> {
        GraphEngine::export_dump(&self.inner)
    }
    fn apply_dump(&self, dump: Dump) -> Result<ImportReport> {
        GraphEngine::apply_dump(&self.inner, dump)
    }
}

/// 10 `person` nodes (`p01`…`p10`, ids 1..=10) with `p01` linked to every
/// other node (a small hub), behind a counting decorator.
fn hub_graph() -> CountingEngine {
    let g = NativeGraph::new();
    for i in 1..=10u64 {
        g.create_node(NewNode {
            kind: "person".into(),
            title: format!("p{i:02}"),
            body: String::new(),
            body_html: String::new(),
            properties: Default::default(),
        })
        .expect("create node");
    }
    for to in 2..=10u64 {
        g.create_edge(NewEdge {
            from_id: 1,
            to_id: to,
            kind: "KNOWS".into(),
            weight: 1.0,
            properties: Default::default(),
        })
        .expect("create edge");
    }
    let e = CountingEngine::new(g);
    e.reset(); // ignore the build traffic; count only query-time calls
    e
}

fn run(engine: &CountingEngine, source: &str) -> ExecResult {
    let q = parse(source).expect("parse");
    execute_on_engine(&q, engine, HashMap::new()).expect("execute")
}

fn titles(res: &ExecResult) -> Vec<String> {
    res.rows
        .iter()
        .map(|row| match &row[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected string, got {other:?}"),
        })
        .collect()
}

#[test]
fn id_equality_seeks_instead_of_scanning() {
    let e = hub_graph();
    let res = run(&e, "MATCH (n) WHERE id(n) = 5 RETURN n.title");
    assert_eq!(titles(&res), vec!["p05"]);
    assert_eq!(
        e.full_scans(),
        0,
        "id(n) = <int> must resolve via get_node, not a full scan"
    );
    assert!(e.point_lookups() >= 1);
}

#[test]
fn id_membership_seeks_instead_of_scanning() {
    let e = hub_graph();
    let res = run(
        &e,
        "MATCH (n) WHERE id(n) IN [2, 9] RETURN n.title ORDER BY n.title",
    );
    assert_eq!(titles(&res), vec!["p02", "p09"]);
    assert_eq!(e.full_scans(), 0);
}

#[test]
fn hub_expansion_with_id_seek_never_scans() {
    let e = hub_graph();
    let res = run(
        &e,
        "MATCH (a)-[:KNOWS]->(b) WHERE id(a) = 1 RETURN count(b)",
    );
    assert_eq!(res.rows, vec![vec![Value::Integer(9)]]);
    assert_eq!(
        e.full_scans(),
        0,
        "the anchor seeks by id and the hop expands adjacency — no full scan anywhere"
    );
}

#[test]
fn missing_and_negative_ids_yield_empty_without_scanning() {
    let e = hub_graph();
    let res = run(&e, "MATCH (n) WHERE id(n) = 999 RETURN n.title");
    assert!(res.rows.is_empty());
    let res = run(&e, "MATCH (n) WHERE id(n) = -1 RETURN n.title");
    assert!(res.rows.is_empty());
    assert_eq!(e.full_scans(), 0);
}

#[test]
fn label_check_still_applies_to_seeked_candidates() {
    let e = hub_graph();
    // Node 3 exists but is a `person`; the label in the pattern must still be
    // enforced by the exact filter even though the candidate came from a seek.
    let res = run(&e, "MATCH (n:Ghost) WHERE id(n) = 3 RETURN n.title");
    assert!(res.rows.is_empty());
    assert_eq!(e.full_scans(), 0);
}

#[test]
fn non_integer_id_value_falls_back_to_the_exact_path() {
    let e = hub_graph();
    // `id(n) = 'x'` cannot seek; whatever the exact semantics are, they must
    // be produced by the ordinary path (narrowing skipped, not an empty seek).
    let q = parse("MATCH (n) WHERE id(n) = 'x' RETURN n.title").expect("parse");
    let seeked = execute_on_engine(&q, &e, HashMap::new());
    let plain = execute_on_engine(&q, &e.inner, HashMap::new());
    match (seeked, plain) {
        (Ok(a), Ok(b)) => assert_eq!(a.rows, b.rows),
        (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
        (a, b) => panic!(
            "seeked vs plain diverged: {:?} vs {:?}",
            a.map(|r| r.rows.len()).map_err(|e| e.to_string()),
            b.map(|r| r.rows.len()).map_err(|e| e.to_string())
        ),
    }
}

#[test]
fn disjunctive_id_terms_are_not_pushed() {
    let e = hub_graph();
    // Under OR the id term is not required, so it must NOT narrow: the scan
    // still happens and every node matches.
    let res = run(
        &e,
        "MATCH (n) WHERE id(n) = 1 OR n.title = 'p07' RETURN count(*)",
    );
    assert_eq!(res.rows, vec![vec![Value::Integer(2)]]);
    assert!(
        e.full_scans() >= 1,
        "an OR-guarded id term must not suppress the scan"
    );
}
