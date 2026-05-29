//! Integration tests for the HNSW index — Phase 12 task `00076`.
//!
//! Exercises the public `drevo::vector::HnswIndex` surface the way the joint
//! graph+vector Cypher predicate (`00077`) and the persistence layer
//! (`00078`) will: build an index keyed by node ids, query it for the nearest
//! embeddings, and confirm the approximate result tracks the exact
//! brute-force answer across realistic shapes (high dimensions, many
//! elements, both metrics). Bad inputs must surface as recoverable
//! [`VectorError`]s rather than panics, matching the distance kernels.

use std::collections::HashSet;

use drevo::vector::{
    euclidean_distance, HnswConfig, HnswIndex, Metric, Neighbor, Vector, VectorError,
};

/// A tiny, dependency-free xorshift generator so the tests build reproducible
/// random data without pulling in `rand`.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f32 / (1u64 << 53) as f32
    }
    fn vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.next_f32() - 0.5).collect()
    }
}

/// Exact top-`k` by Euclidean distance — the ground truth HNSW approximates.
fn brute_force(data: &[(u64, Vec<f32>)], query: &[f32], k: usize) -> Vec<u64> {
    let mut scored: Vec<(u64, f32)> = data
        .iter()
        .map(|(key, v)| (*key, euclidean_distance(query, v).unwrap()))
        .collect();
    scored.sort_by(|a, b| a.1.total_cmp(&b.1));
    scored.into_iter().take(k).map(|(key, _)| key).collect()
}

#[test]
fn builds_and_queries_a_small_knowledge_graph() {
    // Five "concept" nodes embedded in 3-space; a query near the first should
    // return it plus its nearest direction-mate. This mirrors how 00077 will
    // resolve `similar(n.embedding, $q, ...)` over node ids.
    let mut index = HnswIndex::new(HnswConfig::default());
    index
        .insert(101, Vector::from(vec![1.0, 0.0, 0.0]))
        .unwrap();
    index
        .insert(102, Vector::from(vec![0.0, 1.0, 0.0]))
        .unwrap();
    index
        .insert(103, Vector::from(vec![0.0, 0.0, 1.0]))
        .unwrap();
    index
        .insert(104, Vector::from(vec![0.95, 0.05, 0.0]))
        .unwrap();
    index
        .insert(105, Vector::from(vec![-1.0, 0.0, 0.0]))
        .unwrap();

    let hits = index.search(&[1.0, 0.0, 0.0], 2).unwrap();
    let keys: Vec<u64> = hits.iter().map(|h| h.key).collect();
    assert_eq!(keys, vec![101, 104]);
}

#[test]
fn high_recall_on_high_dimensional_data() {
    // 384-dim embeddings (a common sentence-embedding size), 500 of them.
    // HNSW's top-10 must overlap the exact top-10 by at least 9/10.
    let dim = 384;
    let n = 500u64;
    let mut rng = Rng::new(0xDEAD_BEEF);
    let mut index = HnswIndex::new(HnswConfig {
        m: 16,
        ef_construction: 200,
        seed: 99,
        metric: Metric::Euclidean,
    });
    let mut data = Vec::new();
    for i in 0..n {
        let v = rng.vector(dim);
        data.push((i, v.clone()));
        index.insert(i, Vector::from(v)).unwrap();
    }
    assert_eq!(index.len(), n as usize);
    assert_eq!(index.dim(), Some(dim));

    let query = rng.vector(dim);
    let k = 10;
    let truth: HashSet<u64> = brute_force(&data, &query, k).into_iter().collect();
    let got: HashSet<u64> = index
        .search_with_ef(&query, k, 128)
        .unwrap()
        .into_iter()
        .map(|h| h.key)
        .collect();
    let overlap = truth.intersection(&got).count();
    assert!(overlap >= 9, "recall@{k} = {overlap}/{k}");
}

#[test]
fn cosine_metric_finds_same_direction_regardless_of_magnitude() {
    // Under cosine, a long vector pointing the same way as the query beats a
    // short vector pointing elsewhere — magnitude must not matter.
    let mut index = HnswIndex::new(HnswConfig {
        metric: Metric::Cosine,
        ..HnswConfig::default()
    });
    index.insert(1, Vector::from(vec![100.0, 0.0])).unwrap(); // same direction, big
    index.insert(2, Vector::from(vec![0.1, 0.1])).unwrap(); // 45°, small
    index.insert(3, Vector::from(vec![0.0, 5.0])).unwrap(); // orthogonal

    let hits = index.search(&[1.0, 0.0], 3).unwrap();
    assert_eq!(hits[0].key, 1, "same-direction vector ranks first");
    assert!(hits[0].distance < 1e-5, "cosine distance ~0 for parallel");
    assert_eq!(hits[2].key, 3, "orthogonal vector ranks last");
}

#[test]
fn two_indexes_with_same_seed_and_order_are_identical() {
    let build = |seed: u64| {
        let mut rng = Rng::new(2024);
        let mut index = HnswIndex::new(HnswConfig {
            m: 12,
            ef_construction: 100,
            seed,
            metric: Metric::Cosine,
        });
        for i in 0..120u64 {
            index.insert(i, Vector::from(rng.vector(32))).unwrap();
        }
        let q = Rng::new(2024).vector(32);
        index.search(&q, 15).unwrap()
    };
    // Identical seed → identical graph → identical results.
    assert_eq!(build(5), build(5));
}

#[test]
fn different_seeds_still_return_correct_neighbors() {
    // Seed changes the internal graph but not correctness: the exact nearest
    // neighbor of a query that coincides with a stored point is that point.
    for seed in [1u64, 2, 3, 100] {
        let mut index = HnswIndex::new(HnswConfig {
            seed,
            ..HnswConfig::default()
        });
        let mut rng = Rng::new(seed.wrapping_mul(7) | 1);
        let mut target = Vec::new();
        for i in 0..80u64 {
            let v = rng.vector(24);
            if i == 40 {
                target = v.clone();
            }
            index.insert(i, Vector::from(v)).unwrap();
        }
        let hits = index.search(&target, 1).unwrap();
        assert_eq!(hits[0].key, 40, "seed={seed}");
    }
}

#[test]
fn incremental_inserts_keep_serving_queries() {
    // The index must answer queries correctly at every size as it grows, not
    // only after a final build step.
    let mut index = HnswIndex::new(HnswConfig::default());
    let mut rng = Rng::new(7);
    // A fixed non-zero query (cosine is undefined for a zero-magnitude vector).
    let query = vec![0.3, -0.1, 0.5, 0.2, -0.4, 0.1, 0.0, 0.6];
    for i in 0..60u64 {
        index.insert(i, Vector::from(rng.vector(8))).unwrap();
        let hits = index.search(&query, (i as usize + 1).min(5)).unwrap();
        assert!(!hits.is_empty());
        assert!(hits.len() <= 5);
        // Results are always sorted by ascending distance.
        for w in hits.windows(2) {
            assert!(w[0].distance <= w[1].distance);
        }
    }
}

#[test]
fn errors_are_recoverable_not_panics() {
    let mut index = HnswIndex::new(HnswConfig::default());
    assert_eq!(
        index.insert(1, Vector::from(Vec::<f32>::new())),
        Err(VectorError::Empty)
    );
    index.insert(1, Vector::from(vec![1.0, 2.0, 3.0])).unwrap();
    assert_eq!(
        index.insert(2, Vector::from(vec![1.0, 2.0])),
        Err(VectorError::DimensionMismatch { left: 3, right: 2 })
    );
    assert_eq!(
        index.search(&[1.0, 2.0], 3),
        Err(VectorError::DimensionMismatch { left: 3, right: 2 })
    );
    assert_eq!(index.search(&[], 3), Err(VectorError::Empty));
}

#[test]
fn neighbor_struct_carries_key_and_distance() {
    let mut index = HnswIndex::new(HnswConfig {
        metric: Metric::Euclidean,
        ..HnswConfig::default()
    });
    index.insert(42, Vector::from(vec![3.0, 4.0])).unwrap();
    let hits = index.search(&[0.0, 0.0], 1).unwrap();
    let expected = Neighbor {
        key: 42,
        distance: 5.0,
    };
    assert_eq!(hits[0].key, expected.key);
    assert!((hits[0].distance - expected.distance).abs() < 1e-5);
}
