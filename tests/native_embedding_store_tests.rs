//! Native durable embedding store + HNSW behaviour (issue #446, epic #444).
//!
//! The native `NativeService` embedding store — `set`/`get`/`count`/`delete`
//! plus HNSW `vector_search` — is drevo-py's backend (S3), so its behaviour is
//! pinned here directly. This was formerly a KV-vs-native differential suite;
//! the KV oracle was dropped with the KV engine (epic #444), leaving direct
//! assertions against known vectors.

use drevo::engine::GraphEngine;
use drevo::model::{NewNode, Properties};
use drevo::native_service::NativeService;

fn nn(title: &str) -> NewNode {
    NewNode {
        kind: "doc".into(),
        title: title.into(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(Default::default()),
    }
}

/// The known corpus: four unit-ish vectors on a fresh native store, ids
/// allocated 1.. in insertion order.
fn fixtures() -> (NativeService, Vec<Vec<f32>>, Vec<u64>) {
    let native = NativeService::in_memory();
    let vecs = vec![
        vec![1.0, 0.0, 0.0],
        vec![0.0, 1.0, 0.0],
        vec![0.0, 0.0, 1.0],
        vec![0.7, 0.7, 0.0],
    ];
    let mut ids = Vec::new();
    for (i, v) in vecs.iter().enumerate() {
        let id = native.graph().create_node(nn(&format!("n{i}"))).unwrap().id;
        native.set_embedding(id, v.clone()).unwrap();
        ids.push(id);
    }
    (native, vecs, ids)
}

#[test]
fn count_and_get_return_the_stored_embeddings() {
    let (native, vecs, ids) = fixtures();
    assert_eq!(native.embedding_count(), 4);
    for (id, expected) in ids.iter().zip(vecs) {
        assert_eq!(
            native.get_embedding(*id),
            Some(expected),
            "embedding for {id} must round-trip"
        );
    }
}

#[test]
fn set_embedding_on_a_missing_node_is_rejected() {
    let native = NativeService::in_memory();
    assert!(native.set_embedding(999, vec![1.0]).is_err());
}

#[test]
fn vector_search_ranks_the_nearest_first() {
    let (native, _, ids) = fixtures();
    // A query closest to n0 = [1, 0, 0].
    let hits = native.vector_search(&[0.9, 0.1, 0.0], 3).unwrap();
    assert_eq!(hits.len(), 3, "asked for the 3 nearest");
    assert_eq!(
        hits[0].0, ids[0],
        "the nearest neighbour of [0.9, 0.1, 0] must be n0, got {hits:?}"
    );
}

#[test]
fn delete_removes_the_embedding() {
    let (native, _, ids) = fixtures();
    native.delete_embedding(ids[0]).unwrap();
    assert_eq!(native.embedding_count(), 3, "one fewer after delete");
    assert_eq!(
        native.get_embedding(ids[0]),
        None,
        "the deleted embedding is gone"
    );
}
