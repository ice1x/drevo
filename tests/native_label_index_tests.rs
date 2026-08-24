//! The native secondary-label index (RFC `docs/rfc-native-core.md`, #307,
//! Phase 6.6) — a change-feed consumer that indexes the `_labels` Cypher labels
//! the primary-kind index does not cover.
//!
//! These lock: it indexes secondary labels in ascending id order, reflects
//! label add/remove and node deletion after `sync`, rebuilds when the feed was
//! trimmed past its cursor, and — the headline — makes a native
//! `MATCH (n:Label)` return the **same rows as the full-scan path and as the KV
//! store**, so it is a faithful, scan-free native label lookup.

use drevo::cypher::executor::{
    execute, execute_on_engine, execute_on_engine_with_indexes, ExecResult, Value,
};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::engine::GraphEngine;
use drevo::model::{NewNode, NodePatch, Properties};
use drevo::native::NativeGraph;
use drevo::native_label_index::NativeLabelIndex;
use std::collections::HashMap;

/// A node with a primary `kind` and zero or more secondary labels, stored the
/// way `SET n:Label` stores them (the reserved `_labels` JSON-array property).
fn labeled(kind: &str, title: &str, secondary: &[&str]) -> NewNode {
    let props = if secondary.is_empty() {
        Properties::default()
    } else {
        Properties(HashMap::from([(
            "_labels".to_string(),
            serde_json::json!(secondary),
        )]))
    };
    NewNode {
        kind: kind.into(),
        title: title.into(),
        body: String::new(),
        body_html: String::new(),
        properties: props,
    }
}

#[test]
fn indexes_secondary_labels_in_ascending_id_order() {
    let g = NativeGraph::new();
    let a = g
        .create_node(labeled("person", "a", &["employee"]))
        .unwrap();
    let b = g
        .create_node(labeled("person", "b", &["employee", "manager"]))
        .unwrap();
    let _c = g.create_node(labeled("city", "c", &[])).unwrap();

    let mut idx = NativeLabelIndex::new();
    idx.sync(&g);

    // Only nodes that carry a secondary label are tracked (c has none).
    assert_eq!(idx.len(), 2);
    assert!(!idx.is_empty());
    assert_eq!(idx.node_ids("employee"), vec![a.id, b.id]);
    assert_eq!(idx.node_ids("manager"), vec![b.id]);
    // The primary kind is NOT a secondary label — that stays with the engine's
    // own kind index.
    assert!(idx.node_ids("person").is_empty());
    assert!(idx.node_ids("ghost").is_empty());
}

#[test]
fn reflects_label_changes_and_deletes_after_sync() {
    let g = NativeGraph::new();
    let a = g
        .create_node(labeled("person", "a", &["employee"]))
        .unwrap();

    let mut idx = NativeLabelIndex::new();
    idx.sync(&g);
    assert_eq!(idx.node_ids("employee"), vec![a.id]);

    // Re-label a: employee -> contractor (a full property replace, as the
    // executor's SET does under the hood).
    g.update_node(
        a.id,
        NodePatch {
            properties: Some(Properties(HashMap::from([(
                "_labels".to_string(),
                serde_json::json!(["contractor"]),
            )]))),
            ..Default::default()
        },
    )
    .unwrap();
    idx.sync(&g);
    assert!(idx.node_ids("employee").is_empty());
    assert_eq!(idx.node_ids("contractor"), vec![a.id]);

    // Delete a: its bucket empties and the index goes empty.
    g.delete_node(a.id).unwrap();
    idx.sync(&g);
    assert!(idx.node_ids("contractor").is_empty());
    assert!(idx.is_empty());
}

#[test]
fn rebuilds_when_feed_trimmed_past_cursor() {
    let g = NativeGraph::new();
    let a = g
        .create_node(labeled("person", "a", &["employee"]))
        .unwrap();

    let mut idx = NativeLabelIndex::new();
    idx.sync(&g);
    assert_eq!(idx.node_ids("employee"), vec![a.id]);

    // The graph churns and the owner trims the feed past the index's cursor.
    let b = g
        .create_node(labeled("person", "b", &["employee"]))
        .unwrap();
    g.trim_before(g.change_head());

    // sync must notice the lag and rebuild from a fresh snapshot.
    idx.sync(&g);
    assert_eq!(idx.node_ids("employee"), vec![a.id, b.id]);
}

/// Collect the ids a `MATCH (n:Label) RETURN n` yields, sorted so the
/// comparison is order-insensitive (unordered Cypher results have no defined
/// order across engines / candidate strategies).
fn matched_ids(res: ExecResult) -> Vec<u64> {
    let mut ids: Vec<u64> = res
        .rows
        .iter()
        .filter_map(|row| match row.first() {
            Some(Value::Node(n)) => Some(n.id),
            _ => None,
        })
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn indexed_label_scan_equals_full_scan_and_kv() {
    // Build the identical graph, in the same order, on the native engine and
    // the KV store so node ids line up. Two nodes carry the secondary label
    // `employee` (one is also primary-kind `employee`), a mix that only a
    // union of the kind index and the label index covers.
    let build = |eng: &dyn GraphEngine| {
        eng.create_node(labeled("person", "a", &["employee"]))
            .unwrap();
        eng.create_node(labeled("employee", "b", &[])).unwrap(); // primary kind
        eng.create_node(labeled("person", "c", &["contractor"]))
            .unwrap();
        eng.create_node(labeled("employee", "d", &["employee"]))
            .unwrap(); // both
    };
    let native = NativeGraph::new();
    let kv = Drevo::open_in_memory().unwrap();
    build(&native);
    build(&kv);

    let mut idx = NativeLabelIndex::new();
    idx.sync(&native);

    let query = parse("MATCH (n:employee) RETURN n").unwrap();

    // Native full scan (no index), native indexed union, and KV — all three
    // must agree on exactly which nodes carry the label `employee`.
    let full_scan = matched_ids(execute_on_engine(&query, &native, HashMap::new()).unwrap());
    let indexed = matched_ids(
        execute_on_engine_with_indexes(&query, &native, None, Some(&idx), HashMap::new()).unwrap(),
    );
    let kv_rows = matched_ids(execute(&query, &kv, HashMap::new()).unwrap());

    assert_eq!(full_scan, indexed, "indexed union diverged from full scan");
    assert_eq!(indexed, kv_rows, "native indexed diverged from KV");
    // Sanity: it really found the three `employee`-labelled nodes (a, b, d),
    // not the contractor c.
    assert_eq!(indexed.len(), 3);
}
