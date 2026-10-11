//! A labelled `MATCH` whose candidates an index already narrowed must not load
//! every node of the label (issue #600). The exact per-node filter checks the
//! labels anyway, so enumerating the label only cost time: ~22 ms per query on
//! 200 000 nodes.
//!
//! The engine here counts `nodes_by_kind` / `all_nodes` calls, so the tests see
//! whether the executor walked the label or the whole graph.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use drevo::cypher::executor::{
    execute_on_engine_with_context, ExecResult, NativeQueryContext, Value,
};
use drevo::cypher::parser::parse;
use drevo::native::NativeGraph;
use drevo::native_label_index::NativeLabelIndex;
use drevo::native_property_index::NativePropertyIndex;
use drevo::native_text_index::{NativeTextIndex, TextIndexSpec};
use drevo_core::dump::{Dump, ImportReport};
use drevo_core::engine::GraphEngine;
use drevo_core::error::Result;
use drevo_core::model::{Direction, Edge, EdgePatch, NewEdge, NewNode, Node, NodePatch};

/// A [`NativeGraph`] that counts the calls enumerating many nodes.
struct Counting {
    inner: NativeGraph,
    by_kind: AtomicUsize,
    all: AtomicUsize,
}

impl Counting {
    fn scans(&self) -> (usize, usize) {
        (
            self.by_kind.load(Ordering::SeqCst),
            self.all.load(Ordering::SeqCst),
        )
    }

    fn reset(&self) {
        self.by_kind.store(0, Ordering::SeqCst);
        self.all.store(0, Ordering::SeqCst);
    }
}

impl GraphEngine for Counting {
    fn create_node(&self, n: NewNode) -> Result<Node> {
        self.inner.create_node(n)
    }
    fn get_node(&self, id: u64) -> Result<Option<Arc<Node>>> {
        self.inner.get_node(id)
    }
    fn update_node(&self, id: u64, p: NodePatch) -> Result<Node> {
        self.inner.update_node(id, p)
    }
    fn delete_node(&self, id: u64) -> Result<()> {
        self.inner.delete_node(id)
    }
    fn create_edge(&self, e: NewEdge) -> Result<Edge> {
        self.inner.create_edge(e)
    }
    fn get_edge(&self, id: u64) -> Result<Option<Edge>> {
        self.inner.get_edge(id)
    }
    fn update_edge(&self, id: u64, p: EdgePatch) -> Result<Edge> {
        self.inner.update_edge(id, p)
    }
    fn delete_edge(&self, id: u64) -> Result<()> {
        self.inner.delete_edge(id)
    }
    fn neighbor_ids(&self, id: u64, d: Direction, k: Option<&str>) -> Result<Vec<u64>> {
        self.inner.neighbor_ids(id, d, k)
    }
    fn neighbors(&self, id: u64, d: Direction, k: Option<&str>) -> Result<Vec<Arc<Node>>> {
        self.inner.neighbors(id, d, k)
    }
    fn edges_of(&self, id: u64, d: Direction) -> Result<Vec<Edge>> {
        self.inner.edges_of(id, d)
    }
    fn all_nodes(&self) -> Result<Vec<Arc<Node>>> {
        self.all.fetch_add(1, Ordering::SeqCst);
        self.inner.all_nodes()
    }
    fn all_edges(&self) -> Result<Vec<Edge>> {
        self.inner.all_edges()
    }
    fn nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Arc<Node>>> {
        self.by_kind.fetch_add(1, Ordering::SeqCst);
        self.inner.nodes_by_kind(kind, limit, offset)
    }
    fn export_dump(&self) -> Result<Dump> {
        self.inner.export_dump()
    }
    fn apply_dump(&self, dump: Dump) -> Result<ImportReport> {
        self.inner.apply_dump(dump)
    }
}

/// An IT task manager with tickets, some carrying a secondary `Urgent` label.
struct Fixture {
    graph: Counting,
    labels: NativeLabelIndex,
    props: NativePropertyIndex,
    texts: NativeTextIndex,
}

impl Fixture {
    fn new() -> Self {
        let graph = Counting {
            inner: NativeGraph::new(),
            by_kind: AtomicUsize::new(0),
            all: AtomicUsize::new(0),
        };
        let mut f = Self {
            graph,
            labels: NativeLabelIndex::new(),
            props: NativePropertyIndex::new(),
            texts: NativeTextIndex::new(vec![TextIndexSpec::new(
                Some("Ticket".into()),
                vec!["summary".into()],
            )
            .expect("spec")]),
        };
        f.run(
            "UNWIND range(1, 50) AS i \
             CREATE (:Ticket {title: 'T-' + toString(i), key: 'K' + toString(i), \
                     summary: 'PROJ-' + toString(i) + ' crash'})",
        );
        f.run("CREATE (:Note {title: 'note', key: 'K7', summary: 'PROJ-7 crash'})");
        // Like the service, bring the indexes up to date before the next
        // statement reads through them.
        f.sync();
        f.run("MATCH (t:Ticket {key: 'K3'}) SET t:Urgent");
        f.sync();
        f
    }

    fn sync(&mut self) {
        self.labels.sync(&self.graph.inner);
        self.props.sync(&self.graph.inner);
        self.texts.sync(&self.graph.inner);
    }

    fn run(&self, q: &str) -> ExecResult {
        let ctx = NativeQueryContext {
            labels: Some(&self.labels),
            properties: Some(&self.props),
            texts: Some(&self.texts),
            ..NativeQueryContext::default()
        };
        execute_on_engine_with_context(&parse(q).expect("parse"), &self.graph, &ctx, HashMap::new())
            .unwrap_or_else(|e| panic!("`{q}`: {e:?}"))
    }

    fn titles(&self, q: &str) -> Vec<String> {
        self.run(q)
            .rows
            .iter()
            .map(|r| match &r[0] {
                Value::String(s) => s.clone(),
                other => panic!("{other:?}"),
            })
            .collect()
    }
}

#[test]
fn an_indexed_predicate_skips_the_label_scan() {
    let f = Fixture::new();
    for (q, want) in [
        (
            "MATCH (t:Ticket) WHERE t.key = 'K7' RETURN t.title",
            vec!["T-7"],
        ),
        ("MATCH (t:Ticket {key: 'K7'}) RETURN t.title", vec!["T-7"]),
        (
            "MATCH (t:Ticket) WHERE t.key IN ['K7', 'K9'] RETURN t.title ORDER BY t.title",
            vec!["T-7", "T-9"],
        ),
        (
            "MATCH (t:Ticket) WHERE t.summary STARTS WITH 'PROJ-7 ' RETURN t.title",
            vec!["T-7"],
        ),
    ] {
        f.graph.reset();
        assert_eq!(f.titles(q), want, "{q}");
        assert_eq!(f.graph.scans(), (0, 0), "{q} must not enumerate nodes");
    }
}

#[test]
fn labels_are_still_enforced_on_index_candidates() {
    let f = Fixture::new();
    // The Note shares the key and summary but is not a Ticket.
    assert_eq!(
        f.titles("MATCH (t:Ticket {key: 'K7'}) RETURN t.title"),
        ["T-7"]
    );
    // A second (secondary) label and label expressions are checked too.
    assert_eq!(
        f.titles("MATCH (t:Ticket:Urgent) WHERE t.key IN ['K3', 'K4'] RETURN t.title"),
        ["T-3"]
    );
    assert_eq!(
        f.titles("MATCH (t:Urgent) WHERE t.key IN ['K3', 'K4'] RETURN t.title"),
        ["T-3"]
    );
    assert_eq!(
        f.titles("MATCH (t:Ticket&!Urgent) WHERE t.key IN ['K3', 'K4'] RETURN t.title"),
        ["T-4"]
    );
}

#[test]
fn without_an_index_candidate_the_label_still_narrows() {
    let f = Fixture::new();
    f.graph.reset();
    assert_eq!(
        f.titles("MATCH (t:Ticket) WHERE t.title ENDS WITH '-50' RETURN t.title"),
        ["T-50"]
    );
    let (by_kind, all) = f.graph.scans();
    assert_eq!(by_kind, 1, "the label set is used");
    assert_eq!(all, 0, "never the whole graph");
}
