//! KV ↔ native embedding-store parity (issue #446, epic #444).
//!
//! The native durable embedding store + HNSW (`NativeService`) must behave
//! identically to the KV `Drevo` handle's `vec:` store, so drevo-py can move
//! onto native (S3) without any behaviour change. Same nodes + embeddings are
//! built on both engines, then `get`/`count`/`vector_search`/`delete` are
//! asserted equal — the parity guard analogous to
//! `tests/cypher_kv_native_differential_tests.rs`.

use drevo::db::Drevo;
use drevo::engine::GraphEngine;
use drevo::model::{NewNode, Properties};
use drevo::native_service::NativeService;
use drevo::vector::{HnswConfig, Vector};

fn nn(title: &str) -> NewNode {
    NewNode {
        kind: "doc".into(),
        title: title.into(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(Default::default()),
    }
}

/// Same nodes + embeddings on both engines; ids allocate 1.. on each, so they
/// match position-for-position (id parity is part of the contract).
fn fixtures() -> (Drevo, NativeService, Vec<u64>) {
    let kv = Drevo::open_in_memory().unwrap();
    let native = NativeService::in_memory();
    let vecs = [
        vec![1.0, 0.0, 0.0],
        vec![0.0, 1.0, 0.0],
        vec![0.0, 0.0, 1.0],
        vec![0.7, 0.7, 0.0],
    ];
    let mut ids = Vec::new();
    for (i, v) in vecs.iter().enumerate() {
        let title = format!("n{i}");
        let kid = kv.create_node(nn(&title)).unwrap().id;
        let nid = native.graph().create_node(nn(&title)).unwrap().id;
        assert_eq!(kid, nid, "ids must match across engines");
        kv.set_embedding(kid, Vector::from(v.clone())).unwrap();
        native.set_embedding(nid, v.clone()).unwrap();
        ids.push(kid);
    }
    (kv, native, ids)
}

#[test]
fn get_and_count_match() {
    let (kv, native, ids) = fixtures();
    assert_eq!(kv.embedding_count().unwrap(), native.embedding_count());
    for id in ids {
        let k = kv.get_embedding(id).unwrap().map(|v| v.0);
        assert_eq!(k, native.get_embedding(id), "embedding for {id} must match");
    }
}

#[test]
fn missing_node_rejected_on_both() {
    let kv = Drevo::open_in_memory().unwrap();
    let native = NativeService::in_memory();
    assert!(kv.set_embedding(999, Vector::from(vec![1.0])).is_err());
    assert!(native.set_embedding(999, vec![1.0]).is_err());
}

#[test]
fn vector_search_matches() {
    let (kv, native, _) = fixtures();
    let query = [0.9, 0.1, 0.0];
    // Both rebuild a default-config HNSW over the same vectors inserted in the
    // same (ascending-id) order, so the index — and the search — are identical.
    let kv_hits: Vec<(u64, f32)> = kv
        .build_vector_index(HnswConfig::default())
        .unwrap()
        .search(&query, 3)
        .unwrap()
        .into_iter()
        .map(|n| (n.key, n.distance))
        .collect();
    let native_hits = native.vector_search(&query, 3).unwrap();
    assert_eq!(
        kv_hits, native_hits,
        "KV and native HNSW must return identical (id, distance) ordering"
    );
}

#[test]
fn delete_matches() {
    let (kv, native, ids) = fixtures();
    kv.delete_embedding(ids[0]).unwrap();
    native.delete_embedding(ids[0]).unwrap();
    assert_eq!(kv.embedding_count().unwrap(), native.embedding_count());
    assert_eq!(
        kv.get_embedding(ids[0]).unwrap().map(|v| v.0),
        native.get_embedding(ids[0])
    );
}
