//! Count pushdown (RFC `docs/rfc-native-core.md`, #307 — the last
//! Memgraph-scoreboard gap from baseline run 6): a bare
//! `MATCH (n[:Label]) RETURN count(*)` must be answered from cardinalities —
//! [`GraphEngine::count_nodes`] / the kind bucket + label index — instead of
//! enumerating, projecting, and aggregating every node.
//!
//! Like the `id()`-seek tests, "the scan was skipped" is an assertion
//! through a counting engine decorator, not a timing guess; every query's
//! rows are still checked, and any shape the detector does not recognise
//! must take the ordinary path with identical results.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use drevo::cypher::executor::{
    execute, execute_on_engine, execute_on_engine_with_indexes, ExecResult, Value,
};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::engine::GraphEngine;
use drevo::model::{Direction, Edge, EdgePatch, NewEdge, NewNode, Node, NodePatch, Properties};
use drevo::native::NativeGraph;
use drevo::native_label_index::NativeLabelIndex;
use drevo_core::dump::{Dump, ImportReport};
use drevo_core::error::Result;

/// A [`GraphEngine`] decorator that counts full scans, point lookups, and
/// cardinality calls.
struct CountingEngine {
    inner: NativeGraph,
    all_nodes_calls: AtomicUsize,
    get_node_calls: AtomicUsize,
    count_nodes_calls: AtomicUsize,
    count_kind_calls: AtomicUsize,
}

impl CountingEngine {
    fn new(inner: NativeGraph) -> Self {
        CountingEngine {
            inner,
            all_nodes_calls: AtomicUsize::new(0),
            get_node_calls: AtomicUsize::new(0),
            count_nodes_calls: AtomicUsize::new(0),
            count_kind_calls: AtomicUsize::new(0),
        }
    }

    fn full_scans(&self) -> usize {
        self.all_nodes_calls.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.all_nodes_calls.store(0, Ordering::SeqCst);
        self.get_node_calls.store(0, Ordering::SeqCst);
        self.count_nodes_calls.store(0, Ordering::SeqCst);
        self.count_kind_calls.store(0, Ordering::SeqCst);
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
    fn count_nodes(&self) -> Result<u64> {
        self.count_nodes_calls.fetch_add(1, Ordering::SeqCst);
        GraphEngine::count_nodes(&self.inner)
    }
    fn count_nodes_by_kind(&self, kind: &str) -> Result<u64> {
        self.count_kind_calls.fetch_add(1, Ordering::SeqCst);
        GraphEngine::count_nodes_by_kind(&self.inner, kind)
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

fn node(kind: &str, title: &str, labels: &[&str]) -> NewNode {
    let mut props: HashMap<String, serde_json::Value> = HashMap::new();
    if !labels.is_empty() {
        props.insert(
            "_labels".to_string(),
            serde_json::Value::Array(
                labels
                    .iter()
                    .map(|l| serde_json::Value::String(l.to_string()))
                    .collect(),
            ),
        );
    }
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(props),
    }
}

/// 3 `person` + 2 `city` nodes; one person carries a secondary `mayor`
/// label, and one node's *kind* is `mayor` too (the union-dedup case).
fn seeded() -> CountingEngine {
    let g = NativeGraph::new();
    g.create_node(node("person", "ada", &[])).unwrap();
    g.create_node(node("person", "bob", &["mayor"])).unwrap();
    g.create_node(node("person", "cy", &[])).unwrap();
    g.create_node(node("city", "paris", &[])).unwrap();
    g.create_node(node("mayor", "eve", &["mayor"])).unwrap();
    CountingEngine::new(g)
}

fn labels_for(engine: &CountingEngine) -> NativeLabelIndex {
    let mut idx = NativeLabelIndex::new();
    idx.sync(&engine.inner);
    idx
}

fn run_plain(engine: &CountingEngine, source: &str) -> ExecResult {
    let q = parse(source).expect("parse");
    execute_on_engine(&q, engine, HashMap::new()).expect("execute")
}

fn run_indexed(engine: &CountingEngine, idx: &NativeLabelIndex, source: &str) -> ExecResult {
    let q = parse(source).expect("parse");
    execute_on_engine_with_indexes(&q, engine, None, Some(idx), None, HashMap::new())
        .expect("execute")
}

fn single_int(result: &ExecResult) -> i64 {
    assert_eq!(result.rows.len(), 1, "count returns one row: {result:?}");
    match result.rows[0].as_slice() {
        [Value::Integer(n)] => *n,
        other => panic!("expected one integer, got {other:?}"),
    }
}

// ── the pushdown itself ────────────────────────────────────────────────

#[test]
fn bare_count_star_skips_the_scan_on_any_engine() {
    let engine = seeded();
    for source in [
        "MATCH (n) RETURN count(*)",
        "MATCH () RETURN count(*)",
        "MATCH (n) RETURN count(n)",
        "MATCH (n) RETURN COUNT(*)",
    ] {
        engine.reset();
        let result = run_plain(&engine, source);
        assert_eq!(single_int(&result), 5, "{source}");
        assert_eq!(engine.full_scans(), 0, "{source} must not scan");
        assert_eq!(
            engine.count_nodes_calls.load(Ordering::SeqCst),
            1,
            "{source} must use the cardinality"
        );
    }
}

#[test]
fn count_star_columns_and_rows_match_the_ordinary_path() {
    let engine = seeded();
    let pushed = run_plain(&engine, "MATCH (n) RETURN count(*)");
    // `WHERE 1 = 1` defeats the detector but not the semantics — this is
    // the ordinary enumerate + aggregate path.
    let ordinary = run_plain(&engine, "MATCH (n) WHERE 1 = 1 RETURN count(*)");
    assert!(engine.full_scans() >= 1, "the WHERE variant must scan");
    assert_eq!(pushed, ordinary, "pushdown must be output-identical");

    let aliased = run_plain(&engine, "MATCH (n) RETURN count(*) AS total");
    assert_eq!(aliased.columns, ["total"]);
    assert_eq!(single_int(&aliased), 5);
}

#[test]
fn labelled_count_uses_the_kind_bucket_and_label_index() {
    let engine = seeded();
    let idx = labels_for(&engine);
    engine.reset();

    // kind person = 3, none carry `person` as a secondary label.
    let persons = run_indexed(&engine, &idx, "MATCH (n:person) RETURN count(*)");
    assert_eq!(single_int(&persons), 3);
    assert_eq!(engine.full_scans(), 0, "labelled count must not scan");
    assert_eq!(engine.count_kind_calls.load(Ordering::SeqCst), 1);

    // `mayor`: kind eve + secondary-label bob; eve also carries the
    // secondary label, and must not be counted twice.
    engine.reset();
    let mayors = run_indexed(&engine, &idx, "MATCH (n:mayor) RETURN count(*)");
    assert_eq!(
        single_int(&mayors),
        2,
        "kind ∪ secondary label, deduplicated"
    );
    assert_eq!(engine.full_scans(), 0);

    // A label nothing carries.
    engine.reset();
    let ghosts = run_indexed(&engine, &idx, "MATCH (n:ghost) RETURN count(*)");
    assert_eq!(single_int(&ghosts), 0);
    assert_eq!(engine.full_scans(), 0);
}

#[test]
fn labelled_count_without_a_label_index_takes_the_ordinary_path() {
    let engine = seeded();
    let idx = labels_for(&engine);
    // No index → the union is not computable exactly → ordinary scan.
    let plain = run_plain(&engine, "MATCH (n:mayor) RETURN count(*)");
    assert!(engine.full_scans() >= 1, "no index → the scan must happen");
    // Same answer as the indexed pushdown.
    engine.reset();
    let indexed = run_indexed(&engine, &idx, "MATCH (n:mayor) RETURN count(*)");
    assert_eq!(plain, indexed);
    assert_eq!(engine.full_scans(), 0);
}

#[test]
fn count_pushdown_applies_per_union_arm() {
    let engine = seeded();
    let result = run_plain(
        &engine,
        "MATCH (n) RETURN count(*) AS c UNION ALL MATCH (m:city) RETURN count(m) AS c",
    );
    assert_eq!(engine.full_scans(), 1, "only the labelled plain arm scans");
    assert_eq!(result.columns, ["c"]);
    let counts: Vec<i64> = result
        .rows
        .iter()
        .map(|r| match r.as_slice() {
            [Value::Integer(n)] => *n,
            other => panic!("expected integer, got {other:?}"),
        })
        .collect();
    assert_eq!(counts, [5, 1]);
}

#[test]
fn empty_graph_counts_zero_without_scanning() {
    let engine = CountingEngine::new(NativeGraph::new());
    let result = run_plain(&engine, "MATCH (n) RETURN count(*)");
    assert_eq!(single_int(&result), 0);
    assert_eq!(engine.full_scans(), 0);
}

// ── shapes the detector must leave alone ───────────────────────────────

#[test]
fn non_pushdown_shapes_take_the_ordinary_path_with_correct_rows() {
    let engine = seeded();
    // (source, expected count) — every one of these must scan.
    for (source, expected) in [
        ("MATCH (n) WHERE n.title = 'ada' RETURN count(*)", 1),
        ("MATCH (n {title: 'ada'}) RETURN count(*)", 1),
        ("OPTIONAL MATCH (n:ghost) RETURN count(n)", 0),
        ("MATCH (n) RETURN count(DISTINCT n)", 5),
        ("MATCH (a), (b) RETURN count(*)", 25),
        ("MATCH (n) RETURN count(*) LIMIT 1", 5),
    ] {
        engine.reset();
        let result = run_plain(&engine, source);
        assert_eq!(single_int(&result), expected, "{source}");
        assert!(engine.full_scans() >= 1, "{source} must take the scan path");
    }

    // Two projections — not a bare count.
    engine.reset();
    let two = run_plain(&engine, "MATCH (n) RETURN count(*), 1");
    assert_eq!(two.rows, vec![vec![Value::Integer(5), Value::Integer(1)]]);
    assert!(engine.full_scans() >= 1);

    // A relationship pattern is never pushed.
    engine.reset();
    let rels = run_plain(&engine, "MATCH (a)-->(b) RETURN count(*)");
    assert_eq!(single_int(&rels), 0);
}

// ── KV engine parity ───────────────────────────────────────────────────

#[test]
fn kv_count_star_agrees_with_the_ordinary_path() {
    let db = Drevo::open_in_memory().expect("open");
    for stmt in [
        "CREATE (:Person {title: 'ada'})",
        "CREATE (:Person {title: 'bob'})",
        "CREATE (:City {title: 'paris'})",
    ] {
        let q = parse(stmt).expect("parse");
        execute(&q, &db, HashMap::new()).expect("seed");
    }
    let q = parse("MATCH (n) RETURN count(*)").expect("parse");
    let pushed = execute(&q, &db, HashMap::new()).expect("pushed");
    let q = parse("MATCH (n) WHERE 1 = 1 RETURN count(*)").expect("parse");
    let ordinary = execute(&q, &db, HashMap::new()).expect("ordinary");
    assert_eq!(pushed, ordinary);
    assert_eq!(single_int(&pushed), 3);

    // Deletes must be reflected — the key population, not a stale counter.
    let q = parse("MATCH (n {title: 'bob'}) DELETE n").expect("parse");
    execute(&q, &db, HashMap::new()).expect("delete");
    let q = parse("MATCH (n) RETURN count(*)").expect("parse");
    assert_eq!(
        single_int(&execute(&q, &db, HashMap::new()).expect("recount")),
        2
    );

    // The labelled form has no complete label index on KV — ordinary path,
    // same answer as counting through a scan. Only `ada` is left.
    let q = parse("MATCH (n:Person) RETURN count(*)").expect("parse");
    assert_eq!(
        single_int(&execute(&q, &db, HashMap::new()).expect("label")),
        1
    );
}
