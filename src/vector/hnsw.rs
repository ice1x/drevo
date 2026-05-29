//! Phase 12 task `00076` — an in-memory HNSW approximate-nearest-neighbor
//! index over [`Vector`](crate::vector::Vector) embeddings.
//!
//! HNSW (Hierarchical Navigable Small World, Malkov & Yashunin 2016) is the
//! index the joint graph+vector Cypher predicate (`00077`) will query when a
//! `similar(n.embedding, $q, 0.85)` clause needs the top-`k` neighbors of a
//! query embedding without scanning every node. It is a multi-layer
//! proximity graph: the top layers are sparse "express lanes" that let a
//! greedy walk cover distance quickly, and layer 0 holds every element with
//! dense local connectivity for an accurate final sweep. Search descends the
//! layers greedily and finishes with a beam (`ef`) search on layer 0; insert
//! does the same descent, then wires the new element bidirectionally to its
//! nearest neighbors on each layer it occupies.
//!
//! ## What this task delivers (and what it does not)
//!
//! This is the pure in-memory index: build it, insert `(key, vector)` pairs,
//! query it for the nearest keys. The `key` is a [`u64`] so it can carry a
//! [`Node::id`](crate::model::Node::id) directly. Persisting the graph to
//! redb (`00078`) and the Cypher surface (`00077`) layer on top of this type
//! and are intentionally out of scope here.
//!
//! ## Determinism
//!
//! HNSW assigns each element a random maximum layer drawn from an exponential
//! distribution. A real ANN index seeds that from the OS RNG, but a database
//! index must be reproducible: the same inserts in the same order must build
//! the same graph so tests can assert exact recall and so a future on-disk
//! format round-trips. The index therefore carries its own seeded
//! [`splitmix64`](https://prng.di.unimi.it/splitmix64.c) generator
//! ([`HnswConfig::seed`]) rather than touching global randomness — and it
//! pulls in no RNG dependency, matching the dependency-free stance of the
//! rest of [`crate::vector`].
//!
//! ## Metric
//!
//! HNSW orders candidates by a *distance* (smaller = nearer). Cosine
//! *similarity* runs the other way, so [`Metric::Cosine`] is stored as
//! `1 - cosine_similarity` to turn "most similar" into "least distant". All
//! measures reuse the validated kernels in
//! [`distance`](crate::vector::distance), so a dimension mismatch or a
//! zero-magnitude vector surfaces as a recoverable
//! [`VectorError`](crate::vector::VectorError) exactly as it does there.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};

use super::{cosine_similarity, euclidean_distance, Vector, VectorError};

/// Which distance the index orders neighbors by.
///
/// Both variants present as a *distance* (smaller = nearer) to the index;
/// [`Metric::Cosine`] converts the underlying similarity with `1 - cos θ`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// `1 - cosine_similarity`, in `[0, 2]`. The default for normalized
    /// embedding models, where direction carries the meaning and magnitude
    /// does not.
    Cosine,
    /// Euclidean (L2) distance, in `[0, ∞)`. Use when magnitude matters.
    Euclidean,
}

impl Metric {
    /// Distance between two equal-length embeddings under this metric.
    ///
    /// # Errors
    ///
    /// Propagates any [`VectorError`] from the underlying kernel —
    /// [`VectorError::Empty`], [`VectorError::DimensionMismatch`], or (for
    /// [`Metric::Cosine`]) [`VectorError::ZeroVector`].
    fn distance(self, a: &[f32], b: &[f32]) -> Result<f32, VectorError> {
        match self {
            Metric::Cosine => cosine_similarity(a, b).map(|s| 1.0 - s),
            Metric::Euclidean => euclidean_distance(a, b),
        }
    }
}

/// Tuning parameters for an [`HnswIndex`].
///
/// The defaults (`m = 16`, `ef_construction = 200`) are the values the HNSW
/// paper and common libraries (`hnswlib`, `faiss`) use as general-purpose
/// settings; they trade a little build time for high recall.
#[derive(Debug, Clone, Copy)]
pub struct HnswConfig {
    /// Target number of bidirectional links per element on layers above 0.
    /// Layer 0 keeps up to `2 * m` so the densest layer can afford a wider
    /// neighborhood. Larger `m` raises recall and memory.
    pub m: usize,
    /// Beam width used while *building* the graph. Larger values explore
    /// more candidates per insert, producing better links at higher build
    /// cost. Also the default beam width for [`HnswIndex::search`].
    pub ef_construction: usize,
    /// Seed for the internal level-assignment RNG. Two indexes built with
    /// the same seed and the same insert order are byte-for-byte identical.
    pub seed: u64,
    /// Distance the index orders neighbors by.
    pub metric: Metric,
}

impl Default for HnswConfig {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 200,
            seed: 0x9E37_79B9_7F4A_7C15,
            metric: Metric::Cosine,
        }
    }
}

/// A single hit returned by [`HnswIndex::search`]: the stored key and its
/// distance to the query under the index's [`Metric`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Neighbor {
    /// The key supplied at [`HnswIndex::insert`] time (typically a
    /// [`Node::id`](crate::model::Node::id)).
    pub key: u64,
    /// Distance to the query under the index [`Metric`] — smaller is nearer.
    pub distance: f32,
}

/// One element of the proximity graph: its key, embedding, and the
/// adjacency lists that place it on each layer it occupies.
struct HnswNode {
    key: u64,
    vector: Vector,
    /// `neighbors[layer]` holds the internal indices this element links to
    /// on `layer`. Length is `level + 1`, so the element appears on layers
    /// `0..=level`.
    neighbors: Vec<Vec<usize>>,
}

impl HnswNode {
    /// Highest layer this element appears on.
    fn level(&self) -> usize {
        self.neighbors.len() - 1
    }
}

/// A `(distance, internal-id)` pair ordered by distance, used in the
/// candidate and result heaps during a layer search.
///
/// `f32` is not [`Ord`] (NaN), so ordering goes through
/// [`f32::total_cmp`]; distances here are always finite (the kernels reject
/// non-finite inputs), so the total order is the natural numeric one. Ties
/// break on the id to keep the order deterministic.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    dist: f32,
    id: usize,
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.dist
            .total_cmp(&other.dist)
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// An in-memory HNSW approximate-nearest-neighbor index.
///
/// Build it with [`HnswIndex::new`], add embeddings with
/// [`HnswIndex::insert`], and query the nearest keys with
/// [`HnswIndex::search`]. See the [module docs](self) for the algorithm and
/// the determinism guarantee.
///
/// ```
/// use drevo::vector::{HnswIndex, HnswConfig, Vector};
///
/// let mut index = HnswIndex::new(HnswConfig::default());
/// index.insert(1, Vector::from(vec![1.0, 0.0, 0.0])).unwrap();
/// index.insert(2, Vector::from(vec![0.0, 1.0, 0.0])).unwrap();
/// index.insert(3, Vector::from(vec![0.9, 0.1, 0.0])).unwrap();
///
/// // Query closest to node 1's direction; node 3 points almost the same way.
/// let hits = index.search(&[1.0, 0.0, 0.0], 2).unwrap();
/// assert_eq!(hits[0].key, 1);
/// assert_eq!(hits[1].key, 3);
/// ```
pub struct HnswIndex {
    config: HnswConfig,
    /// Reciprocal of `ln(m)`: the level-distribution normalizer `mL` from the
    /// paper. Precomputed so `random_level` avoids a `ln` per insert.
    level_mult: f64,
    nodes: Vec<HnswNode>,
    /// Internal index of the current top-layer entry point, or `None` while
    /// the index is empty.
    entry: Option<usize>,
    /// Embedding dimension, fixed by the first insert; later inserts must
    /// match it.
    dim: Option<usize>,
    /// Current state of the seeded splitmix64 RNG for level assignment.
    rng_state: u64,
}

impl HnswIndex {
    /// Create an empty index with the given configuration.
    #[must_use]
    pub fn new(config: HnswConfig) -> Self {
        // mL = 1 / ln(m); m == 1 would make every element land on layer 0,
        // degrading to a flat graph, so clamp the log base to ≥ 2.
        let level_mult = 1.0 / (config.m.max(2) as f64).ln();
        let rng_state = config.seed;
        Self {
            config,
            level_mult,
            nodes: Vec::new(),
            entry: None,
            dim: None,
            rng_state,
        }
    }

    /// Number of elements in the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the index holds no elements.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Embedding dimension the index is locked to, or `None` before the
    /// first insert.
    #[must_use]
    pub fn dim(&self) -> Option<usize> {
        self.dim
    }

    /// Insert an embedding under `key`.
    ///
    /// The first insert fixes the index dimension; every later insert must
    /// match it. Keys are not deduplicated — inserting the same key twice
    /// stores two elements, and a search may return either.
    ///
    /// # Errors
    ///
    /// * [`VectorError::Empty`] — `vector` has zero dimensions.
    /// * [`VectorError::DimensionMismatch`] — `vector`'s length differs from
    ///   the dimension fixed by the first insert.
    pub fn insert(&mut self, key: u64, vector: Vector) -> Result<(), VectorError> {
        if vector.is_empty() {
            return Err(VectorError::Empty);
        }
        match self.dim {
            None => self.dim = Some(vector.dim()),
            Some(d) if d != vector.dim() => {
                return Err(VectorError::DimensionMismatch {
                    left: d,
                    right: vector.dim(),
                });
            }
            Some(_) => {}
        }

        let level = self.random_level();
        let new_id = self.nodes.len();
        self.nodes.push(HnswNode {
            key,
            vector,
            neighbors: vec![Vec::new(); level + 1],
        });

        // First element ever: it becomes the entry point and links to
        // nothing.
        let Some(mut entry) = self.entry else {
            self.entry = Some(new_id);
            return Ok(());
        };

        let query = self.nodes[new_id].vector.0.clone();
        let entry_level = self.nodes[entry].level();

        // Phase 1 — greedy descent through the layers ABOVE the new element's
        // top layer, narrowing the entry point to the closest element found.
        let mut lc = entry_level;
        while lc > level {
            entry = self.greedy_closest(&query, entry, lc)?;
            lc -= 1;
        }

        // Phase 2 — on each layer the new element occupies, beam-search for
        // candidates, pick its neighbors, and wire the links both ways.
        let m = self.config.m;
        let top = level.min(entry_level);
        let mut lc = top as isize;
        while lc >= 0 {
            let layer = lc as usize;
            // Layer 0 keeps a wider neighborhood (2·m) than the express layers.
            let m_max = if layer == 0 { m * 2 } else { m };
            let found = self.search_layer(&query, &[entry], self.config.ef_construction, layer)?;
            let selected = self.select_neighbors(&found, m);

            for &neighbor in &selected {
                self.nodes[new_id].neighbors[layer].push(neighbor);
                self.nodes[neighbor].neighbors[layer].push(new_id);
                self.prune(neighbor, layer, m_max)?;
            }

            // Descend from the closest candidate on this layer.
            if let Some(closest) = found.first() {
                entry = closest.id;
            }
            lc -= 1;
        }

        // The new element reaches higher than any existing one: it is the new
        // top-layer entry point.
        if level > entry_level {
            self.entry = Some(new_id);
        }
        Ok(())
    }

    /// Return the `k` nearest keys to `query`, ascending by distance.
    ///
    /// Uses [`HnswConfig::ef_construction`] as the beam width; for finer
    /// control over the recall/latency trade-off use
    /// [`HnswIndex::search_with_ef`]. Returns fewer than `k` hits when the
    /// index holds fewer than `k` elements, and an empty `Vec` when the index
    /// is empty.
    ///
    /// # Errors
    ///
    /// * [`VectorError::Empty`] — `query` has zero dimensions.
    /// * [`VectorError::DimensionMismatch`] — `query`'s length differs from
    ///   the index dimension.
    /// * [`VectorError::ZeroVector`] — under [`Metric::Cosine`], `query` (or a
    ///   stored vector) has zero magnitude.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<Neighbor>, VectorError> {
        self.search_with_ef(query, k, self.config.ef_construction.max(k))
    }

    /// Return the `k` nearest keys to `query` using an explicit beam width
    /// `ef`, ascending by distance.
    ///
    /// `ef` is the size of the dynamic candidate list on layer 0: it must be
    /// at least `k`, and larger values raise recall at the cost of visiting
    /// more elements. It is clamped up to `k` if a smaller value is passed.
    ///
    /// # Errors
    ///
    /// See [`HnswIndex::search`].
    pub fn search_with_ef(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
    ) -> Result<Vec<Neighbor>, VectorError> {
        if query.is_empty() {
            return Err(VectorError::Empty);
        }
        if let Some(d) = self.dim {
            if d != query.len() {
                return Err(VectorError::DimensionMismatch {
                    left: d,
                    right: query.len(),
                });
            }
        }
        let Some(mut entry) = self.entry else {
            return Ok(Vec::new());
        };
        if k == 0 {
            return Ok(Vec::new());
        }

        // Greedy descent through every layer above 0 to land near the answer.
        let entry_level = self.nodes[entry].level();
        let mut lc = entry_level;
        while lc > 0 {
            entry = self.greedy_closest(query, entry, lc)?;
            lc -= 1;
        }

        // Beam search on the dense bottom layer, then keep the k closest.
        let ef = ef.max(k);
        let mut found = self.search_layer(query, &[entry], ef, 0)?;
        found.truncate(k);
        Ok(found
            .into_iter()
            .map(|c| Neighbor {
                key: self.nodes[c.id].key,
                distance: c.dist,
            })
            .collect())
    }

    /// Greedy single-best walk on `layer`: from `start`, repeatedly hop to the
    /// closest neighbor until no neighbor is closer than the current node, and
    /// return that local minimum. Used to descend the express layers where a
    /// full beam search is unnecessary.
    fn greedy_closest(
        &self,
        query: &[f32],
        start: usize,
        layer: usize,
    ) -> Result<usize, VectorError> {
        let mut current = start;
        let mut current_dist = self.dist_to(query, current)?;
        loop {
            let mut improved = false;
            for &neighbor in &self.nodes[current].neighbors[layer] {
                let d = self.dist_to(query, neighbor)?;
                if d < current_dist {
                    current_dist = d;
                    current = neighbor;
                    improved = true;
                }
            }
            if !improved {
                return Ok(current);
            }
        }
    }

    /// Beam search on a single `layer`: explore the proximity graph from the
    /// `entry_points`, keeping the `ef` closest elements seen. Returns them
    /// sorted ascending by distance.
    ///
    /// This is the workhorse of both insert and query — Algorithm 2 from the
    /// HNSW paper. `candidates` is a min-heap of frontier elements to expand;
    /// `results` is a max-heap capped at `ef` so its peek is the current
    /// farthest keeper, letting the loop stop once the frontier can no longer
    /// beat it.
    fn search_layer(
        &self,
        query: &[f32],
        entry_points: &[usize],
        ef: usize,
        layer: usize,
    ) -> Result<Vec<Candidate>, VectorError> {
        let mut visited: HashSet<usize> = HashSet::with_capacity(ef * 4);
        // Min-heap of the frontier: Reverse flips BinaryHeap's max-order so
        // pop() yields the *closest* unexpanded candidate.
        let mut candidates: BinaryHeap<std::cmp::Reverse<Candidate>> = BinaryHeap::new();
        // Max-heap of the best ef found: peek() is the farthest keeper.
        let mut results: BinaryHeap<Candidate> = BinaryHeap::new();

        for &ep in entry_points {
            if visited.insert(ep) {
                let c = Candidate {
                    dist: self.dist_to(query, ep)?,
                    id: ep,
                };
                candidates.push(std::cmp::Reverse(c));
                results.push(c);
            }
        }
        // Trim seed set down to ef before exploring.
        while results.len() > ef {
            results.pop();
        }

        while let Some(std::cmp::Reverse(c)) = candidates.pop() {
            // Once the closest frontier element is farther than the farthest
            // keeper, no unexpanded element can improve the result.
            let farthest = results.peek().map_or(f32::INFINITY, |f| f.dist);
            if c.dist > farthest && results.len() >= ef {
                break;
            }
            // Clone the neighbor list to drop the borrow on `self.nodes`
            // before the distance calls below.
            let neighbors = self.nodes[c.id].neighbors[layer].clone();
            for neighbor in neighbors {
                if visited.insert(neighbor) {
                    let d = self.dist_to(query, neighbor)?;
                    let farthest = results.peek().map_or(f32::INFINITY, |f| f.dist);
                    if d < farthest || results.len() < ef {
                        let cand = Candidate {
                            dist: d,
                            id: neighbor,
                        };
                        candidates.push(std::cmp::Reverse(cand));
                        results.push(cand);
                        if results.len() > ef {
                            results.pop();
                        }
                    }
                }
            }
        }

        let mut out: Vec<Candidate> = results.into_vec();
        out.sort_unstable();
        Ok(out)
    }

    /// Pick the `m` closest of an already-distance-sorted candidate list.
    ///
    /// This is the simple neighbor-selection heuristic from the paper
    /// (Algorithm 3, `keepPrunedConnections = false`, no distance-diversity
    /// filtering). It is enough for the recall this task targets; the
    /// diversity heuristic is a later tuning concern.
    fn select_neighbors(&self, candidates: &[Candidate], m: usize) -> Vec<usize> {
        candidates.iter().take(m).map(|c| c.id).collect()
    }

    /// After adding a link to `node`, drop its weakest connections on `layer`
    /// so it keeps at most `m_max` neighbors — the closest ones.
    fn prune(&mut self, node: usize, layer: usize, m_max: usize) -> Result<(), VectorError> {
        if self.nodes[node].neighbors[layer].len() <= m_max {
            return Ok(());
        }
        let base = self.nodes[node].vector.0.clone();
        let mut scored: Vec<Candidate> = self.nodes[node].neighbors[layer]
            .iter()
            .map(|&n| {
                Ok(Candidate {
                    dist: self.dist_to(&base, n)?,
                    id: n,
                })
            })
            .collect::<Result<_, VectorError>>()?;
        scored.sort_unstable();
        scored.truncate(m_max);
        self.nodes[node].neighbors[layer] = scored.into_iter().map(|c| c.id).collect();
        Ok(())
    }

    /// Distance from `query` to the stored embedding of internal element
    /// `id`, under the index metric.
    fn dist_to(&self, query: &[f32], id: usize) -> Result<f32, VectorError> {
        self.config.metric.distance(query, &self.nodes[id].vector.0)
    }

    /// Draw a maximum layer from the exponential level distribution
    /// `floor(-ln(U) · mL)`, advancing the seeded RNG. Higher layers are
    /// exponentially rarer, which is what gives HNSW its logarithmic search.
    fn random_level(&mut self) -> usize {
        let u = self.next_unit();
        (-u.ln() * self.level_mult).floor() as usize
    }

    /// Next pseudo-random `f64` in the half-open interval `(0, 1]`, from the
    /// seeded splitmix64 generator. The upper bound is inclusive and the
    /// lower bound excluded so `ln(u)` in [`random_level`] never sees 0.
    fn next_unit(&mut self) -> f64 {
        // splitmix64 — a tiny, well-distributed integer generator.
        self.rng_state = self.rng_state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // Take the top 53 bits for a uniform double in [0, 1), then shift to
        // (0, 1] so the log is always defined.
        let unit = ((z >> 11) as f64) / ((1u64 << 53) as f64);
        1.0 - unit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> HnswConfig {
        HnswConfig {
            m: 8,
            ef_construction: 64,
            seed: 42,
            metric: Metric::Cosine,
        }
    }

    #[test]
    fn empty_index_searches_to_nothing() {
        let index = HnswIndex::new(cfg());
        assert!(index.is_empty());
        assert_eq!(index.len(), 0);
        assert_eq!(index.dim(), None);
        assert_eq!(index.search(&[1.0, 0.0], 5).unwrap(), Vec::new());
    }

    #[test]
    fn single_element_returns_itself() {
        let mut index = HnswIndex::new(cfg());
        index.insert(7, Vector::from(vec![1.0, 2.0, 3.0])).unwrap();
        let hits = index.search(&[1.0, 2.0, 3.0], 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, 7);
        assert!(hits[0].distance.abs() < 1e-5);
    }

    #[test]
    fn first_insert_fixes_dimension() {
        let mut index = HnswIndex::new(cfg());
        index.insert(1, Vector::from(vec![1.0, 0.0])).unwrap();
        assert_eq!(index.dim(), Some(2));
        let err = index.insert(2, Vector::from(vec![1.0, 0.0, 0.0]));
        assert_eq!(
            err,
            Err(VectorError::DimensionMismatch { left: 2, right: 3 })
        );
    }

    #[test]
    fn empty_vector_rejected_on_insert_and_search() {
        let mut index = HnswIndex::new(cfg());
        assert_eq!(
            index.insert(1, Vector::from(Vec::<f32>::new())),
            Err(VectorError::Empty)
        );
        index.insert(1, Vector::from(vec![1.0, 0.0])).unwrap();
        assert_eq!(index.search(&[], 3), Err(VectorError::Empty));
    }

    #[test]
    fn search_dimension_mismatch_errors() {
        let mut index = HnswIndex::new(cfg());
        index.insert(1, Vector::from(vec![1.0, 0.0, 0.0])).unwrap();
        assert_eq!(
            index.search(&[1.0, 0.0], 3),
            Err(VectorError::DimensionMismatch { left: 3, right: 2 })
        );
    }

    #[test]
    fn ranks_neighbors_by_proximity() {
        let mut index = HnswIndex::new(cfg());
        index.insert(1, Vector::from(vec![1.0, 0.0, 0.0])).unwrap();
        index.insert(2, Vector::from(vec![0.0, 1.0, 0.0])).unwrap();
        index.insert(3, Vector::from(vec![0.9, 0.1, 0.0])).unwrap();
        index.insert(4, Vector::from(vec![0.0, 0.0, 1.0])).unwrap();

        let hits = index.search(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].key, 1, "exact match first");
        assert_eq!(hits[1].key, 3, "near-parallel vector second");
        // Distances must be non-decreasing.
        assert!(hits[0].distance <= hits[1].distance);
    }

    #[test]
    fn k_larger_than_size_returns_all() {
        let mut index = HnswIndex::new(cfg());
        for i in 0..5u64 {
            index
                .insert(i, Vector::from(vec![i as f32, 1.0, 0.0]))
                .unwrap();
        }
        let hits = index.search(&[0.0, 1.0, 0.0], 50).unwrap();
        assert_eq!(hits.len(), 5);
    }

    #[test]
    fn k_zero_returns_empty() {
        let mut index = HnswIndex::new(cfg());
        index.insert(1, Vector::from(vec![1.0, 0.0])).unwrap();
        assert_eq!(index.search(&[1.0, 0.0], 0).unwrap(), Vec::new());
    }

    #[test]
    fn euclidean_metric_orders_by_l2() {
        let mut index = HnswIndex::new(HnswConfig {
            metric: Metric::Euclidean,
            ..cfg()
        });
        index.insert(1, Vector::from(vec![0.0, 0.0])).unwrap();
        index.insert(2, Vector::from(vec![10.0, 10.0])).unwrap();
        index.insert(3, Vector::from(vec![1.0, 1.0])).unwrap();

        let hits = index.search(&[0.0, 0.0], 3).unwrap();
        assert_eq!(hits[0].key, 1);
        assert_eq!(hits[1].key, 3);
        assert_eq!(hits[2].key, 2);
    }

    #[test]
    fn deterministic_under_fixed_seed() {
        let build = || {
            let mut index = HnswIndex::new(cfg());
            for i in 0..50u64 {
                let v = vec![(i as f32).sin(), (i as f32).cos(), (i % 7) as f32];
                index.insert(i, Vector::from(v)).unwrap();
            }
            index.search(&[0.1, 0.9, 2.0], 10).unwrap()
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn recall_matches_brute_force_on_random_data() {
        // Build an index over pseudo-random vectors and confirm HNSW's top-10
        // overlaps the exact brute-force top-10 by a high margin. This is the
        // property the phase DoD ultimately gates on.
        let dim = 16;
        let n = 400u64;
        let mut index = HnswIndex::new(HnswConfig {
            m: 16,
            ef_construction: 200,
            seed: 7,
            metric: Metric::Euclidean,
        });
        let mut data: Vec<(u64, Vec<f32>)> = Vec::new();
        let mut s = 123_456_789u64;
        let mut rnd = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f32 / (1u64 << 53) as f32
        };
        for i in 0..n {
            let v: Vec<f32> = (0..dim).map(|_| rnd()).collect();
            data.push((i, v.clone()));
            index.insert(i, Vector::from(v)).unwrap();
        }

        let query: Vec<f32> = (0..dim).map(|_| rnd()).collect();
        let k = 10;

        // Exact ground truth.
        let mut exact: Vec<(u64, f32)> = data
            .iter()
            .map(|(key, v)| (*key, euclidean_distance(&query, v).unwrap()))
            .collect();
        exact.sort_by(|a, b| a.1.total_cmp(&b.1));
        let truth: HashSet<u64> = exact.iter().take(k).map(|(key, _)| *key).collect();

        let hits = index.search_with_ef(&query, k, 100).unwrap();
        let got: HashSet<u64> = hits.iter().map(|h| h.key).collect();
        let overlap = truth.intersection(&got).count();
        assert!(
            overlap >= 9,
            "recall@{k} too low: {overlap}/{k} (truth={truth:?}, got={got:?})"
        );
    }

    #[test]
    fn distances_are_non_decreasing() {
        let mut index = HnswIndex::new(cfg());
        for i in 0..30u64 {
            let v = vec![(i as f32) * 0.1, 1.0 - (i as f32) * 0.01, 0.5];
            index.insert(i, Vector::from(v)).unwrap();
        }
        let hits = index.search(&[1.0, 0.5, 0.5], 10).unwrap();
        for w in hits.windows(2) {
            assert!(w[0].distance <= w[1].distance);
        }
    }

    #[test]
    fn duplicate_keys_are_allowed() {
        let mut index = HnswIndex::new(cfg());
        index.insert(1, Vector::from(vec![1.0, 0.0])).unwrap();
        index.insert(1, Vector::from(vec![1.0, 0.0])).unwrap();
        assert_eq!(index.len(), 2);
        let hits = index.search(&[1.0, 0.0], 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.key == 1));
    }
}
