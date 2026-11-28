//! `NativeFtsIndex::search_with_changes` / `NativeFtsRelIndex::search_with_changes`:
//! full-text search over the committed index completed with what a
//! transaction has changed so far, so the transaction reads its own writes.
//!
//! The rule every test here comes down to: the answer inside the transaction
//! is the answer [`search`](NativeFtsIndex::search) gives once the
//! transaction is committed and the index has caught up - the same hits, in
//! the same order, with the same scores.

use drevo_core::engine::GraphEngine;
use drevo_core::model::{EdgePatch, NewEdge, NewNode, NodePatch, Properties};
use drevo_core::native::{NativeGraph, NativeTxId};
use drevo_core::native_fts::{NativeFtsIndex, NativeFtsRelIndex};

fn doc(title: &str) -> NewNode {
    NewNode {
        kind: "Doc".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn retitle(title: &str) -> NodePatch {
    NodePatch {
        title: Some(title.to_string()),
        ..NodePatch::default()
    }
}

fn note(text: &str) -> Properties {
    let mut p = Properties::default();
    p.insert("note", serde_json::json!(text));
    p
}

fn rel(from_id: u64, to_id: u64, text: &str) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: "MENTIONS".to_string(),
        weight: 1.0,
        properties: note(text),
    }
}

fn node_index(g: &NativeGraph) -> NativeFtsIndex {
    let mut fts = NativeFtsIndex::new();
    fts.sync(g);
    fts
}

fn rel_index(g: &NativeGraph) -> NativeFtsRelIndex {
    let mut fts = NativeFtsRelIndex::new();
    fts.sync(g);
    fts
}

/// The node search inside `tx`, over the index of the committed graph.
fn in_tx(g: &NativeGraph, tx: NativeTxId, query: &str, limit: usize) -> Vec<(u64, f32)> {
    let changes = g.tx_changes(tx).expect("open transaction");
    node_index(g).search_with_changes(query, limit, &changes.nodes)
}

/// The relationship search inside `tx`, over the index of the committed graph.
fn rels_in_tx(g: &NativeGraph, tx: NativeTxId, query: &str, limit: usize) -> Vec<(u64, f32)> {
    let changes = g.tx_changes(tx).expect("open transaction");
    rel_index(g).search_with_changes(query, limit, &changes.edges)
}

fn ids(hits: &[(u64, f32)]) -> Vec<u64> {
    hits.iter().map(|(id, _)| *id).collect()
}

// ── nodes ────────────────────────────────────────────────────────────────

#[test]
fn no_changes_answer_exactly_as_search() {
    let g = NativeGraph::new();
    for title in ["sourdough bread", "rye bread", "bread and butter pudding"] {
        g.create_node(doc(title)).unwrap();
    }
    let fts = node_index(&g);
    for query in ["bread", "rye", "butter", "absent"] {
        assert_eq!(
            fts.search_with_changes(query, 10, &[]),
            fts.search(query, 10),
            "{query}"
        );
    }
}

#[test]
fn a_node_created_in_the_transaction_is_found() {
    let g = NativeGraph::new();
    let sourdough = g.create_node(doc("sourdough bread")).unwrap();
    let tx = g.tx_begin();
    let rye = g
        .tx_engine(tx)
        .unwrap()
        .create_node(doc("rye bread"))
        .unwrap();

    let mut found = ids(&in_tx(&g, tx, "bread", 10));
    found.sort_unstable();
    assert_eq!(found, vec![sourdough.id, rye.id]);
    assert_eq!(ids(&in_tx(&g, tx, "rye", 10)), vec![rye.id]);
}

#[test]
fn a_node_deleted_in_the_transaction_is_left_out() {
    let g = NativeGraph::new();
    let sourdough = g.create_node(doc("sourdough bread")).unwrap();
    let stale = g.create_node(doc("stale bread")).unwrap();
    let tx = g.tx_begin();
    g.tx_engine(tx).unwrap().delete_node(stale.id).unwrap();

    assert_eq!(ids(&in_tx(&g, tx, "bread", 10)), vec![sourdough.id]);
    assert_eq!(ids(&in_tx(&g, tx, "stale", 10)), Vec::<u64>::new());
}

#[test]
fn a_retitled_node_matches_its_new_text_only() {
    let g = NativeGraph::new();
    let white = g.create_node(doc("white bread")).unwrap();
    let tx = g.tx_begin();
    g.tx_engine(tx)
        .unwrap()
        .update_node(white.id, retitle("white loaf"))
        .unwrap();

    assert_eq!(ids(&in_tx(&g, tx, "bread", 10)), Vec::<u64>::new());
    assert_eq!(ids(&in_tx(&g, tx, "loaf", 10)), vec![white.id]);
    assert_eq!(ids(&in_tx(&g, tx, "white", 10)), vec![white.id]);
}

#[test]
fn a_word_only_the_transaction_wrote_is_found() {
    let g = NativeGraph::new();
    g.create_node(doc("sourdough bread")).unwrap();
    let tx = g.tx_begin();
    let odd = g
        .tx_engine(tx)
        .unwrap()
        .create_node(doc("zqmarrow"))
        .unwrap();
    // No committed node holds any of the word's trigrams.
    assert_eq!(node_index(&g).search("zqmarrow", 10), vec![]);
    assert_eq!(ids(&in_tx(&g, tx, "zqmarrow", 10)), vec![odd.id]);
}

#[test]
fn an_empty_index_finds_what_the_transaction_wrote() {
    let g = NativeGraph::new();
    let tx = g.tx_begin();
    let rye = g
        .tx_engine(tx)
        .unwrap()
        .create_node(doc("rye bread"))
        .unwrap();
    let hits = in_tx(&g, tx, "bread", 10);
    assert_eq!(ids(&hits), vec![rye.id]);
    assert!(hits[0].1 > 0.0, "a positive score: {hits:?}");
}

#[test]
fn a_node_created_and_deleted_in_the_transaction_is_not_found() {
    let g = NativeGraph::new();
    let tx = g.tx_begin();
    let e = g.tx_engine(tx).unwrap();
    let rye = e.create_node(doc("rye bread")).unwrap();
    e.delete_node(rye.id).unwrap();
    assert_eq!(in_tx(&g, tx, "bread", 10), vec![]);
}

#[test]
fn the_limit_counts_after_the_changes() {
    let g = NativeGraph::new();
    // The best committed match for "bread" …
    let best = g.create_node(doc("bread")).unwrap();
    let second = g.create_node(doc("bread from the corner bakery")).unwrap();
    assert_eq!(ids(&node_index(&g).search("bread", 1)), vec![best.id]);
    // … is deleted by the transaction, so the single hit asked for is the
    // next one, not an empty answer.
    let tx = g.tx_begin();
    g.tx_engine(tx).unwrap().delete_node(best.id).unwrap();
    assert_eq!(ids(&in_tx(&g, tx, "bread", 1)), vec![second.id]);
    assert_eq!(in_tx(&g, tx, "bread", 0), vec![]);
}

/// A corpus, then a transaction that creates, retitles and deletes: every
/// query answers inside the transaction as it does after the commit.
#[test]
fn the_answer_in_the_transaction_is_the_answer_after_the_commit() {
    let g = NativeGraph::new();
    let words = [
        "bread",
        "butter",
        "rye",
        "sourdough",
        "loaf",
        "oven",
        "crust",
        "flour",
        "yeast",
        "salt",
    ];
    let mut committed = Vec::new();
    for i in 0..40usize {
        // Two to four words each, so lengths and term frequencies vary.
        let title: Vec<&str> = (0..2 + i % 3)
            .map(|j| words[(i * 7 + j * 3) % 10])
            .collect();
        committed.push(g.create_node(doc(&title.join(" "))).unwrap().id);
    }

    let tx = g.tx_begin();
    let e = g.tx_engine(tx).unwrap();
    for i in 0..6usize {
        e.create_node(doc(&format!("{} {} bread", words[i], words[9 - i])))
            .unwrap();
    }
    e.create_node(doc("zqmarrow bread")).unwrap();
    for (n, id) in committed.iter().enumerate() {
        match n % 5 {
            0 => e.delete_node(*id).unwrap(),
            1 => {
                e.update_node(*id, retitle(&format!("{} crust oven", words[n % 10])))
                    .unwrap();
            }
            _ => {}
        }
    }

    let queries = [
        "bread",
        "butter",
        "rye",
        "crust",
        "oven",
        "zqmarrow",
        "rye bread",
        "salt",
        "flour yeast",
        "absent",
    ];
    let inside: Vec<Vec<(u64, f32)>> = [3usize, 10, 100]
        .iter()
        .flat_map(|&k| queries.iter().map(move |q| (q, k)))
        .map(|(q, k)| in_tx(&g, tx, q, k))
        .collect();

    g.tx_commit(tx).unwrap();
    let fts = node_index(&g);
    let after: Vec<Vec<(u64, f32)>> = [3usize, 10, 100]
        .iter()
        .flat_map(|&k| queries.iter().map(move |q| (q, k)))
        .map(|(q, k)| fts.search(q, k))
        .collect();
    assert!(
        after.iter().any(|hits| !hits.is_empty()),
        "the queries match"
    );
    assert_eq!(inside, after);
}

/// The index may be ahead of the transaction's snapshot (another writer
/// committed since it began): the transaction's own changes still apply.
#[test]
fn changes_apply_over_an_index_newer_than_the_snapshot() {
    let g = NativeGraph::new();
    let white = g.create_node(doc("white bread")).unwrap();
    let tx = g.tx_begin();
    g.tx_engine(tx)
        .unwrap()
        .update_node(white.id, retitle("white loaf"))
        .unwrap();
    g.create_node(doc("someone else's bread")).unwrap();

    assert_eq!(ids(&in_tx(&g, tx, "loaf", 10)), vec![white.id]);
    assert!(!ids(&in_tx(&g, tx, "bread", 10)).contains(&white.id));
}

// ── relationships ────────────────────────────────────────────────────────

#[test]
fn no_relationship_changes_answer_exactly_as_search() {
    let g = NativeGraph::new();
    let a = g.create_node(doc("a")).unwrap().id;
    let b = g.create_node(doc("b")).unwrap().id;
    g.create_edge(rel(a, b, "baked the bread")).unwrap();
    g.create_edge(rel(b, a, "ate the bread and the butter"))
        .unwrap();
    let fts = rel_index(&g);
    for query in ["bread", "butter", "absent"] {
        assert_eq!(
            fts.search_with_changes(query, 10, &[]),
            fts.search(query, 10),
            "{query}"
        );
    }
}

#[test]
fn a_relationship_created_in_the_transaction_is_found() {
    let g = NativeGraph::new();
    let a = g.create_node(doc("a")).unwrap().id;
    let b = g.create_node(doc("b")).unwrap().id;
    let tx = g.tx_begin();
    let baked = g
        .tx_engine(tx)
        .unwrap()
        .create_edge(rel(a, b, "baked the bread"))
        .unwrap();
    assert_eq!(rel_index(&g).search("bread", 10), vec![]);
    assert_eq!(ids(&rels_in_tx(&g, tx, "bread", 10)), vec![baked.id]);
}

#[test]
fn a_changed_or_deleted_relationship_is_seen_as_the_transaction_left_it() {
    let g = NativeGraph::new();
    let a = g.create_node(doc("a")).unwrap().id;
    let b = g.create_node(doc("b")).unwrap().id;
    let baked = g.create_edge(rel(a, b, "baked the bread")).unwrap();
    let ate = g.create_edge(rel(b, a, "ate the bread")).unwrap();
    let tx = g.tx_begin();
    let e = g.tx_engine(tx).unwrap();
    e.update_edge(
        baked.id,
        EdgePatch {
            properties: Some(note("baked the loaf")),
            ..EdgePatch::default()
        },
    )
    .unwrap();
    e.delete_edge(ate.id).unwrap();

    assert_eq!(rels_in_tx(&g, tx, "bread", 10), vec![]);
    assert_eq!(ids(&rels_in_tx(&g, tx, "loaf", 10)), vec![baked.id]);
}

#[test]
fn relationships_of_a_deleted_node_are_left_out() {
    let g = NativeGraph::new();
    let a = g.create_node(doc("a")).unwrap().id;
    let b = g.create_node(doc("b")).unwrap().id;
    let c = g.create_node(doc("c")).unwrap().id;
    g.create_edge(rel(a, b, "baked the bread")).unwrap();
    let kept = g.create_edge(rel(b, c, "sold the bread")).unwrap();
    let tx = g.tx_begin();
    g.tx_engine(tx).unwrap().delete_node(a).unwrap();
    assert_eq!(ids(&rels_in_tx(&g, tx, "bread", 10)), vec![kept.id]);
}

#[test]
fn the_relationship_answer_in_the_transaction_is_the_answer_after_the_commit() {
    let g = NativeGraph::new();
    let words = ["bread", "butter", "rye", "loaf", "oven", "crust", "flour"];
    let nodes: Vec<u64> = (0..8)
        .map(|i| g.create_node(doc(&format!("n{i}"))).unwrap().id)
        .collect();
    let mut committed = Vec::new();
    for i in 0..24usize {
        let text = format!(
            "{} {} {}",
            words[i % 7],
            words[(i * 3 + 1) % 7],
            words[(i * 5 + 2) % 7]
        );
        committed.push(
            g.create_edge(rel(nodes[i % 8], nodes[(i + 1) % 8], &text))
                .unwrap()
                .id,
        );
    }

    let tx = g.tx_begin();
    let e = g.tx_engine(tx).unwrap();
    for i in 0..5usize {
        e.create_edge(rel(
            nodes[i],
            nodes[i + 2],
            &format!("{} zqmarrow bread", words[i]),
        ))
        .unwrap();
    }
    for (n, id) in committed.iter().enumerate() {
        match n % 4 {
            0 => e.delete_edge(*id).unwrap(),
            1 => {
                e.update_edge(
                    *id,
                    EdgePatch {
                        properties: Some(note(&format!("{} crust oven", words[n % 7]))),
                        ..EdgePatch::default()
                    },
                )
                .unwrap();
            }
            _ => {}
        }
    }

    let queries = [
        "bread",
        "butter",
        "crust",
        "oven",
        "zqmarrow",
        "rye bread",
        "absent",
    ];
    let inside: Vec<Vec<(u64, f32)>> = [2usize, 50]
        .iter()
        .flat_map(|&k| queries.iter().map(move |q| (q, k)))
        .map(|(q, k)| rels_in_tx(&g, tx, q, k))
        .collect();

    g.tx_commit(tx).unwrap();
    let fts = rel_index(&g);
    let after: Vec<Vec<(u64, f32)>> = [2usize, 50]
        .iter()
        .flat_map(|&k| queries.iter().map(move |q| (q, k)))
        .map(|(q, k)| fts.search(q, k))
        .collect();
    assert!(
        after.iter().any(|hits| !hits.is_empty()),
        "the queries match"
    );
    assert_eq!(inside, after);
}
