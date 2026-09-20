//! Vector index construction (Phase 12 task `00078`, issue #446).
//!
//! Historically this module also held a durable `vec:` keyspace over the KV
//! `StorageBackend`; that persistence layer was removed with the KV engine
//! (epic #444). The native engine keeps its own durable embedding store and
//! feeds the engine-agnostic `build_hnsw_from` builder below, which turns any
//! source of `(node_id, Vector)` pairs into an in-memory [`HnswIndex`].

use crate::error::Result;
use crate::vector::{HnswConfig, HnswIndex, Vector};

/// Build an in-memory [`HnswIndex`] from any source of `(node_id, Vector)`
/// pairs, inserting in iterator order. Engine-agnostic: the native engine feeds
/// it its own durable-embedding scan. Callers that need a deterministic graph
/// shape for a fixed [`HnswConfig`] seed must yield ids in ascending order.
///
/// # Errors
///
/// Returns [`crate::error::DrevoError::Vector`] if a vector cannot be inserted
/// (e.g. a dimension mismatch against the first).
pub fn build_hnsw_from<I>(vectors: I, config: HnswConfig) -> Result<HnswIndex>
where
    I: IntoIterator<Item = (u64, Vector)>,
{
    let mut index = HnswIndex::new(config);
    for (node_id, vector) in vectors {
        index.insert(node_id, vector)?;
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_hnsw_from_iterator_is_backend_agnostic() {
        // Feed (id, Vector) pairs from any source — here a plain Vec, standing
        // in for the native engine's own embedding scan (issue #446).
        let vectors = vec![
            (1u64, Vector::from(vec![1.0, 0.0, 0.0])),
            (2, Vector::from(vec![0.0, 1.0, 0.0])),
            (3, Vector::from(vec![0.0, 0.0, 1.0])),
        ];
        let index = build_hnsw_from(vectors, HnswConfig::default()).unwrap();
        assert_eq!(index.len(), 3);
        assert_eq!(index.search(&[1.0, 0.0, 0.0], 1).unwrap()[0].key, 1);
    }

    #[test]
    fn build_hnsw_from_empty_iterator_is_empty() {
        let index =
            build_hnsw_from(std::iter::empty::<(u64, Vector)>(), HnswConfig::default()).unwrap();
        assert!(index.is_empty());
    }

    #[test]
    fn build_hnsw_from_surfaces_dimension_mismatch() {
        let vectors = vec![
            (1u64, Vector::from(vec![1.0, 2.0])),
            (2, Vector::from(vec![1.0, 2.0, 3.0])),
        ];
        assert!(build_hnsw_from(vectors, HnswConfig::default()).is_err());
    }
}
