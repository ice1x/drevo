//! Micro-benchmark + regression guard for the batched GraphML import
//! (`apply_dump` folds every record's index writes into one `put_batch`
//! transaction instead of one commit per trigram/index-entry).
//!
//! `#[ignore]`d: it opens real redb files on disk, so it is a manual benchmark
//! (redb fsync timing is noisy on shared CI runners — see the project's
//! slow-redb policy). Run with:
//!
//! ```text
//! cargo test --test import_perf_tests -- --ignored --nocapture
//! ```

#![cfg(feature = "redb-backend")]

use std::collections::HashMap;
use std::time::Instant;

use drevo::db::Drevo;
use drevo::model::{NewEdge, NewNode, Properties};

/// ~200 chars of varied words → many distinct trigrams per node, so FTS
/// indexing emits a lot of entries (the thing that used to fsync per entry).
fn text_body(i: usize) -> String {
    format!(
        "node {i} anxious thoughts deadlines mentoring graph vectors embeddings \
         semantic search relationships knowledge base entity {i} observation \
         lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod"
    )
}

#[test]
#[ignore = "disk/redb micro-benchmark; run manually with --ignored --nocapture"]
fn graphml_import_of_text_heavy_graph_is_fast() {
    let dir = tempfile::tempdir().unwrap();

    // Build a text-heavy source graph (create_* already batch, so this is fast).
    let src = Drevo::open(&dir.path().join("src.redb")).unwrap();
    const N: usize = 2000;
    let nodes: Vec<NewNode> = (0..N)
        .map(|i| NewNode {
            kind: "Entity".into(),
            title: format!("n{i}"),
            body: text_body(i),
            body_html: String::new(),
            properties: Properties(HashMap::new()),
        })
        .collect();
    let created = src.create_nodes(nodes).unwrap();
    let edges: Vec<NewEdge> = (0..N - 1)
        .map(|i| NewEdge {
            from_id: created[i].id,
            to_id: created[i + 1].id,
            kind: "NEXT".into(),
            weight: 1.0,
            properties: Properties(HashMap::new()),
        })
        .collect();
    src.create_edges(edges).unwrap();

    let graphml = src.export_graphml().unwrap();

    // Import into a FRESH redb database — this is the path `shrink`/`restore`
    // exercise, and the one that used to fsync per index entry.
    let dst = Drevo::open(&dir.path().join("dst.redb")).unwrap();
    let started = Instant::now();
    let report = dst.import_graphml(&graphml).unwrap();
    let elapsed = started.elapsed();

    eprintln!(
        "IMPORT {N} text-heavy nodes + {} edges into fresh redb: {elapsed:?}",
        N - 1
    );

    assert_eq!(report.nodes_imported, N);
    assert_eq!(report.edges_imported, N - 1);
    // Sanity: the data really landed and round-trips.
    assert_eq!(
        dst.get_node_by_title("n0").unwrap().unwrap().body,
        text_body(0)
    );
    assert!(dst.get_node_by_title("n1999").unwrap().is_some());
}
