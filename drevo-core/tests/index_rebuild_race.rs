//! Indexes that rebuild from a snapshot while commits land. An index rebuilds
//! from the graph when the change feed was trimmed past its cursor, and when
//! its specs are replaced; it then resumes from a cursor. A commit landing
//! during the rebuild must not be skipped: once the writer stops and the index
//! syncs again, it holds every node and relationship.

use std::sync::atomic::{AtomicBool, Ordering};

use drevo_core::labels::SECONDARY_LABELS_KEY;
use drevo_core::model::{NewEdge, NewNode, Properties};
use drevo_core::native::NativeGraph;
use drevo_core::native_fts::{NativeFtsIndex, NativeFtsRelIndex};
use drevo_core::native_label_index::NativeLabelIndex;
use drevo_core::native_path_index::{NativePathIndex, PathIndexSpec, PropertyPath};
use drevo_core::native_point_index::{NativePointIndex, Nearest, PointIndexSpec};
use drevo_core::native_property_index::NativePropertyIndex;
use drevo_core::native_text_index::{NativeTextIndex, TextIndexSpec, TextMatch};
use serde_json::{json, Value as Json};

const ROUNDS: usize = 30;
const NODES: u64 = 300;

/// Note `i`: also labelled `Memo`, with `k = i`, `meta.k = i`, a unique
/// `name`, and its own position.
fn note(i: u64) -> NewNode {
    let mut properties = Properties::new();
    properties.insert(SECONDARY_LABELS_KEY, json!(["Memo"]));
    properties.insert("k", Json::from(i));
    properties.insert("meta", json!({ "k": i }));
    properties.insert("name", Json::from(name(i)));
    let (lat, lon) = position(i);
    properties.insert("lat", Json::from(lat));
    properties.insert("lon", Json::from(lon));
    NewNode {
        kind: "Note".into(),
        title: format!("note {i}"),
        properties,
        body: String::new(),
        body_html: String::new(),
    }
}

fn name(i: u64) -> String {
    format!("n{i:05}z")
}

fn position(i: u64) -> (f64, f64) {
    (-75.0 + i as f64 * 0.5, (i % 7) as f64)
}

/// How the index is kept up to date while the writer runs.
enum Refresh {
    /// The feed is trimmed to its head before each sync, so most syncs find
    /// their cursor behind the feed and rebuild.
    Lagged,
    /// The specs are replaced, which rebuilds; nothing is trimmed.
    SetSpecs,
}

/// Commit `NODES` notes one by one, each with a relationship from a seed
/// note, while `step(graph, false)` keeps rebuilding the index; then
/// `step(graph, true)` once the writer is done, which syncs it.
fn rebuild_while_writing(refresh: Refresh, step: &mut dyn FnMut(&NativeGraph, bool)) {
    let graph = NativeGraph::new();
    let seed = graph.create_nodes(vec![note(NODES)]).expect("seed")[0].id;
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for i in 0..NODES {
                let id = graph.create_nodes(vec![note(i)]).expect("create")[0].id;
                graph
                    .create_edges(vec![NewEdge {
                        from_id: seed,
                        to_id: id,
                        kind: "links".into(),
                        weight: 1.0,
                        properties: Properties::new(),
                    }])
                    .expect("link");
            }
            done.store(true, Ordering::SeqCst);
        });
        while !done.load(Ordering::SeqCst) {
            if let Refresh::Lagged = refresh {
                graph.trim_before(graph.change_head());
            }
            step(&graph, false);
        }
    });
    step(&graph, true);
}

/// The notes `0..NODES` for which `found` is false.
fn missing(found: impl Fn(u64) -> bool) -> Vec<u64> {
    (0..NODES).filter(|i| !found(*i)).collect()
}

fn strings(s: &[&str]) -> Vec<String> {
    s.iter().map(|p| (*p).to_string()).collect()
}

fn path_spec() -> PathIndexSpec {
    PathIndexSpec {
        label: Some("Note".into()),
        path: PropertyPath::new(strings(&["meta", "k"]), false).expect("path"),
    }
}

fn text_spec() -> TextIndexSpec {
    TextIndexSpec::new(Some("Note".into()), strings(&["name"])).expect("spec")
}

fn point_spec() -> PointIndexSpec {
    PointIndexSpec::new("Note", "lat", "lon").expect("spec")
}

fn path_missing(index: &NativePathIndex) -> Vec<u64> {
    let note = strings(&["Note"]);
    let path = strings(&["meta", "k"]);
    missing(|i| {
        index
            .node_ids(&note, &path, &Json::from(i))
            .is_some_and(|ids| !ids.is_empty())
    })
}

fn text_missing(index: &NativeTextIndex) -> Vec<u64> {
    let note = strings(&["Note"]);
    let path = strings(&["name"]);
    missing(|i| {
        index
            .candidates(&note, &path, TextMatch::Contains, &name(i))
            .is_some_and(|ids| !ids.is_empty())
    })
}

fn point_missing(index: &NativePointIndex) -> Vec<u64> {
    let one = Nearest {
        k: 1,
        max_meters: Some(1.0),
    };
    missing(|i| {
        index
            .nearest("Note", "lat", "lon", position(i), &one, &mut |_| true)
            .is_some_and(|near| near.len() == 1)
    })
}

#[test]
fn a_label_index_rebuilt_during_commits_holds_every_node() {
    for round in 0..ROUNDS {
        let mut index = NativeLabelIndex::new();
        rebuild_while_writing(Refresh::Lagged, &mut |g, _| index.sync(g));
        // The seed note too.
        assert_eq!(
            index.node_ids("Memo").len() as u64,
            NODES + 1,
            "round {round}"
        );
    }
}

#[test]
fn a_property_index_rebuilt_during_commits_holds_every_node() {
    for round in 0..ROUNDS {
        let mut index = NativePropertyIndex::new();
        rebuild_while_writing(Refresh::Lagged, &mut |g, _| index.sync(g));
        let missing = missing(|i| !index.node_ids("k", &Json::from(i)).is_empty());
        assert!(missing.is_empty(), "round {round}: missing {missing:?}");
    }
}

#[test]
fn full_text_indexes_rebuilt_during_commits_hold_everything() {
    for round in 0..ROUNDS {
        let mut nodes = NativeFtsIndex::new();
        let mut rels = NativeFtsRelIndex::new();
        rebuild_while_writing(Refresh::Lagged, &mut |g, _| {
            nodes.sync(g);
            rels.sync(g);
        });
        assert_eq!(nodes.len() as u64, NODES + 1, "round {round}: nodes");
        assert_eq!(rels.len() as u64, NODES, "round {round}: relationships");
    }
}

fn path_index(refresh: Refresh) {
    for round in 0..ROUNDS {
        let mut index = NativePathIndex::new(vec![path_spec()]);
        let set_specs = matches!(refresh, Refresh::SetSpecs);
        rebuild_while_writing(refresh_of(set_specs), &mut |g, last| {
            if set_specs && !last {
                index.set_specs(vec![path_spec()], g);
            } else {
                index.sync(g);
            }
        });
        let missing = path_missing(&index);
        assert!(missing.is_empty(), "round {round}: missing {missing:?}");
    }
}

fn text_index(refresh: Refresh) {
    for round in 0..ROUNDS {
        let mut index = NativeTextIndex::new(vec![text_spec()]);
        let set_specs = matches!(refresh, Refresh::SetSpecs);
        rebuild_while_writing(refresh_of(set_specs), &mut |g, last| {
            if set_specs && !last {
                index.set_specs(vec![text_spec()], g);
            } else {
                index.sync(g);
            }
        });
        let missing = text_missing(&index);
        assert!(missing.is_empty(), "round {round}: missing {missing:?}");
    }
}

fn point_index(refresh: Refresh) {
    for round in 0..ROUNDS {
        let mut index = NativePointIndex::new(vec![point_spec()]);
        let set_specs = matches!(refresh, Refresh::SetSpecs);
        rebuild_while_writing(refresh_of(set_specs), &mut |g, last| {
            if set_specs && !last {
                index.set_specs(vec![point_spec()], g);
            } else {
                index.sync(g);
            }
        });
        let missing = point_missing(&index);
        assert!(missing.is_empty(), "round {round}: missing {missing:?}");
    }
}

fn refresh_of(set_specs: bool) -> Refresh {
    if set_specs {
        Refresh::SetSpecs
    } else {
        Refresh::Lagged
    }
}

#[test]
fn a_path_index_rebuilt_on_a_lagged_feed_holds_every_node() {
    path_index(Refresh::Lagged);
}

#[test]
fn a_path_index_given_new_specs_during_commits_holds_every_node() {
    path_index(Refresh::SetSpecs);
}

#[test]
fn a_text_index_rebuilt_on_a_lagged_feed_holds_every_node() {
    text_index(Refresh::Lagged);
}

#[test]
fn a_text_index_given_new_specs_during_commits_holds_every_node() {
    text_index(Refresh::SetSpecs);
}

#[test]
fn a_point_index_rebuilt_on_a_lagged_feed_holds_every_node() {
    point_index(Refresh::Lagged);
}

#[test]
fn a_point_index_given_new_specs_during_commits_holds_every_node() {
    point_index(Refresh::SetSpecs);
}
