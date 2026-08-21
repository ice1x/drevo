//! Guards for the `GraphEngine` seam (RFC `docs/rfc-native-core.md`, #307).
//!
//! The trait is introduced **additively** — `Drevo` implements it by delegating
//! to its existing inherent methods, so there is zero behaviour change. These
//! tests pin that (a) the seam exists and is object-safe, (b) driving a `Drevo`
//! purely through `&dyn GraphEngine` reproduces the same graph CRUD + traversal
//! behaviour as the inherent API. That gives a future native `drevo-core`
//! engine an executable contract to match.

use drevo::db::Drevo;
use drevo::engine::GraphEngine;
use drevo::model::{Direction, NewEdge, NewNode, NodePatch};

fn node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.into(),
        title: title.into(),
        body: String::new(),
        body_html: String::new(),
        properties: Default::default(),
    }
}

fn edge(from: u64, to: u64, kind: &str) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.into(),
        weight: 1.0,
        properties: Default::default(),
    }
}

#[test]
fn drevo_is_usable_purely_through_the_graph_engine_seam() {
    let db = Drevo::open_in_memory().unwrap();
    let engine: &dyn GraphEngine = &db;

    let a = engine.create_node(node("note", "a")).unwrap();
    let b = engine.create_node(node("note", "b")).unwrap();
    let c = engine.create_node(node("note", "c")).unwrap();

    engine.create_edge(edge(a.id, b.id, "links_to")).unwrap();
    engine.create_edge(edge(a.id, c.id, "tagged_with")).unwrap();

    // get_node round-trips through the trait.
    assert_eq!(engine.get_node(a.id).unwrap().unwrap().title, "a");
    assert!(engine.get_node(999_999).unwrap().is_none());

    // neighbor_ids honours direction …
    let mut outs = engine
        .neighbor_ids(a.id, Direction::Outgoing, None)
        .unwrap();
    outs.sort_unstable();
    let mut expect = vec![b.id, c.id];
    expect.sort_unstable();
    assert_eq!(outs, expect);

    // … and the kind filter.
    let kinded = engine
        .neighbor_ids(a.id, Direction::Outgoing, Some("links_to"))
        .unwrap();
    assert_eq!(kinded, vec![b.id]);

    // neighbors returns full nodes.
    let ns = engine
        .neighbors(a.id, Direction::Outgoing, Some("tagged_with"))
        .unwrap();
    assert_eq!(ns.len(), 1);
    assert_eq!(ns[0].title, "c");
}

#[test]
fn seam_matches_inherent_api_for_update_and_delete() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(node("note", "orig")).unwrap();

    // Update through the trait; the inherent API observes the same state.
    let engine: &dyn GraphEngine = &db;
    let patched = engine
        .update_node(
            a.id,
            NodePatch {
                title: Some("renamed".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(patched.title, "renamed");
    assert_eq!(db.get_node(a.id).unwrap().unwrap().title, "renamed");

    let b = db.create_node(node("note", "b")).unwrap();
    let e = engine.create_edge(edge(a.id, b.id, "links_to")).unwrap();
    assert!(engine.get_edge(e.id).unwrap().is_some());
    engine.delete_edge(e.id).unwrap();
    assert!(engine.get_edge(e.id).unwrap().is_none());

    engine.delete_node(b.id).unwrap();
    assert!(db.get_node(b.id).unwrap().is_none());
}
