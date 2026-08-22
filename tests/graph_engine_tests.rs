//! Guards for the `GraphEngine` seam (RFC `docs/rfc-native-core.md`, #307).
//!
//! The trait is introduced **additively** — `Drevo` implements it by delegating
//! to its existing inherent methods, so there is zero behaviour change. These
//! tests pin that (a) the seam exists and is object-safe, (b) driving a `Drevo`
//! purely through `&dyn GraphEngine` reproduces the same graph CRUD + traversal
//! behaviour as the inherent API. That gives a future native `drevo-core`
//! engine an executable contract to match.

use std::collections::HashMap;

use drevo::cypher::executor::{execute, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::engine::GraphEngine;
use drevo::model::{Direction, EdgePatch, NewEdge, NewNode, NodePatch};

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

// ---------------------------------------------------------------------------
// Phase 1.2 — the Cypher executor's node/edge read paths flow through the seam.
//
// After routing `Executor::get_node` / `get_edge` call sites through the
// `GraphEngine` accessor, a MATCH that resolves a node, expands a relationship
// and loads the far node must still return the correct data. This is the
// behaviour guard for that refactor (the whole cypher_* corpus is the broader
// net; this pins the specific read paths that were re-routed).
// ---------------------------------------------------------------------------

#[test]
fn executor_read_paths_resolve_nodes_and_edges_through_the_seam() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(node("person", "alice")).unwrap();
    let b = db.create_node(node("person", "bob")).unwrap();
    db.create_edge(edge(a.id, b.id, "KNOWS")).unwrap();

    // MATCH resolves `a` (get_node), expands the relationship, loads `b`
    // (get_node) and materialises the edge (get_edge) for `type(r)`.
    let q = parse("MATCH (x)-[r]->(y) RETURN x.title, type(r), y.title").unwrap();
    let res = execute(&q, &db, HashMap::new()).unwrap();

    assert_eq!(res.rows.len(), 1);
    let row = &res.rows[0];
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    };
    assert_eq!(text(&row[0]), "alice");
    assert_eq!(text(&row[1]), "KNOWS");
    assert_eq!(text(&row[2]), "bob");
}

// ---------------------------------------------------------------------------
// Phase 1.3 — the Cypher executor's node/edge write paths flow through the seam.
//
// CREATE (create_node + create_edge), SET (update_node) and DELETE
// (delete_edge + delete_node) must still mutate the store correctly after the
// write call sites are routed through the `GraphEngine` accessor.
// ---------------------------------------------------------------------------

#[test]
fn executor_write_paths_create_update_delete_through_the_seam() {
    let db = Drevo::open_in_memory().unwrap();

    // CREATE → create_node ×2 + create_edge ×1.
    let q = parse("CREATE (a:person {title: 'ann'})-[:KNOWS]->(b:person {title: 'ben'})").unwrap();
    execute(&q, &db, HashMap::new()).unwrap();

    let ann = db.get_node_by_title("ann").unwrap().expect("ann exists");
    let ben = db.get_node_by_title("ben").unwrap().expect("ben exists");
    assert_eq!(
        db.neighbor_ids(ann.id, Direction::Outgoing, Some("KNOWS"))
            .unwrap(),
        vec![ben.id]
    );

    // SET → update_node.
    let q = parse("MATCH (a {title: 'ann'}) SET a.body = 'hello'").unwrap();
    execute(&q, &db, HashMap::new()).unwrap();
    assert_eq!(db.get_node(ann.id).unwrap().unwrap().body, "hello");

    // DELETE → delete the relationship, then the nodes.
    let q = parse("MATCH (a {title: 'ann'})-[r]->(b) DELETE r").unwrap();
    execute(&q, &db, HashMap::new()).unwrap();
    assert!(db
        .neighbor_ids(ann.id, Direction::Outgoing, None)
        .unwrap()
        .is_empty());

    let q = parse("MATCH (n:person) DELETE n").unwrap();
    execute(&q, &db, HashMap::new()).unwrap();
    assert!(db.get_node(ann.id).unwrap().is_none());
    assert!(db.get_node(ben.id).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Phase 1.4 — update_edge on the seam.
//
// The seam now carries edge updates. Assert both the direct trait call and a
// Cypher `SET r.<prop>` (which the executor routes through the seam) mutate
// the edge.
// ---------------------------------------------------------------------------

#[test]
fn update_edge_through_the_seam_direct_and_via_cypher() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(node("person", "u")).unwrap();
    let b = db.create_node(node("person", "v")).unwrap();
    let e = db.create_edge(edge(a.id, b.id, "LINKS")).unwrap();

    // Direct: bump the weight through &dyn GraphEngine.
    let engine: &dyn GraphEngine = &db;
    let updated = engine
        .update_edge(
            e.id,
            EdgePatch {
                weight: Some(2.5),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(updated.weight, 2.5);
    assert_eq!(db.get_edge(e.id).unwrap().unwrap().weight, 2.5);

    // Via Cypher: SET on the relationship property flows through the executor's
    // update_edge site (now on the seam).
    let q = parse("MATCH ()-[r:LINKS]->() SET r.note = 'x' RETURN r").unwrap();
    execute(&q, &db, HashMap::new()).unwrap();
    let stored = db.get_edge(e.id).unwrap().unwrap();
    assert_eq!(
        stored.properties.get("note").and_then(|v| v.as_str()),
        Some("x")
    );
}

// ---------------------------------------------------------------------------
// Phase 1.5 — read-scan surface on the seam.
//
// Full scans (all_nodes / all_edges), the kind (label) scan, and full-edge
// expansion (edges_of) now go through GraphEngine. Assert both the direct
// trait calls and the Cypher forms that the executor routes onto them.
// ---------------------------------------------------------------------------

#[test]
fn scans_and_edges_of_through_the_seam() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(node("person", "p1")).unwrap();
    let b = db.create_node(node("person", "p2")).unwrap();
    let t = db.create_node(node("tag", "t1")).unwrap();
    db.create_edge(edge(a.id, b.id, "KNOWS")).unwrap();
    db.create_edge(edge(a.id, t.id, "TAGGED")).unwrap();

    let engine: &dyn GraphEngine = &db;

    // Full scans.
    assert_eq!(engine.all_nodes().unwrap().len(), 3);
    assert_eq!(engine.all_edges().unwrap().len(), 2);

    // Label scan.
    let people = engine.nodes_by_kind("person", 10, 0).unwrap();
    assert_eq!(people.len(), 2);
    assert!(people.iter().all(|n| n.kind == "person"));

    // Full-edge expansion loads the whole Edge records.
    let mut out = engine.edges_of(a.id, Direction::Outgoing).unwrap();
    out.sort_by(|x, y| x.kind.cmp(&y.kind));
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].kind, "KNOWS");
    assert_eq!(out[1].kind, "TAGGED");

    // Via Cypher: label-less scan, label scan, anonymous relationship scan.
    let count = |src: &str| -> i64 {
        let q = parse(src).unwrap();
        match &execute(&q, &db, HashMap::new()).unwrap().rows[0][0] {
            Value::Integer(n) => *n,
            other => panic!("expected int, got {other:?}"),
        }
    };
    assert_eq!(count("MATCH (n) RETURN count(n)"), 3);
    assert_eq!(count("MATCH (n:person) RETURN count(n)"), 2);
    assert_eq!(count("MATCH ()-[r]->() RETURN count(r)"), 2);
}
