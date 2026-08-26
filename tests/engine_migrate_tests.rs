//! Cross-engine data migration (RFC `docs/rfc-native-core.md`, #307, Phase 6
//! prerequisite).
//!
//! A live graph must be movable between the KV-backed [`drevo::db::Drevo`] and
//! the native [`drevo::native::NativeGraph`] without losing a byte — same node
//! and edge **ids**, same content, same adjacency, and with the id-allocation
//! counters clamped so a post-migration create never reuses an imported id.
//! That is what lets a deployment adopt (or roll back from) the native engine.
//!
//! Migration rides the proven `drevo-json-v1` [`drevo::dump::Dump`] interchange
//! that already round-trips through JSON / GraphML, so both engines speak the
//! same dialect and [`drevo::migrate::migrate`] is a one-liner over the
//! [`drevo::engine::GraphEngine`] seam.
//!
//! uuid / timestamp fields are non-deterministic and excluded from comparison;
//! everything else (id, kind, title, body, properties, endpoints, weight) must
//! survive the trip exactly.

use drevo::db::Drevo;
use drevo::engine::GraphEngine;
use drevo::migrate::migrate;
use drevo::model::{Direction, Edge, NewEdge, NewNode, Node};
use drevo::native::NativeGraph;

// ---------------------------------------------------------------------------
// Comparable projections (drop uuid / created_at / updated_at)
// ---------------------------------------------------------------------------

fn node_key(n: &Node) -> (u64, String, String, String, String) {
    (
        n.id,
        n.kind.clone(),
        n.title.clone(),
        n.body.clone(),
        serde_json::to_string(&n.properties).unwrap(),
    )
}

fn edge_key(e: &Edge) -> (u64, u64, u64, String, String, String) {
    (
        e.id,
        e.from_id,
        e.to_id,
        e.kind.clone(),
        format!("{:?}", e.weight),
        serde_json::to_string(&e.properties).unwrap(),
    )
}

fn new_node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.into(),
        title: title.into(),
        body: format!("body of {title}"),
        body_html: String::new(),
        properties: Default::default(),
    }
}

fn new_edge(from: u64, to: u64, kind: &str) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.into(),
        weight: 1.0,
        properties: Default::default(),
    }
}

/// Assert two engines carry byte-identical graphs (ids, content, counters).
fn assert_same_graph(a: &dyn GraphEngine, b: &dyn GraphEngine) {
    let da = a.export_dump().unwrap();
    let db = b.export_dump().unwrap();

    let mut na: Vec<_> = da.nodes.iter().map(node_key).collect();
    let mut nb: Vec<_> = db.nodes.iter().map(node_key).collect();
    na.sort();
    nb.sort();
    assert_eq!(na, nb, "node sets diverge");

    let mut ea: Vec<_> = da.edges.iter().map(edge_key).collect();
    let mut eb: Vec<_> = db.edges.iter().map(edge_key).collect();
    ea.sort();
    eb.sort();
    assert_eq!(ea, eb, "edge sets diverge");

    assert_eq!(da.next_node_id, db.next_node_id, "next_node_id diverges");
    assert_eq!(da.next_edge_id, db.next_edge_id, "next_edge_id diverges");
}

/// Build a small graph with a create/update/delete history on any engine.
fn seed(g: &dyn GraphEngine) {
    let a = g.create_node(new_node("Person", "alice")).unwrap();
    let b = g.create_node(new_node("Person", "bob")).unwrap();
    let c = g.create_node(new_node("Company", "acme")).unwrap();
    let _throwaway = g.create_node(new_node("Person", "carol")).unwrap();
    g.create_edge(new_edge(a.id, b.id, "KNOWS")).unwrap();
    g.create_edge(new_edge(a.id, c.id, "WORKS_AT")).unwrap();
    g.create_edge(new_edge(b.id, c.id, "WORKS_AT")).unwrap();
    // Delete a node so ids have a gap and counters sit above live max.
    g.delete_node(_throwaway.id).unwrap();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn migrate_kv_to_native_preserves_graph() {
    let kv = Drevo::open_in_memory().unwrap();
    seed(&kv);

    let native = NativeGraph::new();
    let report = migrate(&kv, &native).unwrap();

    assert_eq!(report.nodes_imported, 3);
    assert_eq!(report.edges_imported, 3);
    assert_eq!(report.nodes_skipped, 0);
    assert_eq!(report.edges_skipped, 0);
    assert_same_graph(&kv, &native);
}

#[test]
fn migrate_native_to_kv_preserves_graph() {
    let native = NativeGraph::new();
    seed(&native);

    let kv = Drevo::open_in_memory().unwrap();
    let report = migrate(&native, &kv).unwrap();

    assert_eq!(report.nodes_imported, 3);
    assert_eq!(report.edges_imported, 3);
    assert_same_graph(&native, &kv);
}

#[test]
fn migrate_round_trips_kv_to_native_to_kv() {
    let kv1 = Drevo::open_in_memory().unwrap();
    seed(&kv1);

    let native = NativeGraph::new();
    migrate(&kv1, &native).unwrap();

    let kv2 = Drevo::open_in_memory().unwrap();
    migrate(&native, &kv2).unwrap();

    assert_same_graph(&kv1, &kv2);
}

#[test]
fn migration_preserves_ids_and_edge_endpoints() {
    let kv = Drevo::open_in_memory().unwrap();
    let a = kv.create_node(new_node("Person", "alice")).unwrap();
    let b = kv.create_node(new_node("Person", "bob")).unwrap();
    let e = kv.create_edge(new_edge(a.id, b.id, "KNOWS")).unwrap();

    let native = NativeGraph::new();
    migrate(&kv, &native).unwrap();

    // Same ids resolve to the same records on the native side.
    assert_eq!(native.get_node(a.id).unwrap().unwrap().title, "alice");
    assert_eq!(native.get_node(b.id).unwrap().unwrap().title, "bob");
    let ne = native.get_edge(e.id).unwrap().unwrap();
    assert_eq!((ne.from_id, ne.to_id), (a.id, b.id));
    // Adjacency is rebuilt: a KNOWS b.
    assert_eq!(
        native
            .neighbor_ids(a.id, Direction::Outgoing, None)
            .unwrap(),
        vec![b.id]
    );
}

#[test]
fn migration_clamps_counters_so_new_ids_never_collide() {
    let kv = Drevo::open_in_memory().unwrap();
    seed(&kv);
    let max_node = kv
        .export_dump()
        .unwrap()
        .nodes
        .iter()
        .map(|n| n.id)
        .max()
        .unwrap();

    let native = NativeGraph::new();
    migrate(&kv, &native).unwrap();

    // A create after migration must allocate strictly above every imported id.
    let fresh = native.create_node(new_node("Person", "dave")).unwrap();
    assert!(
        fresh.id > max_node,
        "fresh id {} must be above imported max {}",
        fresh.id,
        max_node
    );
    // And it must not clobber an imported node.
    assert_eq!(native.get_node(fresh.id).unwrap().unwrap().title, "dave");
}

#[test]
fn apply_dump_is_idempotent_on_native() {
    let kv = Drevo::open_in_memory().unwrap();
    seed(&kv);
    let dump = kv.export_dump().unwrap();

    let native = NativeGraph::new();
    let first = native.apply_dump(dump.clone()).unwrap();
    assert_eq!(first.nodes_imported, 3);

    // Re-applying the identical dump inserts nothing new.
    let second = native.apply_dump(dump).unwrap();
    assert_eq!(second.nodes_imported, 0);
    assert_eq!(second.edges_imported, 0);
    assert_eq!(second.nodes_skipped, 3);
    assert_eq!(second.edges_skipped, 3);
    assert_same_graph(&kv, &native);
}

#[test]
fn apply_dump_rejects_id_collision_with_different_content() {
    let native = NativeGraph::new();
    let a = native.create_node(new_node("Person", "alice")).unwrap();

    // A dump that reuses id `a` for different content must be refused, not
    // silently overwrite the live node.
    let mut dump = native.export_dump().unwrap();
    dump.nodes[0].title = "IMPOSTER".to_string();

    let err = native.apply_dump(dump).unwrap_err();
    assert!(
        err.to_string().contains("collision"),
        "expected id-collision error, got: {err}"
    );
    // The live node is untouched.
    assert_eq!(native.get_node(a.id).unwrap().unwrap().title, "alice");
}

#[test]
fn apply_dump_rejects_edge_with_missing_endpoint() {
    let native = NativeGraph::new();
    let a = native.create_node(new_node("Person", "alice")).unwrap();
    let mut dump = native.export_dump().unwrap();
    // Inject an edge pointing at a non-existent node.
    dump.edges.push(Edge {
        id: 99,
        uuid: [0u8; 16],
        from_id: a.id,
        to_id: 12345,
        kind: "KNOWS".into(),
        weight: 1.0,
        created_at: 0,
        properties: Default::default(),
    });

    let err = native.apply_dump(dump).unwrap_err();
    assert!(
        matches!(err, drevo_core::error::CoreError::NodeNotFound(12345)),
        "expected NodeNotFound(12345), got: {err}"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn migration_into_durable_native_survives_reopen() {
    // Migrating into a durable engine must journal the imported records, so the
    // moved graph is still there after a crash/reopen — the whole point of
    // adopting the native engine on disk.
    let kv = Drevo::open_in_memory().unwrap();
    seed(&kv);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.wal");

    {
        let native = NativeGraph::open_durable(&path).unwrap();
        migrate(&kv, &native).unwrap();
    } // dropped == crash; every imported record was fsynced by apply_dump

    let reopened = NativeGraph::open_durable(&path).unwrap();
    assert_same_graph(&kv, &reopened);
    // And an allocation after reopen still clears every imported id.
    let max_node = kv
        .export_dump()
        .unwrap()
        .nodes
        .iter()
        .map(|n| n.id)
        .max()
        .unwrap();
    let fresh = reopened.create_node(new_node("Person", "erin")).unwrap();
    assert!(fresh.id > max_node);
}
