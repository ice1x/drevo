//! Trigram text indexes for substring predicates (issue #589).
//!
//! A [`NativeTextIndex`] splits the string at a declared property path into
//! case-sensitive character trigrams and answers `CONTAINS`, `STARTS WITH` and
//! `ENDS WITH` lookups with a superset of the matching node ids, or `None`
//! when it cannot narrow (no covering spec, a needle too short, or a
//! non-string value somewhere on the path).

use std::collections::{BTreeSet, HashMap};

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewNode, NodePatch, Properties};
use drevo_core::native::NativeGraph;
use drevo_core::native_text_index::{NativeTextIndex, TextIndexSpec, TextMatch};
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

fn strings(s: &[&str]) -> Vec<String> {
    s.iter().map(|p| (*p).to_string()).collect()
}

fn spec(label: Option<&str>, path: &[&str]) -> TextIndexSpec {
    TextIndexSpec::new(label.map(str::to_string), strings(path)).expect("valid spec")
}

fn ids(v: &[u64]) -> Option<BTreeSet<u64>> {
    Some(v.iter().copied().collect())
}

fn set_props(g: &NativeGraph, id: u64, props: Value) {
    let Value::Object(map) = props else {
        panic!("props must be an object")
    };
    g.update_node(
        id,
        NodePatch {
            properties: Some(Properties(map.into_iter().collect())),
            ..NodePatch::default()
        },
    )
    .expect("update node");
}

/// Bug tracker: bug titles searched by substring.
fn bug_tracker() -> (NativeGraph, u64, u64, u64) {
    let g = NativeGraph::new();
    let crash = node(&g, "Bug", "crash on save", json!({}));
    let button = node(&g, "Bug", "Save button misaligned", json!({}));
    let autosave = node(&g, "Bug", "autosave loses edits", json!({}));
    (g, crash, button, autosave)
}

#[test]
fn contains_is_case_sensitive() {
    let (g, crash, button, autosave) = bug_tracker();
    let mut idx = NativeTextIndex::new(vec![spec(Some("Bug"), &["title"])]);
    idx.sync(&g);
    let bug = strings(&["Bug"]);
    let title = strings(&["title"]);
    assert_eq!(
        idx.candidates(&bug, &title, TextMatch::Contains, "save"),
        ids(&[crash, autosave])
    );
    assert_eq!(
        idx.candidates(&bug, &title, TextMatch::Contains, "Save"),
        ids(&[button])
    );
    assert_eq!(
        idx.candidates(&bug, &title, TextMatch::Contains, "nothing like it"),
        ids(&[])
    );
}

#[test]
fn starts_and_ends_with_narrow_even_for_short_needles() {
    let g = NativeGraph::new();
    let abc = node(&g, "Tag", "tag abc", json!({"code": "abc"}));
    let cab = node(&g, "Tag", "tag cab", json!({"code": "cab"}));
    let ab = node(&g, "Tag", "tag ab", json!({"code": "ab"}));
    let a = node(&g, "Tag", "tag a", json!({"code": "a"}));
    let mut idx = NativeTextIndex::new(vec![spec(None, &["code"])]);
    idx.sync(&g);
    let code = strings(&["code"]);
    let starts = |n: &str| idx.candidates(&[], &code, TextMatch::StartsWith, n);
    let ends = |n: &str| idx.candidates(&[], &code, TextMatch::EndsWith, n);
    assert_eq!(starts("ab"), ids(&[abc, ab]));
    assert_eq!(starts("a"), ids(&[abc, ab, a]));
    assert_eq!(starts("abc"), ids(&[abc]));
    assert_eq!(ends("ab"), ids(&[cab, ab]));
    assert_eq!(ends("b"), ids(&[cab, ab]));
    assert_eq!(ends("c"), ids(&[abc]));
}

#[test]
fn needles_the_trigrams_cannot_serve_decline() {
    let (g, ..) = bug_tracker();
    let mut idx = NativeTextIndex::new(vec![spec(None, &["title"])]);
    idx.sync(&g);
    let title = strings(&["title"]);
    // Fewer than three characters cannot form an unanchored trigram.
    assert_eq!(idx.candidates(&[], &title, TextMatch::Contains, "sa"), None);
    // The empty string is contained in, starts and ends every string.
    for op in [
        TextMatch::Contains,
        TextMatch::StartsWith,
        TextMatch::EndsWith,
    ] {
        assert_eq!(idx.candidates(&[], &title, op, ""), None);
    }
}

#[test]
fn a_labelled_spec_covers_only_patterns_requiring_the_label() {
    let g = NativeGraph::new();
    let bug = node(&g, "Bug", "login broken", json!({}));
    let task = node(&g, "Task", "fix login", json!({}));
    let tagged = node(&g, "Task", "login audit", json!({"_labels": ["Bug"]}));
    let mut idx = NativeTextIndex::new(vec![spec(Some("Bug"), &["title"])]);
    idx.sync(&g);
    let title = strings(&["title"]);
    assert_eq!(
        idx.candidates(&strings(&["Bug"]), &title, TextMatch::Contains, "login"),
        ids(&[bug, tagged])
    );
    // Without the label the index has not seen every node (`task`).
    assert_eq!(
        idx.candidates(&[], &title, TextMatch::Contains, "login"),
        None
    );
    assert_eq!(
        idx.candidates(&strings(&["Task"]), &title, TextMatch::Contains, "login"),
        None
    );
    let _ = task;
}

#[test]
fn an_unindexed_path_declines() {
    let (g, ..) = bug_tracker();
    let mut idx = NativeTextIndex::new(vec![spec(None, &["title"])]);
    idx.sync(&g);
    assert_eq!(
        idx.candidates(&[], &strings(&["body"]), TextMatch::Contains, "save"),
        None
    );
}

#[test]
fn a_non_string_value_blocks_the_index_until_it_is_gone() {
    // `n.code CONTAINS 'x'` on an integer is a type error; narrowing would
    // hide it, so the index declines while such a value exists.
    let g = NativeGraph::new();
    let s = node(&g, "Item", "a", json!({"code": "SKU-100"}));
    let n = node(&g, "Item", "b", json!({"code": 100}));
    let mut idx = NativeTextIndex::new(vec![spec(None, &["code"])]);
    idx.sync(&g);
    let code = strings(&["code"]);
    assert_eq!(idx.candidates(&[], &code, TextMatch::Contains, "100"), None);

    set_props(&g, n, json!({"code": "SKU-200"}));
    idx.sync(&g);
    assert_eq!(
        idx.candidates(&[], &code, TextMatch::Contains, "SKU"),
        ids(&[s, n])
    );

    set_props(&g, n, json!({"code": [1, 2]}));
    idx.sync(&g);
    assert_eq!(idx.candidates(&[], &code, TextMatch::Contains, "SKU"), None);

    g.delete_node(n).expect("delete");
    idx.sync(&g);
    assert_eq!(
        idx.candidates(&[], &code, TextMatch::Contains, "SKU"),
        ids(&[s])
    );
}

#[test]
fn null_and_missing_values_do_not_block() {
    // `null CONTAINS 'x'` is null, not an error: such rows filter out anyway.
    let g = NativeGraph::new();
    let s = node(&g, "Item", "a", json!({"code": "SKU-1"}));
    node(&g, "Item", "b", json!({"code": null}));
    node(&g, "Item", "c", json!({}));
    let mut idx = NativeTextIndex::new(vec![spec(None, &["code"])]);
    idx.sync(&g);
    assert_eq!(
        idx.candidates(&[], &strings(&["code"]), TextMatch::Contains, "SKU"),
        ids(&[s])
    );
}

#[test]
fn nested_paths_are_indexed_and_a_scalar_on_the_way_blocks() {
    let g = NativeGraph::new();
    let ann = node(&g, "Doc", "a", json!({"meta": {"author": "Anna Karenina"}}));
    node(&g, "Doc", "b", json!({"meta": {"editor": "Anna"}}));
    let flat = node(&g, "Doc", "c", json!({"meta": "draft"}));
    let mut idx = NativeTextIndex::new(vec![spec(None, &["meta", "author"])]);
    idx.sync(&g);
    let author = strings(&["meta", "author"]);
    // `n.meta.author` on a string `meta` is an error: decline.
    assert_eq!(
        idx.candidates(&[], &author, TextMatch::Contains, "Anna"),
        None
    );
    set_props(&g, flat, json!({"meta": null}));
    idx.sync(&g);
    assert_eq!(
        idx.candidates(&[], &author, TextMatch::Contains, "Anna"),
        ids(&[ann])
    );
}

#[test]
fn title_and_body_read_the_node_fields() {
    // Cypher shows a node's `title` / `body` as properties unless the
    // property map has its own entry of that name.
    let g = NativeGraph::new();
    let field = node(&g, "Note", "meeting notes", json!({}));
    let shadowed = node(&g, "Note", "ignored", json!({"title": "release notes"}));
    let mut idx = NativeTextIndex::new(vec![spec(None, &["title"])]);
    idx.sync(&g);
    let title = strings(&["title"]);
    assert_eq!(
        idx.candidates(&[], &title, TextMatch::Contains, "notes"),
        ids(&[field, shadowed])
    );
    assert_eq!(
        idx.candidates(&[], &title, TextMatch::Contains, "ignored"),
        ids(&[])
    );
}

#[test]
fn non_ascii_text_is_matched_by_characters() {
    let g = NativeGraph::new();
    let tree = node(&g, "Entry", "новогодняя ёлка", json!({}));
    node(&g, "Entry", "полка", json!({}));
    let mut idx = NativeTextIndex::new(vec![spec(None, &["title"])]);
    idx.sync(&g);
    let title = strings(&["title"]);
    assert_eq!(
        idx.candidates(&[], &title, TextMatch::Contains, "ёлк"),
        ids(&[tree])
    );
    assert_eq!(
        idx.candidates(&[], &title, TextMatch::EndsWith, "ёлка"),
        ids(&[tree])
    );
}

#[test]
fn a_trimmed_change_feed_triggers_a_rebuild() {
    let g = NativeGraph::new();
    let first = node(&g, "Bug", "first crash", json!({}));
    let mut idx = NativeTextIndex::new(vec![spec(None, &["title"])]);
    idx.sync(&g);
    let second = node(&g, "Bug", "second crash", json!({}));
    g.trim_before(g.change_head());
    idx.sync(&g);
    assert_eq!(
        idx.candidates(&[], &strings(&["title"]), TextMatch::Contains, "crash"),
        ids(&[first, second])
    );
    assert_eq!(idx.cursor(), g.change_head());
}

#[test]
fn set_specs_rebuilds_over_the_new_definitions() {
    let (g, crash, _, autosave) = bug_tracker();
    let mut idx = NativeTextIndex::new(Vec::new());
    idx.sync(&g);
    let title = strings(&["title"]);
    assert_eq!(
        idx.candidates(&[], &title, TextMatch::Contains, "save"),
        None
    );
    idx.set_specs(vec![spec(None, &["title"])], &g);
    assert_eq!(
        idx.candidates(&[], &title, TextMatch::Contains, "save"),
        ids(&[crash, autosave])
    );
    assert_eq!(idx.specs().len(), 1);
}

#[test]
fn spec_paths_must_be_non_empty() {
    assert!(TextIndexSpec::new(None, Vec::new()).is_err());
    assert!(TextIndexSpec::new(None, strings(&["meta", ""])).is_err());
    let s = spec(Some("Doc"), &["meta", "author"]);
    assert_eq!(s.label(), Some("Doc"));
    assert_eq!(s.path(), strings(&["meta", "author"]).as_slice());
}

/// Every lookup is a superset of the exact answer, over random strings from a
/// small alphabet (so substrings, prefixes and suffixes collide often).
#[test]
fn lookups_never_miss_a_match() {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move |bound: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % bound
    };
    let alphabet = ['a', 'b', 'c', 'ё', ' '];
    let word = |next: &mut dyn FnMut(u64) -> u64, max: u64| -> String {
        let len = next(max);
        (0..len)
            .map(|_| alphabet[next(alphabet.len() as u64) as usize])
            .collect()
    };
    let g = NativeGraph::new();
    let mut values: Vec<(u64, String)> = Vec::new();
    for i in 0..200 {
        let v = word(&mut next, 9);
        let id = node(&g, "Item", &format!("item {i}"), json!({ "code": v }));
        values.push((id, v));
    }
    let mut idx = NativeTextIndex::new(vec![spec(None, &["code"])]);
    idx.sync(&g);
    let code = strings(&["code"]);
    for _ in 0..500 {
        let needle = word(&mut next, 5);
        for op in [
            TextMatch::Contains,
            TextMatch::StartsWith,
            TextMatch::EndsWith,
        ] {
            let Some(got) = idx.candidates(&[], &code, op, &needle) else {
                continue;
            };
            for (id, v) in &values {
                let hit = match op {
                    TextMatch::Contains => v.contains(&needle),
                    TextMatch::StartsWith => v.starts_with(&needle),
                    TextMatch::EndsWith => v.ends_with(&needle),
                };
                assert!(!hit || got.contains(id), "{op:?} {needle:?} missed {v:?}");
            }
        }
    }
}

#[test]
fn a_needle_most_values_share_declines_on_a_large_index() {
    // When every trigram of the needle sits in more than half of the indexed
    // values, intersecting the postings costs more than scanning.
    let g = NativeGraph::new();
    let mut rare = Vec::new();
    for i in 0..3000 {
        let id = node(
            &g,
            "Ticket",
            &format!("T-{i}"),
            json!({ "summary": format!("issue {i}") }),
        );
        if i % 1000 == 7 {
            rare.push(id);
        }
    }
    let mut idx = NativeTextIndex::new(vec![spec(None, &["summary"])]);
    idx.sync(&g);
    let summary = strings(&["summary"]);
    assert_eq!(
        idx.candidates(&[], &summary, TextMatch::Contains, "issue"),
        None
    );
    assert_eq!(
        idx.candidates(&[], &summary, TextMatch::StartsWith, "iss"),
        None
    );
    // One rare trigram is enough to narrow: `issue 1007`, `issue 2007`.
    assert_eq!(
        idx.candidates(&[], &summary, TextMatch::EndsWith, "007"),
        Some(BTreeSet::from([rare[1], rare[2]]))
    );
}
