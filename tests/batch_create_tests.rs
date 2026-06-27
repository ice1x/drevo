//! Batch node/edge creation (`Drevo::create_nodes` / `create_edges`).
//!
//! These verify the bulk-insert path used by importers (e.g.
//! `tools/neo4j-to-drevo`) is (a) behaviourally identical to looping the
//! per-item `create_node`/`create_edge` and (b) writes every secondary index
//! (record / uuid / title / kind / FTS / property / adjacency) — just folded
//! into a single transaction. The speed win (one fsync for N items) is not
//! asserted here (timing is environment-dependent); correctness is.

use drevo::db::Drevo;
use drevo::error::DrevoError;
use drevo::model::{Direction, NewEdge, NewNode, Properties};

fn node(kind: &str, title: &str, body: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn node_with_prop(kind: &str, title: &str, key: &str, value: serde_json::Value) -> NewNode {
    let mut props = Properties::default();
    props.0.insert(key.to_string(), value);
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: props,
    }
}

fn edge(from_id: u64, to_id: u64, kind: &str) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

#[test]
fn create_nodes_persists_record_and_all_indexes() {
    let db = Drevo::open_in_memory().unwrap();
    let created = db
        .create_nodes(vec![
            node("person", "alpha_one", "hello world"),
            node("person", "beta_two", "graph database"),
            node("project", "gamma_three", "hello graph"),
        ])
        .unwrap();
    assert_eq!(created.len(), 3);

    // Record + id index.
    for n in &created {
        assert_eq!(db.get_node(n.id).unwrap().unwrap().title, n.title);
    }
    // Title index.
    assert_eq!(
        db.get_node_by_title("beta_two").unwrap().unwrap().kind,
        "person"
    );
    // UUID index.
    let a = &created[0];
    assert_eq!(db.get_node_by_uuid(&a.uuid).unwrap().unwrap().id, a.id);
    // Kind index.
    assert_eq!(db.list_nodes_by_kind("person", 10, 0).unwrap().len(), 2);
    assert_eq!(db.list_nodes_by_kind("project", 10, 0).unwrap().len(), 1);
    // FTS index — "hello" appears in two of the three documents.
    let hits = db.search_fts("hello", 10).unwrap();
    assert!(
        hits.len() >= 2,
        "expected >=2 FTS hits for 'hello', got {}",
        hits.len()
    );
}

#[test]
fn create_nodes_indexes_properties() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_nodes(vec![
        node_with_prop("task", "t1", "team", serde_json::json!("alpha")),
        node_with_prop("task", "t2", "team", serde_json::json!("alpha")),
        node_with_prop("task", "t3", "team", serde_json::json!("beta")),
    ])
    .unwrap();
    assert_eq!(
        db.count_nodes_by_property("team", &serde_json::json!("alpha"))
            .unwrap(),
        2
    );
    assert_eq!(
        db.count_nodes_by_property("team", &serde_json::json!("beta"))
            .unwrap(),
        1
    );
}

#[test]
fn create_nodes_duplicate_title_within_batch_errors() {
    let db = Drevo::open_in_memory().unwrap();
    let err = db
        .create_nodes(vec![node("k", "dup", ""), node("k", "dup", "")])
        .unwrap_err();
    assert!(matches!(err, DrevoError::DuplicateTitle(t) if t == "dup"));
}

#[test]
fn create_nodes_duplicate_title_vs_existing_errors() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_node(node("k", "exists", "")).unwrap();
    let err = db.create_nodes(vec![node("k", "exists", "")]).unwrap_err();
    assert!(matches!(err, DrevoError::DuplicateTitle(t) if t == "exists"));
}

#[test]
fn create_edges_persists_record_and_adjacency() {
    let db = Drevo::open_in_memory().unwrap();
    let ns = db
        .create_nodes(vec![
            node("p", "a", ""),
            node("p", "b", ""),
            node("p", "c", ""),
        ])
        .unwrap();
    let (a, b, c) = (ns[0].id, ns[1].id, ns[2].id);

    let edges = db
        .create_edges(vec![edge(a, b, "knows"), edge(a, c, "knows")])
        .unwrap();
    assert_eq!(edges.len(), 2);

    // Edge record + id index.
    assert_eq!(db.get_edge(edges[0].id).unwrap().unwrap().from_id, a);
    // Adjacency: a's outgoing neighbours are b and c.
    let mut out: Vec<u64> = db
        .neighbors(a, Direction::Outgoing, None)
        .unwrap()
        .into_iter()
        .map(|n| n.id)
        .collect();
    out.sort_unstable();
    let mut want = vec![b, c];
    want.sort_unstable();
    assert_eq!(out, want);
    // Kind filter on adjacency still works.
    assert_eq!(
        db.neighbors(a, Direction::Outgoing, Some("knows"))
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn create_edges_missing_endpoint_errors() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(node("p", "a", "")).unwrap().id;
    let err = db.create_edges(vec![edge(a, 9999, "knows")]).unwrap_err();
    assert!(matches!(err, DrevoError::NodeNotFound(id) if id == 9999));
}

#[test]
fn create_edges_invalid_weight_errors() {
    let db = Drevo::open_in_memory().unwrap();
    let ns = db
        .create_nodes(vec![node("p", "a", ""), node("p", "b", "")])
        .unwrap();
    let mut e = edge(ns[0].id, ns[1].id, "knows");
    e.weight = f32::INFINITY;
    let err = db.create_edges(vec![e]).unwrap_err();
    assert!(matches!(err, DrevoError::InvalidWeight(_)));
}

#[test]
fn batch_matches_individual_path() {
    // Two databases built the same logical graph two ways.
    let individual = Drevo::open_in_memory().unwrap();
    let batch = Drevo::open_in_memory().unwrap();

    let specs = [
        ("person", "carol", "alpha text"),
        ("person", "dave", "beta text"),
        ("project", "drevo", "alpha graph"),
    ];

    for (k, t, b) in specs {
        individual.create_node(node(k, t, b)).unwrap();
    }
    batch
        .create_nodes(specs.iter().map(|(k, t, b)| node(k, t, b)).collect())
        .unwrap();

    // Same kind buckets.
    for kind in ["person", "project"] {
        assert_eq!(
            individual.list_nodes_by_kind(kind, 100, 0).unwrap().len(),
            batch.list_nodes_by_kind(kind, 100, 0).unwrap().len(),
            "kind {kind} count diverged"
        );
    }
    // Same title lookups.
    for (_, t, _) in specs {
        assert_eq!(
            individual.get_node_by_title(t).unwrap().is_some(),
            batch.get_node_by_title(t).unwrap().is_some(),
        );
    }
    // Same FTS recall for a shared token.
    assert_eq!(
        individual.search_fts("alpha", 10).unwrap().len(),
        batch.search_fts("alpha", 10).unwrap().len(),
    );
}
