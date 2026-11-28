//! `NativeFtsIndex::without_labels`: a full-text index that leaves out every
//! node carrying one of the given labels.
//!
//! The rule every test here comes down to: the index answers as if the
//! skipped nodes were not in the graph - the same hits, in the same order,
//! with the same scores.

use drevo_core::engine::GraphEngine;
use drevo_core::labels::SECONDARY_LABELS_KEY;
use drevo_core::model::{NewNode, NodePatch, Properties};
use drevo_core::native::NativeGraph;
use drevo_core::native_fts::NativeFtsIndex;

fn node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

/// A `Doc` that also carries `label` as a secondary label.
fn doc_also_labelled(label: &str, title: &str) -> NewNode {
    let mut properties = Properties::default();
    properties.insert(SECONDARY_LABELS_KEY, serde_json::json!([label]));
    NewNode {
        properties,
        ..node("Doc", title)
    }
}

fn skipping_queue(g: &NativeGraph) -> NativeFtsIndex {
    let mut fts = NativeFtsIndex::without_labels(["Queue", "Trace"]);
    fts.sync(g);
    fts
}

fn everything(g: &NativeGraph) -> NativeFtsIndex {
    let mut fts = NativeFtsIndex::new();
    fts.sync(g);
    fts
}

fn ids(hits: &[(u64, f32)]) -> Vec<u64> {
    hits.iter().map(|(id, _)| *id).collect()
}

#[test]
fn a_node_with_a_skipped_label_is_not_a_hit() {
    let g = NativeGraph::new();
    let bread = g.create_node(node("Doc", "sourdough bread")).unwrap();
    g.create_node(node("Queue", "bread to be read later"))
        .unwrap();
    g.create_node(node("Trace", "what is bread")).unwrap();

    let fts = skipping_queue(&g);
    assert_eq!(ids(&fts.search("bread", 10)), vec![bread.id]);
    assert_eq!(fts.len(), 1, "only the Doc is indexed");
    assert_eq!(fts.doc_count(), 1);
}

#[test]
fn a_secondary_label_counts() {
    let g = NativeGraph::new();
    let bread = g.create_node(node("Doc", "sourdough bread")).unwrap();
    g.create_node(doc_also_labelled("Queue", "queued bread"))
        .unwrap();

    assert_eq!(ids(&skipping_queue(&g).search("bread", 10)), vec![bread.id]);
}

#[test]
fn the_rest_scores_as_if_the_skipped_nodes_were_not_there() {
    let with = NativeGraph::new();
    let without = NativeGraph::new();
    for title in ["sourdough bread", "rye bread", "bread and butter pudding"] {
        with.create_node(node("Doc", title)).unwrap();
        without.create_node(node("Doc", title)).unwrap();
    }
    // Long, and full of the word: indexed, it would change the document
    // count, the average length and the word's document frequency.
    with.create_node(node(
        "Queue",
        "bread bread bread, a whole chapter about bread waiting in the queue",
    ))
    .unwrap();

    let skipping = skipping_queue(&with);
    let plain = everything(&without);
    for query in ["bread", "rye", "butter pudding", "queue"] {
        assert_eq!(
            skipping.search(query, 10),
            plain.search(query, 10),
            "{query}"
        );
    }
    assert_ne!(
        everything(&with).search("bread", 10),
        plain.search("bread", 10),
        "the test has teeth: indexed, the queue row changes the answer"
    );
}

#[test]
fn a_node_that_gains_a_skipped_label_leaves_the_index() {
    let g = NativeGraph::new();
    let bread = g.create_node(node("Doc", "sourdough bread")).unwrap();
    let mut fts = skipping_queue(&g);
    assert_eq!(ids(&fts.search("bread", 10)), vec![bread.id]);

    g.update_node(
        bread.id,
        NodePatch {
            kind: Some("Queue".to_string()),
            ..NodePatch::default()
        },
    )
    .unwrap();
    fts.sync(&g);
    assert_eq!(ids(&fts.search("bread", 10)), Vec::<u64>::new());
    assert!(fts.is_empty());
}

#[test]
fn a_node_that_loses_its_skipped_label_enters_the_index() {
    let g = NativeGraph::new();
    let bread = g.create_node(node("Queue", "sourdough bread")).unwrap();
    let mut fts = skipping_queue(&g);
    assert!(fts.is_empty());

    g.update_node(
        bread.id,
        NodePatch {
            kind: Some("Doc".to_string()),
            ..NodePatch::default()
        },
    )
    .unwrap();
    fts.sync(&g);
    assert_eq!(ids(&fts.search("bread", 10)), vec![bread.id]);
}

#[test]
fn deleting_a_skipped_node_changes_nothing() {
    let g = NativeGraph::new();
    let bread = g.create_node(node("Doc", "sourdough bread")).unwrap();
    let queued = g.create_node(node("Queue", "queued bread")).unwrap();
    let mut fts = skipping_queue(&g);
    let before = fts.search("bread", 10);

    g.delete_node(queued.id).unwrap();
    fts.sync(&g);
    assert_eq!(fts.search("bread", 10), before);
    assert_eq!(ids(&before), vec![bread.id]);
}

#[test]
fn a_transaction_does_not_see_its_own_skipped_nodes_either() {
    let g = NativeGraph::new();
    let bread = g.create_node(node("Doc", "sourdough bread")).unwrap();
    let fts = skipping_queue(&g);

    let tx = g.tx_begin();
    let engine = g.tx_engine(tx).unwrap();
    engine.create_node(node("Queue", "queued bread")).unwrap();
    let rye = engine.create_node(node("Doc", "rye bread")).unwrap();
    let changes = g.tx_changes(tx).expect("open transaction");
    let inside = fts.search_with_changes("bread", 10, &changes.nodes);
    let mut found = ids(&inside);
    found.sort_unstable();
    assert_eq!(found, vec![bread.id, rye.id]);

    // The answer inside the transaction is the answer after its commit.
    g.tx_commit(tx).unwrap();
    assert_eq!(inside, skipping_queue(&g).search("bread", 10));
}

#[test]
fn a_transaction_that_moves_a_node_under_a_skipped_label_stops_finding_it() {
    let g = NativeGraph::new();
    let bread = g.create_node(node("Doc", "sourdough bread")).unwrap();
    let fts = skipping_queue(&g);

    let tx = g.tx_begin();
    g.tx_engine(tx)
        .unwrap()
        .update_node(
            bread.id,
            NodePatch {
                kind: Some("Queue".to_string()),
                ..NodePatch::default()
            },
        )
        .unwrap();
    let changes = g.tx_changes(tx).expect("open transaction");
    assert_eq!(
        ids(&fts.search_with_changes("bread", 10, &changes.nodes)),
        Vec::<u64>::new()
    );
}

#[test]
fn an_index_made_with_new_skips_nothing() {
    let g = NativeGraph::new();
    g.create_node(node("Doc", "sourdough bread")).unwrap();
    g.create_node(node("Queue", "queued bread")).unwrap();
    assert_eq!(everything(&g).search("bread", 10).len(), 2);
    assert_eq!(
        NativeFtsIndex::without_labels(Vec::<String>::new()).len(),
        0
    );
}
