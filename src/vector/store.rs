//! Phase 12 task `00078` — durable persistence for [`Vector`] embeddings.
//!
//! The `00075` [`Vector`] type and the `00076` [`HnswIndex`] both live
//! purely in memory. This module gives embeddings a home on disk: a small
//! key-value schema layered on top of the same
//! [`StorageBackend`](crate::storage::StorageBackend) the graph store
//! uses, so a `Vector` written on one process run is still there after a
//! reopen. It is the storage half of Phase 12 — the index is the
//! query-acceleration half, and [`build_hnsw`](crate::vector::store::build_hnsw)
//! is the bridge that rebuilds an in-memory [`HnswIndex`] from the
//! persisted vectors after a restart.
//!
//! ## On-disk schema
//!
//! ```text
//! vec:{node_id_le8}  ->  bincode(Vector)
//! ```
//!
//! Each embedding is keyed by the `u64` node id it belongs to, encoded
//! little-endian under the `vec:` prefix — the same convention the node /
//! edge tables use in [`crate::db`]. The payload is the `bincode`
//! serialization of the [`Vector`] (a transparent `Vec<f32>`). Keying by
//! node id means an embedding is a sidecar column on the node, looked up
//! in O(log N) and scanned as a contiguous prefix range.
//!
//! ## Batch insert
//!
//! Embeddings arrive in bulk (a whole corpus is embedded at once), and a
//! per-`put` redb transaction `fsync`s on every row — unusable at corpus
//! scale. [`put_batch`](crate::vector::store::put_batch) folds the whole
//! slice into a single
//! [`StorageBackend::put_batch`](crate::storage::StorageBackend::put_batch),
//! which the redb backend commits in one transaction.
//!
//! ## Free-function API
//!
//! Following [`crate::fts::index`], the store is a set of free functions
//! over `&dyn StorageBackend` rather than a struct — it owns no state
//! beyond the keys it writes, so there is nothing to construct.

use crate::error::Result;
use crate::storage::StorageBackend;
use crate::vector::{HnswConfig, HnswIndex, Vector};

/// Key prefix for persisted embeddings: `vec:{node_id_le8}` -> bincode(Vector).
const PREFIX_VECTOR: &[u8] = b"vec:";

/// Bincode configuration — must match the rest of the codebase
/// ([`crate::db`]) so payloads are wire-compatible.
const BINCODE_CONFIG: bincode::config::Configuration = bincode::config::standard();

/// Build the storage key for a node's embedding: `vec:{id_le8}`.
fn vector_key(node_id: u64) -> Vec<u8> {
    let mut key = PREFIX_VECTOR.to_vec();
    key.extend_from_slice(&node_id.to_le_bytes());
    key
}

/// Decode the node id from a `vec:` key, or `None` if it is malformed.
fn node_id_from_key(key: &[u8]) -> Option<u64> {
    let suffix = key.strip_prefix(PREFIX_VECTOR)?;
    let arr: [u8; 8] = suffix.try_into().ok()?;
    Some(u64::from_le_bytes(arr))
}

/// Persist a single embedding for `node_id`, overwriting any prior value.
///
/// # Errors
///
/// Returns [`crate::error::DrevoError::Encode`] if the vector cannot be
/// serialized, or [`crate::error::DrevoError::Storage`] on backend failure.
pub fn put(backend: &dyn StorageBackend, node_id: u64, vector: &Vector) -> Result<()> {
    let bytes = bincode::serde::encode_to_vec(vector, BINCODE_CONFIG)?;
    backend.put(&vector_key(node_id), &bytes)?;
    Ok(())
}

/// Persist many embeddings as one logical operation.
///
/// On the redb backend this commits in a single write transaction (one
/// `fsync` for the whole batch) via [`StorageBackend::put_batch`], which
/// is the point of the batch API: bulk embedding ingest is the common
/// case and per-row transactions do not scale.
///
/// # Errors
///
/// Returns [`crate::error::DrevoError::Encode`] if any vector cannot be
/// serialized, or [`crate::error::DrevoError::Storage`] on backend failure.
pub fn put_batch(backend: &dyn StorageBackend, items: &[(u64, Vector)]) -> Result<()> {
    let mut encoded = Vec::with_capacity(items.len());
    for (node_id, vector) in items {
        let bytes = bincode::serde::encode_to_vec(vector, BINCODE_CONFIG)?;
        encoded.push((vector_key(*node_id), bytes));
    }
    backend.put_batch(&encoded)?;
    Ok(())
}

/// Fetch the embedding stored for `node_id`, or `None` if absent.
///
/// # Errors
///
/// Returns [`crate::error::DrevoError::Decode`] if the stored bytes are
/// corrupt, or [`crate::error::DrevoError::Storage`] on backend failure.
pub fn get(backend: &dyn StorageBackend, node_id: u64) -> Result<Option<Vector>> {
    match backend.get(&vector_key(node_id))? {
        Some(bytes) => {
            let (vector, _) = bincode::serde::decode_from_slice(&bytes, BINCODE_CONFIG)?;
            Ok(Some(vector))
        }
        None => Ok(None),
    }
}

/// Remove the embedding stored for `node_id`. Absent keys are a no-op.
///
/// # Errors
///
/// Returns [`crate::error::DrevoError::Storage`] on backend failure.
pub fn delete(backend: &dyn StorageBackend, node_id: u64) -> Result<()> {
    backend.delete(&vector_key(node_id))?;
    Ok(())
}

/// Count the embeddings currently persisted.
///
/// # Errors
///
/// Returns [`crate::error::DrevoError::Storage`] on backend failure.
pub fn count(backend: &dyn StorageBackend) -> Result<usize> {
    Ok(backend.scan_prefix(PREFIX_VECTOR)?.len())
}

/// Load every persisted embedding as `(node_id, Vector)` pairs, ordered
/// by node id (the natural byte order of the little-endian key suffix).
///
/// Entries whose key or payload is malformed are skipped rather than
/// aborting the whole scan — a single corrupt row must not make the
/// index un-rebuildable.
///
/// # Errors
///
/// Returns [`crate::error::DrevoError::Storage`] on backend failure.
pub fn scan_all(backend: &dyn StorageBackend) -> Result<Vec<(u64, Vector)>> {
    let entries = backend.scan_prefix(PREFIX_VECTOR)?;
    let mut out = Vec::with_capacity(entries.len());
    for (key, bytes) in entries {
        let Some(node_id) = node_id_from_key(&key) else {
            continue;
        };
        let Ok((vector, _)) =
            bincode::serde::decode_from_slice::<Vector, _>(&bytes, BINCODE_CONFIG)
        else {
            continue;
        };
        out.push((node_id, vector));
    }
    Ok(out)
}

/// Rebuild an in-memory [`HnswIndex`] from every persisted embedding.
///
/// This is the persistence → query-acceleration bridge: the HNSW graph
/// (`00076`) is in-memory only, so after a reopen it is reconstructed by
/// replaying the durable vectors through [`HnswIndex::insert`]. The
/// resulting index keys neighbors by node id, matching [`scan_all`].
///
/// Vectors are inserted in node-id order for a deterministic graph shape
/// given a fixed [`HnswConfig`] seed.
///
/// # Errors
///
/// Returns [`crate::error::DrevoError::Storage`] on backend failure, or
/// [`crate::error::DrevoError::Vector`] if a persisted embedding cannot
/// be inserted (e.g. a dimension mismatch against the first vector).
pub fn build_hnsw(backend: &dyn StorageBackend, config: HnswConfig) -> Result<HnswIndex> {
    let mut index = HnswIndex::new(config);
    for (node_id, vector) in scan_all(backend)? {
        index.insert(node_id, vector)?;
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryBackend;

    fn backend() -> MemoryBackend {
        MemoryBackend::new()
    }

    #[test]
    fn vector_key_round_trips_node_id() {
        let key = vector_key(42);
        assert!(key.starts_with(PREFIX_VECTOR));
        assert_eq!(node_id_from_key(&key), Some(42));
    }

    #[test]
    fn node_id_from_malformed_key_is_none() {
        assert_eq!(node_id_from_key(b"other:1234"), None);
        assert_eq!(node_id_from_key(b"vec:short"), None);
    }

    #[test]
    fn put_then_get_returns_same_vector() {
        let b = backend();
        let v = Vector::from(vec![1.0, 2.0, 3.0]);
        put(&b, 7, &v).unwrap();
        assert_eq!(get(&b, 7).unwrap(), Some(v));
    }

    #[test]
    fn get_absent_is_none() {
        let b = backend();
        assert_eq!(get(&b, 99).unwrap(), None);
    }

    #[test]
    fn put_overwrites() {
        let b = backend();
        put(&b, 1, &Vector::from(vec![1.0, 0.0])).unwrap();
        put(&b, 1, &Vector::from(vec![0.0, 1.0])).unwrap();
        assert_eq!(get(&b, 1).unwrap(), Some(Vector::from(vec![0.0, 1.0])));
    }

    #[test]
    fn delete_removes_vector() {
        let b = backend();
        put(&b, 5, &Vector::from(vec![1.0])).unwrap();
        delete(&b, 5).unwrap();
        assert_eq!(get(&b, 5).unwrap(), None);
    }

    #[test]
    fn delete_absent_is_noop() {
        let b = backend();
        delete(&b, 123).unwrap();
    }

    #[test]
    fn count_reflects_inserts_and_deletes() {
        let b = backend();
        assert_eq!(count(&b).unwrap(), 0);
        put(&b, 1, &Vector::from(vec![1.0])).unwrap();
        put(&b, 2, &Vector::from(vec![2.0])).unwrap();
        assert_eq!(count(&b).unwrap(), 2);
        delete(&b, 1).unwrap();
        assert_eq!(count(&b).unwrap(), 1);
    }

    #[test]
    fn put_batch_inserts_all() {
        let b = backend();
        let items = vec![
            (10, Vector::from(vec![1.0, 0.0])),
            (20, Vector::from(vec![0.0, 1.0])),
            (30, Vector::from(vec![1.0, 1.0])),
        ];
        put_batch(&b, &items).unwrap();
        assert_eq!(count(&b).unwrap(), 3);
        assert_eq!(get(&b, 20).unwrap(), Some(Vector::from(vec![0.0, 1.0])));
    }

    #[test]
    fn put_batch_empty_is_noop() {
        let b = backend();
        put_batch(&b, &[]).unwrap();
        assert_eq!(count(&b).unwrap(), 0);
    }

    #[test]
    fn scan_all_is_ordered_by_node_id() {
        let b = backend();
        put(&b, 3, &Vector::from(vec![3.0])).unwrap();
        put(&b, 1, &Vector::from(vec![1.0])).unwrap();
        put(&b, 2, &Vector::from(vec![2.0])).unwrap();
        let all = scan_all(&b).unwrap();
        let ids: Vec<u64> = all.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn build_hnsw_recovers_all_vectors() {
        let b = backend();
        put_batch(
            &b,
            &[
                (1, Vector::from(vec![1.0, 0.0, 0.0])),
                (2, Vector::from(vec![0.0, 1.0, 0.0])),
                (3, Vector::from(vec![0.0, 0.0, 1.0])),
            ],
        )
        .unwrap();
        let index = build_hnsw(&b, HnswConfig::default()).unwrap();
        assert_eq!(index.len(), 3);
        // The nearest neighbor of [1,0,0] is node 1 itself.
        let hits = index.search(&[1.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(hits[0].key, 1);
    }

    #[test]
    fn build_hnsw_on_empty_store_is_empty() {
        let b = backend();
        let index = build_hnsw(&b, HnswConfig::default()).unwrap();
        assert!(index.is_empty());
    }

    #[test]
    fn build_hnsw_surfaces_dimension_mismatch() {
        let b = backend();
        put(&b, 1, &Vector::from(vec![1.0, 2.0])).unwrap();
        put(&b, 2, &Vector::from(vec![1.0, 2.0, 3.0])).unwrap();
        assert!(build_hnsw(&b, HnswConfig::default()).is_err());
    }
}
