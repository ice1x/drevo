//! The native full-text index (RFC `docs/rfc-native-core.md`, #307, Phase 6.3)
//! — a trigram BM25 index that tails a [`NativeGraph`]'s change-feed.
//!
//! These lock: it finds and ranks matching nodes, reflects updates/deletes
//! after `sync`, rebuilds when the feed was trimmed past its cursor, and — the
//! headline — returns the **same ranking as the KV store's `search_fts`** on a
//! shared corpus, so it is a faithful native replacement for the FTS subsystem.

use drevo::db::Drevo;
use drevo::engine::GraphEngine;
use drevo::model::{NewNode, NodePatch};
use drevo::native::NativeGraph;
use drevo::native_fts::NativeFtsIndex;

fn doc(title: &str, body: &str) -> NewNode {
    NewNode {
        kind: "doc".into(),
        title: title.into(),
        body: body.into(),
        body_html: String::new(),
        properties: Default::default(),
    }
}

fn ids(hits: &[(u64, f32)]) -> Vec<u64> {
    hits.iter().map(|(id, _)| *id).collect()
}

#[test]
fn finds_and_ranks_matching_nodes() {
    let g = NativeGraph::new();
    let fox = g.create_node(doc("the quick brown fox", "")).unwrap();
    let dog = g.create_node(doc("the lazy dog", "")).unwrap();
    let both = g
        .create_node(doc("quick fox and lazy dog", "a quick quick fox"))
        .unwrap();

    let mut fts = NativeFtsIndex::new();
    fts.sync(&g);
    assert_eq!(fts.len(), 3);

    // "quick" matches the fox docs, not the dog-only doc.
    let hits = fts.search("quick", 10);
    let hit_ids = ids(&hits);
    assert!(hit_ids.contains(&fox.id));
    assert!(hit_ids.contains(&both.id));
    assert!(!hit_ids.contains(&dog.id));
    // `both` mentions "quick" more, so it should not rank below `fox`.
    let pos_both = hit_ids.iter().position(|&i| i == both.id).unwrap();
    let pos_fox = hit_ids.iter().position(|&i| i == fox.id).unwrap();
    assert!(pos_both <= pos_fox);
}

#[test]
fn update_and_delete_reflected_after_sync() {
    let g = NativeGraph::new();
    let a = g.create_node(doc("alpha keyword", "")).unwrap();
    let b = g.create_node(doc("beta keyword", "")).unwrap();

    let mut fts = NativeFtsIndex::new();
    fts.sync(&g);
    assert_eq!(ids(&fts.search("keyword", 10)).len(), 2);
    assert!(ids(&fts.search("alpha", 10)).contains(&a.id));

    // Rename `a` so it no longer contains "alpha".
    g.update_node(
        a.id,
        NodePatch {
            title: Some("gamma keyword".into()),
            ..Default::default()
        },
    )
    .unwrap();
    // Delete `b`.
    g.delete_node(b.id).unwrap();
    fts.sync(&g);

    assert!(fts.search("alpha", 10).is_empty(), "old term must be gone");
    assert_eq!(ids(&fts.search("keyword", 10)), vec![a.id]);
    assert!(ids(&fts.search("gamma", 10)).contains(&a.id));
}

#[test]
fn rebuilds_when_feed_trimmed_past_cursor() {
    let g = NativeGraph::new();
    let a = g.create_node(doc("indexed early", "")).unwrap();

    let mut fts = NativeFtsIndex::new();
    fts.sync(&g);
    assert!(ids(&fts.search("indexed", 10)).contains(&a.id));

    // The graph churns and the owner trims the feed past the index's cursor.
    let b = g.create_node(doc("added later", "")).unwrap();
    g.trim_before(g.change_head());

    // sync must notice the lag and rebuild from a fresh snapshot.
    fts.sync(&g);
    assert!(ids(&fts.search("indexed", 10)).contains(&a.id));
    assert!(ids(&fts.search("later", 10)).contains(&b.id));
}

#[test]
fn ranking_matches_kv_search_fts_on_a_shared_corpus() {
    // Build the identical corpus, in the same order, on both engines — so node
    // ids line up — then compare the native index against the KV FTS ranker.
    let corpus = [
        doc(
            "graph databases store nodes and edges",
            "native graph engine",
        ),
        doc(
            "full text search over documents",
            "trigram index and ranking",
        ),
        doc("the native graph core", "nodes edges and a change feed"),
        doc("vector similarity search", "embeddings and cosine distance"),
        doc("relational databases use tables", "rows and columns"),
    ];

    let native = NativeGraph::new();
    let kv = Drevo::open_in_memory().unwrap();
    for d in &corpus {
        native.create_node(d.clone()).unwrap();
        kv.create_node(d.clone()).unwrap();
    }

    let mut fts = NativeFtsIndex::new();
    fts.sync(&native);

    for query in [
        "graph",
        "native graph",
        "search",
        "databases",
        "nodes edges",
    ] {
        let native_ids = ids(&fts.search(query, 10));
        let kv_ids: Vec<u64> = kv
            .search_fts(query, 10)
            .unwrap()
            .into_iter()
            .map(|h| h.node.id)
            .collect();
        assert_eq!(
            native_ids, kv_ids,
            "native FTS ranking diverged from KV search_fts for query {query:?}"
        );
    }
}
