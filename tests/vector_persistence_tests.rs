//! Integration tests for Phase 12 task `00078` — vector persistence.
//!
//! Task `00075` gave drevo a [`Vector`] type and `00076` an in-memory
//! [`HnswIndex`]; neither survives a process restart. `00078` is the
//! durable half: a `vec:{node_id}` key schema on the same
//! [`StorageBackend`] the graph store uses, a batched insert API that
//! commits a whole corpus in one redb transaction, and a
//! persistence → index bridge (`build_hnsw` / `Drevo::build_vector_index`)
//! that reconstructs the HNSW graph from disk after a reopen.
//!
//! The contract exercised here:
//!
//! - Store free functions (`put` / `get` / `delete` / `count` /
//!   `put_batch` / `scan_all`) behave identically on the in-memory and
//!   redb backends (backend parity, per `drevo-tdd`).
//! - Embeddings written through the redb backend survive a close/reopen.
//! - The `Drevo` facade methods validate node existence, cascade-delete
//!   the embedding when the owning node is deleted, and rebuild a working
//!   HNSW index from the persisted vectors.
//! - The canonical graph-RAG flow — embed a corpus, persist, reopen,
//!   rebuild the index, query nearest neighbours — works end to end.

use drevo::db::Drevo;
use drevo::model::{NewEdge, NewNode, Properties};
use drevo::storage::{MemoryBackend, RedbBackend, StorageBackend};
use drevo::vector::{store, HnswConfig, Vector};
use tempfile::TempDir;

fn new_node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: format!("body for {title}"),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

// ---------------------------------------------------------------
// Backend-parity tests: the same suite runs against both backends.
// ---------------------------------------------------------------

/// Run a store assertion closure against a fresh instance of each backend
/// so the two implementations are held to an identical contract.
fn for_each_backend(test: impl Fn(&dyn StorageBackend)) {
    test(&MemoryBackend::new());

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("vectors.redb");
    let redb = RedbBackend::open(&path).unwrap();
    test(&redb);
}

#[test]
fn put_then_get_round_trips_on_both_backends() {
    for_each_backend(|b| {
        let v = Vector::from(vec![0.1, 0.2, 0.3, 0.4]);
        store::put(b, 1, &v).unwrap();
        assert_eq!(store::get(b, 1).unwrap(), Some(v));
    });
}

#[test]
fn get_missing_is_none_on_both_backends() {
    for_each_backend(|b| {
        assert_eq!(store::get(b, 404).unwrap(), None);
    });
}

#[test]
fn delete_removes_on_both_backends() {
    for_each_backend(|b| {
        store::put(b, 9, &Vector::from(vec![1.0, 2.0])).unwrap();
        store::delete(b, 9).unwrap();
        assert_eq!(store::get(b, 9).unwrap(), None);
    });
}

#[test]
fn count_tracks_population_on_both_backends() {
    for_each_backend(|b| {
        assert_eq!(store::count(b).unwrap(), 0);
        store::put(b, 1, &Vector::from(vec![1.0])).unwrap();
        store::put(b, 2, &Vector::from(vec![2.0])).unwrap();
        store::put(b, 3, &Vector::from(vec![3.0])).unwrap();
        assert_eq!(store::count(b).unwrap(), 3);
    });
}

#[test]
fn put_batch_inserts_all_on_both_backends() {
    for_each_backend(|b| {
        let items: Vec<(u64, Vector)> = (0..50)
            .map(|i| (i, Vector::from(vec![i as f32, (i * 2) as f32])))
            .collect();
        store::put_batch(b, &items).unwrap();
        assert_eq!(store::count(b).unwrap(), 50);
        assert_eq!(
            store::get(b, 25).unwrap(),
            Some(Vector::from(vec![25.0, 50.0]))
        );
    });
}

#[test]
fn scan_all_returns_node_id_ordered_pairs_on_both_backends() {
    for_each_backend(|b| {
        store::put(b, 30, &Vector::from(vec![3.0])).unwrap();
        store::put(b, 10, &Vector::from(vec![1.0])).unwrap();
        store::put(b, 20, &Vector::from(vec![2.0])).unwrap();
        let all = store::scan_all(b).unwrap();
        let ids: Vec<u64> = all.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![10, 20, 30]);
    });
}

#[test]
fn vector_keys_do_not_collide_with_graph_keys() {
    // The store must only ever see its own `vec:` prefix — a node row
    // ("node:...") sharing the backend must not leak into scan_all/count.
    for_each_backend(|b| {
        b.put(b"node:\x01", b"not a vector").unwrap();
        b.put(b"edge:\x01", b"not a vector").unwrap();
        store::put(b, 1, &Vector::from(vec![1.0, 2.0])).unwrap();
        assert_eq!(store::count(b).unwrap(), 1);
        assert_eq!(store::scan_all(b).unwrap().len(), 1);
    });
}

// ---------------------------------------------------------------
// Durability: embeddings survive a redb reopen.
// ---------------------------------------------------------------

#[test]
fn embeddings_persist_across_redb_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("persist.redb");

    {
        let backend = RedbBackend::open(&path).unwrap();
        store::put_batch(
            &backend,
            &[
                (1, Vector::from(vec![1.0, 0.0, 0.0])),
                (2, Vector::from(vec![0.0, 1.0, 0.0])),
                (3, Vector::from(vec![0.0, 0.0, 1.0])),
            ],
        )
        .unwrap();
    }

    let backend = RedbBackend::open(&path).unwrap();
    assert_eq!(store::count(&backend).unwrap(), 3);
    assert_eq!(
        store::get(&backend, 2).unwrap(),
        Some(Vector::from(vec![0.0, 1.0, 0.0]))
    );
}

#[test]
fn build_hnsw_after_reopen_finds_nearest() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("rebuild.redb");

    {
        let backend = RedbBackend::open(&path).unwrap();
        store::put_batch(
            &backend,
            &[
                (1, Vector::from(vec![1.0, 0.0, 0.0])),
                (2, Vector::from(vec![0.0, 1.0, 0.0])),
                (3, Vector::from(vec![0.9, 0.1, 0.0])),
            ],
        )
        .unwrap();
    }

    let backend = RedbBackend::open(&path).unwrap();
    let index = store::build_hnsw(&backend, HnswConfig::default()).unwrap();
    assert_eq!(index.len(), 3);
    // Query closest to node 1 / node 3; node 2 is orthogonal.
    let hits = index.search(&[1.0, 0.0, 0.0], 2).unwrap();
    let keys: Vec<u64> = hits.iter().map(|n| n.key).collect();
    assert!(keys.contains(&1));
    assert!(keys.contains(&3));
    assert!(!keys.contains(&2));
}

// ---------------------------------------------------------------
// Drevo facade: validation, cascade, and the index bridge.
// ---------------------------------------------------------------

#[test]
fn set_embedding_requires_existing_node() {
    let db = Drevo::open_in_memory().unwrap();
    let err = db.set_embedding(999, Vector::from(vec![1.0, 2.0]));
    assert!(matches!(
        err,
        Err(drevo::error::DrevoError::NodeNotFound(999))
    ));
}

#[test]
fn set_and_get_embedding_through_facade() {
    let db = Drevo::open_in_memory().unwrap();
    let node = db.create_node(new_node("doc", "Embedded doc")).unwrap();
    let v = Vector::from(vec![0.5, 0.5, 0.5, 0.5]);
    db.set_embedding(node.id, v.clone()).unwrap();
    assert_eq!(db.get_embedding(node.id).unwrap(), Some(v));
    assert_eq!(db.embedding_count().unwrap(), 1);
}

#[test]
fn deleting_node_cascades_to_its_embedding() {
    let db = Drevo::open_in_memory().unwrap();
    let node = db.create_node(new_node("doc", "Doomed")).unwrap();
    db.set_embedding(node.id, Vector::from(vec![1.0, 2.0, 3.0]))
        .unwrap();
    assert_eq!(db.embedding_count().unwrap(), 1);

    db.delete_node(node.id).unwrap();
    assert_eq!(db.get_embedding(node.id).unwrap(), None);
    assert_eq!(db.embedding_count().unwrap(), 0);
}

#[test]
fn batch_embeddings_are_all_or_nothing_on_bad_id() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("doc", "A")).unwrap();
    let b = db.create_node(new_node("doc", "B")).unwrap();

    // Second pair references a non-existent node — the whole batch must
    // be rejected, leaving no embedding written.
    let err = db.set_embeddings_batch(&[
        (a.id, Vector::from(vec![1.0, 0.0])),
        (12345, Vector::from(vec![0.0, 1.0])),
        (b.id, Vector::from(vec![1.0, 1.0])),
    ]);
    assert!(matches!(
        err,
        Err(drevo::error::DrevoError::NodeNotFound(12345))
    ));
    assert_eq!(db.embedding_count().unwrap(), 0);
}

#[test]
fn build_vector_index_through_facade_round_trips() {
    let db = Drevo::open_in_memory().unwrap();
    let mut batch = Vec::new();
    for i in 0..10u64 {
        let node = db
            .create_node(new_node("doc", &format!("doc-{i}")))
            .unwrap();
        // Distinct one-hot-ish embeddings so nearest-neighbour is unambiguous.
        let mut e = vec![0.0f32; 10];
        e[i as usize] = 1.0;
        batch.push((node.id, Vector::from(e)));
    }
    db.set_embeddings_batch(&batch).unwrap();

    let index = db.build_vector_index(HnswConfig::default()).unwrap();
    assert_eq!(index.len(), 10);

    // The query closest to the node whose one-hot is index 4 is that node.
    let target_id = batch[4].0;
    let mut q = vec![0.0f32; 10];
    q[4] = 1.0;
    let hits = index.search(&q, 1).unwrap();
    assert_eq!(hits[0].key, target_id);
}

// ---------------------------------------------------------------
// End-to-end graph-RAG scenario across a reopen.
// ---------------------------------------------------------------

#[test]
fn graph_rag_corpus_persist_reopen_and_retrieve() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("rag.redb");

    // Ingest a tiny "document" corpus with reference edges, then embed it
    // and persist the embeddings — the canonical graph-RAG write path.
    let (alpha_id, beta_id) = {
        let db = Drevo::open(&path).unwrap();
        let alpha = db
            .create_node(new_node("doc", "Vectors in databases"))
            .unwrap();
        let beta = db.create_node(new_node("doc", "Graph traversal")).unwrap();
        let gamma = db
            .create_node(new_node("doc", "Unrelated cooking"))
            .unwrap();
        db.create_edge(NewEdge {
            from_id: alpha.id,
            to_id: beta.id,
            kind: "cites".to_string(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .unwrap();

        db.set_embeddings_batch(&[
            (alpha.id, Vector::from(vec![1.0, 0.1, 0.0])),
            (beta.id, Vector::from(vec![0.9, 0.2, 0.0])),
            (gamma.id, Vector::from(vec![0.0, 0.0, 1.0])),
        ])
        .unwrap();
        db.close().unwrap();
        (alpha.id, beta.id)
    };

    // Reopen, rebuild the index from disk, and retrieve: a query near the
    // "vectors" cluster returns alpha, whose neighbourhood (via the cites
    // edge) is beta — similarity hit expanded into graph context.
    let db = Drevo::open(&path).unwrap();
    assert_eq!(db.embedding_count().unwrap(), 3);

    let index = db.build_vector_index(HnswConfig::default()).unwrap();
    let hits = index.search(&[1.0, 0.1, 0.0], 1).unwrap();
    assert_eq!(hits[0].key, alpha_id);

    // Expand the similarity hit into its neighbourhood.
    let neighbours = db
        .neighbors(alpha_id, drevo::model::Direction::Outgoing, None)
        .unwrap();
    assert!(neighbours.iter().any(|n| n.id == beta_id));
}
