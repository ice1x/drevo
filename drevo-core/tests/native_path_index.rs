//! Opt-in indexes on nested property paths (issue #578).
//!
//! A [`NativePathIndex`] indexes values *inside* map properties — one declared
//! path (`meta.author`), every path under a map (`meta.*`), or the whole
//! property map (`*`) — optionally only for nodes with a given label. Lookups
//! return a superset of the matching node ids, or `None` when no declared index
//! covers the path (the caller then scans).

use std::collections::{BTreeSet, HashMap};

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewNode, NodePatch, Properties};
use drevo_core::native::NativeGraph;
use drevo_core::native_path_index::{NativePathIndex, PathIndexSpec, PropertyPath};
use drevo_core::native_property_index::RangeOp;
use serde_json::{json, Value};

fn node(g: &NativeGraph, kind: &str, title: &str, props: Value) -> u64 {
    let Value::Object(map) = props else {
        panic!("props must be an object")
    };
    g.create_node(NewNode {
        kind: kind.into(),
        title: title.into(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(map.into_iter().collect::<HashMap<_, _>>()),
    })
    .expect("create node")
    .id
}

fn path(s: &[&str]) -> Vec<String> {
    s.iter().map(|p| (*p).to_string()).collect()
}

fn labels(s: &[&str]) -> Vec<String> {
    path(s)
}

fn spec(label: Option<&str>, segments: &[&str], wildcard: bool) -> PathIndexSpec {
    PathIndexSpec {
        label: label.map(str::to_string),
        path: PropertyPath::new(path(segments), wildcard).expect("valid path"),
    }
}

/// Bug tracker: bugs carry `meta: {severity, assignee: {name}}`.
fn bug_tracker() -> (NativeGraph, u64, u64, u64) {
    let g = NativeGraph::new();
    let crash = node(
        &g,
        "Bug",
        "crash on save",
        json!({"meta": {"severity": "high", "assignee": {"name": "ann"}, "points": 8}}),
    );
    let typo = node(
        &g,
        "Bug",
        "typo in footer",
        json!({"meta": {"severity": "low", "assignee": {"name": "bob"}, "points": 1}}),
    );
    let feature = node(
        &g,
        "Feature",
        "dark mode",
        json!({"meta": {"severity": "high", "points": 5}}),
    );
    (g, crash, typo, feature)
}

#[test]
fn exact_path_equality() {
    let (g, crash, _, feature) = bug_tracker();
    let mut idx = NativePathIndex::new(vec![spec(None, &["meta", "severity"], false)]);
    idx.sync(&g);
    assert_eq!(
        idx.node_ids(&[], &path(&["meta", "severity"]), &json!("high")),
        Some(vec![crash, feature])
    );
    assert_eq!(
        idx.node_ids(&[], &path(&["meta", "severity"]), &json!("none")),
        Some(vec![])
    );
}

#[test]
fn uncovered_path_is_none_so_the_caller_scans() {
    let (g, ..) = bug_tracker();
    let mut idx = NativePathIndex::new(vec![spec(None, &["meta", "severity"], false)]);
    idx.sync(&g);
    assert_eq!(
        idx.node_ids(&[], &path(&["meta", "points"]), &json!(8)),
        None
    );
    assert_eq!(
        idx.node_ids(&[], &path(&["meta", "assignee", "name"]), &json!("ann")),
        None
    );
}

#[test]
fn wildcard_covers_every_nested_path() {
    let (g, crash, typo, _) = bug_tracker();
    let mut idx = NativePathIndex::new(vec![spec(None, &["meta"], true)]);
    idx.sync(&g);
    assert_eq!(
        idx.node_ids(&[], &path(&["meta", "assignee", "name"]), &json!("bob")),
        Some(vec![typo])
    );
    assert_eq!(
        idx.node_ids(&[], &path(&["meta", "points"]), &json!(8)),
        Some(vec![crash])
    );
    // The map itself is not a leaf of `meta.*`.
    assert_eq!(idx.node_ids(&[], &path(&["meta"]), &json!("x")), None);
}

#[test]
fn whole_map_wildcard_covers_any_nested_path() {
    let (g, crash, ..) = bug_tracker();
    let mut idx = NativePathIndex::new(vec![spec(None, &[], true)]);
    idx.sync(&g);
    assert_eq!(
        idx.node_ids(&[], &path(&["meta", "assignee", "name"]), &json!("ann")),
        Some(vec![crash])
    );
}

#[test]
fn labelled_index_serves_only_patterns_requiring_that_label() {
    let (g, crash, _, _) = bug_tracker();
    let mut idx = NativePathIndex::new(vec![spec(Some("Bug"), &["meta", "severity"], false)]);
    idx.sync(&g);
    // `MATCH (n:Bug) WHERE n.meta.severity = 'high'` — served, Bugs only.
    assert_eq!(
        idx.node_ids(
            &labels(&["Bug"]),
            &path(&["meta", "severity"]),
            &json!("high")
        ),
        Some(vec![crash])
    );
    // `MATCH (n) WHERE …` could match the Feature too: the index cannot serve it.
    assert_eq!(
        idx.node_ids(&[], &path(&["meta", "severity"]), &json!("high")),
        None
    );
}

#[test]
fn secondary_labels_count_for_a_labelled_index() {
    let g = NativeGraph::new();
    let id = node(
        &g,
        "Bug",
        "old crash",
        json!({"_labels": ["Archived"], "meta": {"severity": "high"}}),
    );
    let mut idx = NativePathIndex::new(vec![spec(Some("Archived"), &["meta"], true)]);
    idx.sync(&g);
    assert_eq!(
        idx.node_ids(
            &labels(&["Archived"]),
            &path(&["meta", "severity"]),
            &json!("high")
        ),
        Some(vec![id])
    );
}

#[test]
fn numeric_ranges_on_a_nested_path() {
    // CBT journal: entries carry `mood: {score}`.
    let g = NativeGraph::new();
    let low = node(&g, "Entry", "monday", json!({"mood": {"score": 2}}));
    let mid = node(&g, "Entry", "tuesday", json!({"mood": {"score": 5.5}}));
    let high = node(&g, "Entry", "friday", json!({"mood": {"score": 9}}));
    let mut idx = NativePathIndex::new(vec![spec(Some("Entry"), &["mood", "score"], false)]);
    idx.sync(&g);
    let got = idx
        .range_ids(
            &labels(&["Entry"]),
            &path(&["mood", "score"]),
            RangeOp::Ge,
            &json!(5),
        )
        .expect("covered");
    assert!(got.contains(&mid) && got.contains(&high), "{got:?}");
    assert!(!got.contains(&low), "{got:?}");
}

#[test]
fn updates_and_deletes_follow_the_change_feed() {
    let (g, crash, typo, _) = bug_tracker();
    let mut idx = NativePathIndex::new(vec![spec(Some("Bug"), &["meta"], true)]);
    idx.sync(&g);
    let p = path(&["meta", "assignee", "name"]);

    // Reassign the crash to bob.
    g.update_node(
        crash,
        NodePatch {
            properties: Some(Properties(HashMap::from([(
                "meta".to_string(),
                json!({"severity": "high", "assignee": {"name": "bob"}}),
            )]))),
            ..NodePatch::default()
        },
    )
    .expect("update");
    g.delete_node(typo).expect("delete");
    idx.sync(&g);

    assert_eq!(
        idx.node_ids(&labels(&["Bug"]), &p, &json!("bob")),
        Some(vec![crash])
    );
    assert_eq!(
        idx.node_ids(&labels(&["Bug"]), &p, &json!("ann")),
        Some(vec![])
    );
}

#[test]
fn lists_are_leaves_and_maps_are_not_indexed_as_values() {
    let g = NativeGraph::new();
    node(&g, "Doc", "a", json!({"meta": {"tags": ["x", "y"]}}));
    let mut idx = NativePathIndex::new(vec![spec(None, &["meta"], true)]);
    idx.sync(&g);
    // A list value is not an indexable scalar: no false "match", and the path
    // is still covered (an empty, exact answer for scalar probes).
    assert_eq!(
        idx.node_ids(&[], &path(&["meta", "tags"]), &json!("x")),
        Some(vec![])
    );
    // Nothing below a list is a path.
    assert_eq!(
        idx.node_ids(&[], &path(&["meta", "tags", "0"]), &json!("x")),
        Some(vec![])
    );
}

#[test]
fn keys_containing_dots_do_not_collide() {
    let g = NativeGraph::new();
    let dotted = node(&g, "Doc", "a", json!({"a.b": {"c": 1}}));
    let nested = node(&g, "Doc", "b", json!({"a": {"b.c": 1}}));
    let mut idx = NativePathIndex::new(vec![spec(None, &[], true)]);
    idx.sync(&g);
    assert_eq!(
        idx.node_ids(&[], &path(&["a.b", "c"]), &json!(1)),
        Some(vec![dotted])
    );
    assert_eq!(
        idx.node_ids(&[], &path(&["a", "b.c"]), &json!(1)),
        Some(vec![nested])
    );
}

#[test]
fn replacing_the_specs_rebuilds() {
    let (g, crash, _, feature) = bug_tracker();
    let mut idx = NativePathIndex::new(vec![]);
    idx.sync(&g);
    assert_eq!(
        idx.node_ids(&[], &path(&["meta", "severity"]), &json!("high")),
        None
    );
    idx.set_specs(vec![spec(None, &["meta", "severity"], false)], &g);
    assert_eq!(
        idx.node_ids(&[], &path(&["meta", "severity"]), &json!("high")),
        Some(vec![crash, feature])
    );
}

#[test]
fn rebuilds_when_the_feed_was_trimmed_past_the_cursor() {
    let (g, ..) = bug_tracker();
    let mut idx = NativePathIndex::new(vec![spec(None, &["meta", "severity"], false)]);
    idx.sync(&g);
    let late = node(&g, "Bug", "late", json!({"meta": {"severity": "high"}}));
    g.trim_before(g.change_head());
    idx.sync(&g);
    let got: BTreeSet<u64> = idx
        .node_ids(&[], &path(&["meta", "severity"]), &json!("high"))
        .expect("covered")
        .into_iter()
        .collect();
    assert!(got.contains(&late), "{got:?}");
}

#[test]
fn property_path_validation_and_display() {
    assert!(
        PropertyPath::new(vec![], false).is_err(),
        "empty exact path"
    );
    assert!(
        PropertyPath::new(path(&["a", ""]), false).is_err(),
        "empty segment"
    );
    let p = PropertyPath::new(path(&["meta", "author"]), false).unwrap();
    assert_eq!(p.to_string(), "meta.author");
    assert!(p.is_nested());
    let w = PropertyPath::new(path(&["meta"]), true).unwrap();
    assert_eq!(w.to_string(), "meta.*");
    assert_eq!(PropertyPath::new(vec![], true).unwrap().to_string(), "*");
    let top = PropertyPath::new(path(&["title"]), false).unwrap();
    assert!(
        !top.is_nested(),
        "a single segment is the always-on top-level index"
    );
    let dotted = PropertyPath::new(path(&["a.b", "c"]), false).unwrap();
    assert_eq!(dotted.to_string(), "`a.b`.c");
}

#[test]
fn a_path_holding_a_map_or_string_is_not_range_served() {
    // A range over a path where some node stores a non-number must fall back
    // to the exact filter (which raises drevo's type error), not answer `[]`.
    let g = NativeGraph::new();
    node(&g, "Doc", "a", json!({"meta": {"size": 3}}));
    node(&g, "Doc", "b", json!({"meta": {"size": {"w": 1}}}));
    let mut idx = NativePathIndex::new(vec![spec(None, &["meta"], true)]);
    idx.sync(&g);
    assert_eq!(
        idx.range_ids(&[], &path(&["meta", "size"]), RangeOp::Gt, &json!(1)),
        None
    );
}
