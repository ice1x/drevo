//! Phase 9 task `00057` — GraphML import integration tests.
//!
//! Cover the `Drevo::import_graphml` / `Drevo::import_graphml_from_path`
//! surface that ships under the `dump` module — the inverse of the task
//! `00056` GraphML exporter. The assertions focus on:
//!
//! 1. **Round-trip fidelity** — a graph exported with `export_graphml` and
//!    re-imported into a fresh database reproduces every node/edge verbatim
//!    (ids, uuids, timestamps, kinds, titles, bodies, properties, weights),
//!    and re-exporting the destination yields byte-identical GraphML.
//! 2. **Idempotence** — re-importing the same document a second time inserts
//!    nothing and reports every row as skipped, exactly like JSON import.
//! 3. **Merge into a populated database** — importing a disjoint graph adds
//!    to an existing database without disturbing what is already there.
//! 4. **Interop** — a foreign GraphML document (arbitrary string ids, keys
//!    referenced by `attr.name`, no uuids/timestamps) imports by allocating
//!    fresh ids, remapping edge endpoints, and folding unknown attributes
//!    into the property map.
//! 5. **Escaping / Unicode** — XML entity references and multi-byte content
//!    survive the parse.
//! 6. **Error handling** — malformed XML, a missing `<graphml>`/`<graph>`
//!    element, and edges referencing undeclared nodes are rejected.
//! 7. **Cross-backend parity** — a redb-backed database imports the same
//!    document as an in-memory one.
//! 8. **Filesystem variant** — `import_graphml_from_path` reads and loads a
//!    document written to a `tempfile::TempDir` path.

use drevo::db::Drevo;
use drevo::dump::Dump;
use drevo::model::{Edge, NewEdge, NewNode, Node, Properties};
use serde_json::json;
use std::collections::HashMap;
use tempfile::TempDir;

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

fn props_with(pairs: &[(&str, serde_json::Value)]) -> Properties {
    let mut map = HashMap::new();
    for (k, v) in pairs {
        map.insert((*k).to_string(), v.clone());
    }
    Properties::from(map)
}

fn new_node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: format!("Body of {title}"),
        body_html: format!("<p>Body of {title}</p>"),
        properties: props_with(&[("priority", json!(1)), ("tag", json!("test"))]),
    }
}

fn new_edge(from: u64, to: u64, kind: &str, weight: f32) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.to_string(),
        weight,
        properties: props_with(&[("label", json!(kind))]),
    }
}

fn populate_sample_graph(db: &Drevo) -> (Vec<u64>, Vec<u64>) {
    let n1 = db.create_node(new_node("note", "Alpha")).unwrap();
    let n2 = db.create_node(new_node("note", "Beta")).unwrap();
    let n3 = db.create_node(new_node("tag", "Gamma")).unwrap();
    let n4 = db.create_node(new_node("person", "Delta")).unwrap();
    let n5 = db.create_node(new_node("note", "Epsilon")).unwrap();

    let e1 = db
        .create_edge(new_edge(n1.id, n2.id, "links_to", 1.0))
        .unwrap();
    let e2 = db
        .create_edge(new_edge(n2.id, n3.id, "tagged_with", 0.5))
        .unwrap();
    let e3 = db
        .create_edge(new_edge(n4.id, n1.id, "authored", 2.0))
        .unwrap();
    let e4 = db
        .create_edge(new_edge(n1.id, n5.id, "links_to", 1.5))
        .unwrap();

    (
        vec![n1.id, n2.id, n3.id, n4.id, n5.id],
        vec![e1.id, e2.id, e3.id, e4.id],
    )
}

/// Deserialize a database's full contents via its deterministic JSON dump —
/// the only public surface that enumerates every node and edge. `collect_all_*`
/// is crate-private, so integration tests read the graph through `export_json`.
fn dump_of(db: &Drevo) -> Dump {
    serde_json::from_str(&db.export_json().unwrap()).unwrap()
}

fn all_nodes(db: &Drevo) -> Vec<Node> {
    dump_of(db).nodes
}

fn all_edges(db: &Drevo) -> Vec<Edge> {
    dump_of(db).edges
}

/// Assert that two databases hold the same set of nodes and edges (by id and
/// full content) — the strong notion of a lossless import. Compares the
/// deterministic JSON dumps with the volatile `exported_at` timestamp zeroed.
fn assert_same_graph(a: &Drevo, b: &Drevo) {
    let mut da = dump_of(a);
    let mut db_ = dump_of(b);
    da.exported_at = 0;
    db_.exported_at = 0;
    assert_eq!(da.nodes, db_.nodes, "node sets differ after import");
    assert_eq!(da.edges, db_.edges, "edge sets differ after import");
    assert_eq!(da.next_node_id, db_.next_node_id);
    assert_eq!(da.next_edge_id, db_.next_edge_id);
}

// ---------------------------------------------------------------
// 1. Round-trip fidelity
// ---------------------------------------------------------------

#[test]
fn import_graphml_round_trips_full_graph() {
    let src = Drevo::open_in_memory().unwrap();
    populate_sample_graph(&src);
    let xml = src.export_graphml().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    let report = dst.import_graphml(&xml).unwrap();
    assert_eq!(report.nodes_imported, 5);
    assert_eq!(report.edges_imported, 4);
    assert_eq!(report.nodes_skipped, 0);
    assert_eq!(report.edges_skipped, 0);

    assert_same_graph(&src, &dst);
    // Re-export of the destination is byte-identical to the source document.
    assert_eq!(dst.export_graphml().unwrap(), xml);
}

// ---------------------------------------------------------------
// 2. Idempotence
// ---------------------------------------------------------------

#[test]
fn import_graphml_second_pass_skips_everything() {
    let src = Drevo::open_in_memory().unwrap();
    populate_sample_graph(&src);
    let xml = src.export_graphml().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    dst.import_graphml(&xml).unwrap();
    let second = dst.import_graphml(&xml).unwrap();
    assert_eq!(second.nodes_imported, 0);
    assert_eq!(second.edges_imported, 0);
    assert_eq!(second.nodes_skipped, 5);
    assert_eq!(second.edges_skipped, 4);
}

// ---------------------------------------------------------------
// 3. Merge into a populated database
// ---------------------------------------------------------------

#[test]
fn import_graphml_merges_disjoint_graph_into_populated_db() {
    // Destination already has one node (id 1, "Existing").
    let dst = Drevo::open_in_memory().unwrap();
    dst.create_node(new_node("note", "Existing")).unwrap();

    // A foreign document with string ids so the importer allocates fresh ids
    // above the existing max rather than colliding on id 1.
    let xml = "<graphml>\
         <key id=\"kt\" for=\"node\" attr.name=\"title\"/>\
         <key id=\"kk\" for=\"node\" attr.name=\"kind\"/>\
         <graph>\
         <node id=\"x\"><data key=\"kt\">Imported One</data><data key=\"kk\">note</data></node>\
         <node id=\"y\"><data key=\"kt\">Imported Two</data><data key=\"kk\">note</data></node>\
         <edge source=\"x\" target=\"y\"><data key=\"kk\">links_to</data></edge>\
         </graph></graphml>";
    let report = dst.import_graphml(xml).unwrap();
    assert_eq!(report.nodes_imported, 2);
    assert_eq!(report.edges_imported, 1);

    // Original node untouched; imported nodes got ids 2 and 3.
    assert_eq!(dst.get_node(1).unwrap().unwrap().title, "Existing");
    let titles: Vec<String> = all_nodes(&dst).into_iter().map(|n| n.title).collect();
    assert_eq!(titles.len(), 3);
    assert!(titles.contains(&"Imported One".to_string()));
    assert!(titles.contains(&"Imported Two".to_string()));
}

// ---------------------------------------------------------------
// 4. Interop with foreign GraphML
// ---------------------------------------------------------------

#[test]
fn import_graphml_foreign_folds_unknown_keys_into_properties() {
    let db = Drevo::open_in_memory().unwrap();
    let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\">\
         <key id=\"d0\" for=\"node\" attr.name=\"title\" attr.type=\"string\"/>\
         <key id=\"d1\" for=\"node\" attr.name=\"color\" attr.type=\"string\"/>\
         <key id=\"d2\" for=\"node\" attr.name=\"score\" attr.type=\"double\"/>\
         <graph edgedefault=\"directed\">\
         <node id=\"only\"><data key=\"d0\">Solo</data><data key=\"d1\">blue</data><data key=\"d2\">7.5</data></node>\
         </graph></graphml>";
    let report = db.import_graphml(xml).unwrap();
    assert_eq!(report.nodes_imported, 1);

    let node = db.get_node(1).unwrap().unwrap();
    assert_eq!(node.title, "Solo");
    // `color` is a string that is not valid JSON on its own → stored as a
    // JSON string; `score` parses as a JSON number.
    assert_eq!(node.properties.get("color"), Some(&json!("blue")));
    assert_eq!(node.properties.get("score"), Some(&json!(7.5)));
}

#[test]
fn import_graphml_preserves_edge_weight_and_direction() {
    let src = Drevo::open_in_memory().unwrap();
    let a = src.create_node(new_node("note", "Head")).unwrap();
    let b = src.create_node(new_node("note", "Tail")).unwrap();
    src.create_edge(new_edge(a.id, b.id, "points_to", 4.25))
        .unwrap();
    let xml = src.export_graphml().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    dst.import_graphml(&xml).unwrap();
    let edges = all_edges(&dst);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].from_id, a.id);
    assert_eq!(edges[0].to_id, b.id);
    assert_eq!(edges[0].kind, "points_to");
    assert_eq!(edges[0].weight, 4.25);
}

// ---------------------------------------------------------------
// 5. Escaping / Unicode
// ---------------------------------------------------------------

#[test]
fn import_graphml_decodes_entities_and_unicode() {
    let src = Drevo::open_in_memory().unwrap();
    src.create_node(NewNode {
        kind: "заметка".into(),
        title: "a < b & c > d \" ' 知识 🌳".into(),
        body: "<tag> & more — Привет".into(),
        body_html: String::new(),
        properties: props_with(&[("标签", json!("中文 & <x>")), ("emoji_🔑", json!("🌈"))]),
    })
    .unwrap();
    let xml = src.export_graphml().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    dst.import_graphml(&xml).unwrap();
    let node = dst.get_node(1).unwrap().unwrap();
    assert_eq!(node.kind, "заметка");
    assert_eq!(node.title, "a < b & c > d \" ' 知识 🌳");
    assert_eq!(node.body, "<tag> & more — Привет");
    assert_eq!(node.properties.get("标签"), Some(&json!("中文 & <x>")));
    assert_eq!(node.properties.get("emoji_🔑"), Some(&json!("🌈")));
}

// ---------------------------------------------------------------
// 6. Error handling
// ---------------------------------------------------------------

#[test]
fn import_graphml_rejects_malformed_xml() {
    let db = Drevo::open_in_memory().unwrap();
    assert!(db.import_graphml("<graphml><graph><node>").is_err());
    assert!(db.import_graphml("not xml at all").is_err());
    assert!(db.import_graphml("<a><b></a></b>").is_err());
}

#[test]
fn import_graphml_rejects_missing_structural_elements() {
    let db = Drevo::open_in_memory().unwrap();
    // No <graphml> root.
    assert!(db.import_graphml("<foo><graph/></foo>").is_err());
    // <graphml> but no <graph>.
    assert!(db
        .import_graphml("<graphml><key id=\"x\"/></graphml>")
        .is_err());
}

#[test]
fn import_graphml_rejects_edge_referencing_undeclared_node() {
    let db = Drevo::open_in_memory().unwrap();
    let xml = "<graphml><graph>\
         <node id=\"n1\"><data key=\"title\">A</data></node>\
         <edge source=\"n1\" target=\"ghost\"/>\
         </graph></graphml>";
    assert!(db.import_graphml(xml).is_err());
    // The undeclared-endpoint check runs while building records, before
    // `apply_dump` inserts anything — so neither the node nor the edge lands.
    assert_eq!(all_nodes(&db).len(), 0);
    assert_eq!(all_edges(&db).len(), 0);
}

// ---------------------------------------------------------------
// 7. Cross-backend parity (Memory → Redb)
// ---------------------------------------------------------------

#[cfg(feature = "redb-backend")]
#[test]
fn import_graphml_into_redb_matches_memory() {
    let src = Drevo::open_in_memory().unwrap();
    populate_sample_graph(&src);
    let xml = src.export_graphml().unwrap();

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("imported.redb");
    let disk = Drevo::open(&path).unwrap();
    let report = disk.import_graphml(&xml).unwrap();
    assert_eq!(report.nodes_imported, 5);
    assert_eq!(report.edges_imported, 4);

    assert_same_graph(&src, &disk);
}

// ---------------------------------------------------------------
// 8. Filesystem variant
// ---------------------------------------------------------------

#[test]
fn import_graphml_from_path_reads_and_loads_document() {
    let src = Drevo::open_in_memory().unwrap();
    populate_sample_graph(&src);
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("graph.graphml");
    src.export_graphml_to_path(&path).unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    let report = dst.import_graphml_from_path(&path).unwrap();
    assert_eq!(report.nodes_imported, 5);
    assert_eq!(report.edges_imported, 4);
    assert_same_graph(&src, &dst);
}
