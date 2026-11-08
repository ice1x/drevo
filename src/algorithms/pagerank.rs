//! PageRank centrality via weighted power iteration — Phase 15 task `00098`.
//!
//! Computes the stationary distribution of the random-surfer Markov chain over
//! the directed graph: with probability `damping` the surfer follows an
//! out-edge (chosen proportionally to edge weight), and with probability
//! `1 - damping` it teleports to a uniformly random node. Dangling nodes (no
//! out-edges, or only zero-weight out-edges) redistribute their rank uniformly
//! across all nodes, so the total rank mass is conserved at `1.0` every
//! iteration.
//!
//! With a [`Personalization`] the walk is **Personalized PageRank**: the
//! surfer teleports to the source nodes (in proportion to their weights)
//! instead of to a uniformly random node, and dangling mass returns to the
//! sources too. Ranks then measure closeness to the sources: nodes the
//! sources cannot reach score exactly `0`.

use std::collections::HashMap;

use super::{AdjacencyList, AlgorithmError};

/// Configuration for [`pagerank`].
///
/// Construct with [`PageRankConfig::new`] (validated) or
/// [`PageRankConfig::default`] (the canonical `damping = 0.85`,
/// `max_iterations = 100`, `tolerance = 1e-6`, no personalization), and set
/// [`personalization`](Self::personalization) for Personalized PageRank.
#[derive(Debug, Clone, PartialEq)]
pub struct PageRankConfig {
    /// Damping factor `d` — the probability of following a link rather than
    /// teleporting. Must be in the open interval `(0, 1)`. The classic value
    /// is `0.85`.
    pub damping: f64,
    /// Maximum number of power-iteration steps before giving up on
    /// convergence. Must be at least `1`.
    pub max_iterations: usize,
    /// Convergence tolerance: iteration stops once the L1 norm of the change
    /// in the rank vector between two steps falls to or below this value.
    /// Must be finite and non-negative.
    pub tolerance: f64,
    /// The teleport set of Personalized PageRank; `None` teleports to every
    /// node uniformly (classic PageRank).
    pub personalization: Option<Personalization>,
}

impl Default for PageRankConfig {
    fn default() -> Self {
        Self {
            damping: 0.85,
            max_iterations: 100,
            tolerance: 1e-6,
            personalization: None,
        }
    }
}

/// The teleport set of Personalized PageRank: source node ids and one
/// non-negative weight per source.
///
/// Weights are normalised to sum to `1` when the walk runs, so `[1, 3]` and
/// `[10, 30]` are the same teleport vector. A source listed twice adds its
/// weights; a source that is not in the graph is ignored.
///
/// ```
/// use drevo::algorithms::{pagerank, AdjacencyList, PageRankConfig, Personalization};
///
/// // 1 → 2 → 3, and 4 on its own.
/// let g = AdjacencyList::from_parts(vec![1, 2, 3, 4], vec![(1, 2, 1.0), (2, 3, 1.0)]);
/// let config = PageRankConfig {
///     personalization: Some(Personalization::uniform(vec![1]).unwrap()),
///     ..PageRankConfig::default()
/// };
/// let ranked = pagerank(&g, &config).ranked();
/// assert_eq!(ranked[0].0, 1); // the source ranks first
/// assert_eq!(ranked[3], (4, 0.0)); // 4 is unreachable from 1
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Personalization {
    nodes: Vec<u64>,
    weights: Vec<f64>,
}

impl Personalization {
    /// A teleport set with one weight per source node.
    ///
    /// # Errors
    ///
    /// - [`AlgorithmError::EmptyPersonalization`] if `nodes` is empty.
    /// - [`AlgorithmError::PersonalizationLengthMismatch`] if `weights` does
    ///   not have one entry per node.
    /// - [`AlgorithmError::InvalidPersonalizationWeight`] if a weight is
    ///   negative or not finite.
    /// - [`AlgorithmError::ZeroPersonalization`] if every weight is `0`.
    pub fn new(nodes: Vec<u64>, weights: Vec<f64>) -> Result<Self, AlgorithmError> {
        if nodes.is_empty() {
            return Err(AlgorithmError::EmptyPersonalization);
        }
        if nodes.len() != weights.len() {
            return Err(AlgorithmError::PersonalizationLengthMismatch {
                nodes: nodes.len(),
                weights: weights.len(),
            });
        }
        if let Some(&bad) = weights.iter().find(|w| !(w.is_finite() && **w >= 0.0)) {
            return Err(AlgorithmError::InvalidPersonalizationWeight(bad));
        }
        if weights.iter().all(|&w| w == 0.0) {
            return Err(AlgorithmError::ZeroPersonalization);
        }
        Ok(Self { nodes, weights })
    }

    /// A teleport set that weights every source node equally.
    ///
    /// # Errors
    ///
    /// [`AlgorithmError::EmptyPersonalization`] if `nodes` is empty.
    pub fn uniform(nodes: Vec<u64>) -> Result<Self, AlgorithmError> {
        let weights = vec![1.0; nodes.len()];
        Self::new(nodes, weights)
    }

    /// The source node ids, in the order given.
    pub fn nodes(&self) -> &[u64] {
        &self.nodes
    }

    /// The weights, one per source, as given (not normalised).
    pub fn weights(&self) -> &[f64] {
        &self.weights
    }

    /// The dense teleport vector over `graph`'s nodes, summing to `1`, or
    /// `None` when no source with a positive weight is in the graph.
    fn teleport(&self, graph: &AdjacencyList) -> Option<Vec<f64>> {
        let index: HashMap<u64, usize> = graph
            .nodes()
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, i))
            .collect();
        let mut t = vec![0.0; graph.node_count()];
        let mut total = 0.0;
        for (id, &w) in self.nodes.iter().zip(&self.weights) {
            if let Some(&i) = index.get(id) {
                t[i] += w;
                total += w;
            }
        }
        if total <= 0.0 {
            return None;
        }
        for slot in &mut t {
            *slot /= total;
        }
        Some(t)
    }
}

impl PageRankConfig {
    /// Build a validated config.
    ///
    /// # Errors
    ///
    /// - [`AlgorithmError::InvalidDamping`] if `damping` is not in `(0, 1)`.
    /// - [`AlgorithmError::InvalidIterations`] if `max_iterations` is `0`.
    /// - [`AlgorithmError::InvalidTolerance`] if `tolerance` is negative or
    ///   not finite.
    pub fn new(
        damping: f64,
        max_iterations: usize,
        tolerance: f64,
    ) -> Result<Self, AlgorithmError> {
        if !(damping.is_finite() && damping > 0.0 && damping < 1.0) {
            return Err(AlgorithmError::InvalidDamping(damping));
        }
        if max_iterations == 0 {
            return Err(AlgorithmError::InvalidIterations(max_iterations));
        }
        if !(tolerance.is_finite() && tolerance >= 0.0) {
            return Err(AlgorithmError::InvalidTolerance(tolerance));
        }
        Ok(Self {
            damping,
            max_iterations,
            tolerance,
            personalization: None,
        })
    }
}

/// The result of a [`pagerank`] run.
#[derive(Debug, Clone, PartialEq)]
pub struct PageRankResult {
    /// `(node_id, rank)` pairs in the snapshot's node order. Ranks sum to
    /// `1.0` (within floating-point tolerance) for a non-empty graph.
    pub ranks: Vec<(u64, f64)>,
    /// Number of power-iteration steps actually performed.
    pub iterations: usize,
    /// `true` if the L1 change fell to or below `tolerance` before hitting
    /// `max_iterations`.
    pub converged: bool,
}

impl PageRankResult {
    /// The `(node_id, rank)` pairs sorted by descending rank (ties broken by
    /// ascending node ID) — the natural "most central nodes first" ordering.
    pub fn ranked(&self) -> Vec<(u64, f64)> {
        let mut v = self.ranks.clone();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }
}

/// Run weighted PageRank over `graph`.
///
/// Always terminates in at most `config.max_iterations` steps. An empty graph
/// yields an empty result that reports `converged = true`. With
/// `config.personalization` set, teleport and dangling mass go to the
/// sources (Personalized PageRank); when none of the sources is in the
/// graph, every node scores `0` and no step runs.
pub fn pagerank(graph: &AdjacencyList, config: &PageRankConfig) -> PageRankResult {
    let n = graph.node_count();
    if n == 0 {
        return PageRankResult {
            ranks: Vec::new(),
            iterations: 0,
            converged: true,
        };
    }
    if let Some(p) = &config.personalization {
        return personalized(graph, config, p, Sweep::Serial);
    }

    let nf = n as f64;
    let d = config.damping;

    // Pre-compute each node's total out-weight. A node with a total of 0 is
    // "dangling" and redistributes its rank uniformly.
    let out_weight: Vec<f64> = (0..n)
        .map(|i| graph.out_edges(i).iter().map(|&(_, w)| w).sum::<f64>())
        .collect();

    let mut rank = vec![1.0 / nf; n];
    let mut next = vec![0.0; n];

    let mut iterations = 0;
    let mut converged = false;
    while iterations < config.max_iterations {
        iterations += 1;

        // Mass that leaks out of dangling nodes this step, redistributed
        // uniformly so total rank stays at 1.0.
        let dangling: f64 = (0..n)
            .filter(|&i| out_weight[i] <= 0.0)
            .map(|i| rank[i])
            .sum();

        let base = (1.0 - d) / nf + d * dangling / nf;
        for slot in next.iter_mut() {
            *slot = base;
        }

        for i in 0..n {
            if out_weight[i] <= 0.0 {
                continue;
            }
            let share = d * rank[i] / out_weight[i];
            for &(j, w) in graph.out_edges(i) {
                next[j] += share * w;
            }
        }

        let delta: f64 = (0..n).map(|i| (next[i] - rank[i]).abs()).sum();
        std::mem::swap(&mut rank, &mut next);

        if delta <= config.tolerance {
            converged = true;
            break;
        }
    }

    let ranks = (0..n).map(|i| (graph.id_at(i), rank[i])).collect();
    PageRankResult {
        ranks,
        iterations,
        converged,
    }
}

/// Parallel PageRank over an immutable adjacency snapshot (RFC #307 Phase 8 —
/// parallel runtime).
///
/// **Measured slower than [`pagerank`], not faster**, and the CSR layout did
/// not change that: [`AdjacencyList`] is now Compressed-Sparse-Row internally
/// (#382), yet `benches/pagerank_bench.rs` re-measured this at ~17× slower at
/// 10k nodes, ~8× at 100k, and ~4.4× even at 500k / 3M edges. PageRank is
/// memory-bandwidth-bound, so the per-iteration rayon fork/join and the
/// pull-based reverse index cost more than the extra cores save — more threads
/// do not help. Kept as the correct baseline the bench compares against; a real
/// parallel win would need a fundamentally different scheme (NUMA-aware block
/// partitioning), not just the CSR layout (see `docs/native-load.md`, #382). The
/// user-facing paths therefore call the serial [`pagerank`]. Correctness is
/// identical (pinned by `tests/native_pagerank_tests.rs`).
///
/// Same math as [`pagerank`], but **pull-based**: each step recomputes every
/// node's rank from its *incoming* edges, which makes the per-node update of a
/// step independent, so it parallelises with rayon (`next.par_iter_mut()`) —
/// the "free over the MVCC snapshot" win, since the frozen snapshot is shared
/// immutably across workers. Bit-for-bit equality with [`pagerank`] is not
/// guaranteed (the parallel reduction sums in a different order), but the ranks
/// agree within tolerance — pinned by `pagerank_parallel_matches_serial`.
///
/// Dangling nodes (zero out-weight) redistribute uniformly, identically to the
/// serial version: their contribution is excluded from the pull sum (they have
/// no usable outgoing mass) and folded into the teleport `base` instead.
///
/// On `wasm32` (no threads) this falls back to the serial [`pagerank`]; the API
/// is the same everywhere.
#[cfg(not(target_arch = "wasm32"))]
pub fn pagerank_parallel(graph: &AdjacencyList, config: &PageRankConfig) -> PageRankResult {
    use rayon::prelude::*;

    let n = graph.node_count();
    if n == 0 {
        return PageRankResult {
            ranks: Vec::new(),
            iterations: 0,
            converged: true,
        };
    }
    if let Some(p) = &config.personalization {
        return personalized(graph, config, p, Sweep::Parallel);
    }

    let nf = n as f64;
    let d = config.damping;

    let out_weight: Vec<f64> = (0..n)
        .map(|i| graph.out_edges(i).iter().map(|&(_, w)| w).sum::<f64>())
        .collect();

    // Reverse index (built once): `in_edges[j]` = every `(source i, weight)`
    // with an edge i→j. Only sources with usable out-weight are kept, so the
    // per-node pull never divides by a zero out-weight (dangling sources leak
    // through `base`, matching the serial scatter's `continue`).
    let mut in_edges: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    for (i, &ow) in out_weight.iter().enumerate() {
        if ow <= 0.0 {
            continue;
        }
        for &(j, w) in graph.out_edges(i) {
            in_edges[j].push((i, w));
        }
    }

    let mut rank = vec![1.0 / nf; n];
    let mut next = vec![0.0; n];
    let mut iterations = 0;
    let mut converged = false;

    while iterations < config.max_iterations {
        iterations += 1;

        let dangling: f64 = (0..n)
            .into_par_iter()
            .filter(|&i| out_weight[i] <= 0.0)
            .map(|i| rank[i])
            .sum();
        let base = (1.0 - d) / nf + d * dangling / nf;

        let rank_ref = &rank;
        let in_ref = &in_edges;
        let ow_ref = &out_weight;
        next.par_iter_mut().enumerate().for_each(|(j, slot)| {
            let inflow: f64 = in_ref[j]
                .iter()
                .map(|&(i, w)| d * rank_ref[i] * w / ow_ref[i])
                .sum();
            *slot = base + inflow;
        });

        let delta: f64 = (0..n)
            .into_par_iter()
            .map(|i| (next[i] - rank[i]).abs())
            .sum();
        std::mem::swap(&mut rank, &mut next);

        if delta <= config.tolerance {
            converged = true;
            break;
        }
    }

    let ranks = (0..n).map(|i| (graph.id_at(i), rank[i])).collect();
    PageRankResult {
        ranks,
        iterations,
        converged,
    }
}

/// `wasm32` has no thread pool — run the serial power iteration instead, so the
/// `pagerank_parallel` API is available on every target.
#[cfg(target_arch = "wasm32")]
pub fn pagerank_parallel(graph: &AdjacencyList, config: &PageRankConfig) -> PageRankResult {
    match &config.personalization {
        Some(p) if graph.node_count() > 0 => personalized(graph, config, p, Sweep::Parallel),
        _ => pagerank(graph, config),
    }
}

/// How a Personalized PageRank step is computed.
#[derive(Clone, Copy)]
enum Sweep {
    /// Push each node's rank along its out-edges, one node at a time.
    Serial,
    /// Pull each node's rank from its in-edges, nodes in parallel (rayon).
    Parallel,
}

/// Personalized PageRank: the power iteration of [`pagerank`] with the
/// teleport vector `t` of `p` in place of the uniform `1/n`, so each step is
/// `next = (1 − d)·t + d·dangling·t + d·Σ inflow`.
///
/// The walk starts at `t`, and mass only ever flows along edges or back to
/// the sources, so a node the sources cannot reach stays at exactly `0`.
fn personalized(
    graph: &AdjacencyList,
    config: &PageRankConfig,
    p: &Personalization,
    sweep: Sweep,
) -> PageRankResult {
    let n = graph.node_count();
    let Some(teleport) = p.teleport(graph) else {
        return PageRankResult {
            ranks: (0..n).map(|i| (graph.id_at(i), 0.0)).collect(),
            iterations: 0,
            converged: true,
        };
    };
    let d = config.damping;
    let out_weight: Vec<f64> = (0..n)
        .map(|i| graph.out_edges(i).iter().map(|&(_, w)| w).sum::<f64>())
        .collect();
    let in_edges = match sweep {
        Sweep::Serial => Vec::new(),
        Sweep::Parallel => in_edges(graph, &out_weight),
    };

    let mut rank = teleport.clone();
    let mut next = vec![0.0; n];
    let mut iterations = 0;
    let mut converged = false;
    while iterations < config.max_iterations {
        iterations += 1;
        let dangling: f64 = (0..n)
            .filter(|&i| out_weight[i] <= 0.0)
            .map(|i| rank[i])
            .sum();
        // Teleport and dangling mass both return to the sources.
        let back = (1.0 - d) + d * dangling;
        match sweep {
            Sweep::Serial => {
                for (slot, &t) in next.iter_mut().zip(&teleport) {
                    *slot = back * t;
                }
                for i in 0..n {
                    if out_weight[i] <= 0.0 {
                        continue;
                    }
                    let share = d * rank[i] / out_weight[i];
                    for &(j, w) in graph.out_edges(i) {
                        next[j] += share * w;
                    }
                }
            }
            Sweep::Parallel => {
                pull_step(&mut next, &rank, &teleport, &in_edges, &out_weight, d, back)
            }
        }
        let delta: f64 = (0..n).map(|i| (next[i] - rank[i]).abs()).sum();
        std::mem::swap(&mut rank, &mut next);
        if delta <= config.tolerance {
            converged = true;
            break;
        }
    }

    let ranks = (0..n).map(|i| (graph.id_at(i), rank[i])).collect();
    PageRankResult {
        ranks,
        iterations,
        converged,
    }
}

/// The reverse index of `graph`: `in_edges[j]` = every `(source i, weight)`
/// with an edge `i → j` from a node with usable out-weight.
fn in_edges(graph: &AdjacencyList, out_weight: &[f64]) -> Vec<Vec<(usize, f64)>> {
    let mut in_edges: Vec<Vec<(usize, f64)>> = vec![Vec::new(); graph.node_count()];
    for (i, &ow) in out_weight.iter().enumerate() {
        if ow <= 0.0 {
            continue;
        }
        for &(j, w) in graph.out_edges(i) {
            in_edges[j].push((i, w));
        }
    }
    in_edges
}

/// One pull-based Personalized PageRank step, nodes in parallel.
#[cfg(not(target_arch = "wasm32"))]
fn pull_step(
    next: &mut [f64],
    rank: &[f64],
    teleport: &[f64],
    in_edges: &[Vec<(usize, f64)>],
    out_weight: &[f64],
    d: f64,
    back: f64,
) {
    use rayon::prelude::*;
    next.par_iter_mut().enumerate().for_each(|(j, slot)| {
        let inflow: f64 = in_edges[j]
            .iter()
            .map(|&(i, w)| d * rank[i] * w / out_weight[i])
            .sum();
        *slot = back * teleport[j] + inflow;
    });
}

/// `wasm32` has no thread pool: the same pull step, one node at a time.
#[cfg(target_arch = "wasm32")]
fn pull_step(
    next: &mut [f64],
    rank: &[f64],
    teleport: &[f64],
    in_edges: &[Vec<(usize, f64)>],
    out_weight: &[f64],
    d: f64,
    back: f64,
) {
    for (j, slot) in next.iter_mut().enumerate() {
        let inflow: f64 = in_edges[j]
            .iter()
            .map(|&(i, w)| d * rank[i] * w / out_weight[i])
            .sum();
        *slot = back * teleport[j] + inflow;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(r: &PageRankResult) -> f64 {
        r.ranks.iter().map(|&(_, v)| v).sum()
    }

    fn rank_of(r: &PageRankResult, id: u64) -> f64 {
        r.ranks.iter().find(|&&(i, _)| i == id).unwrap().1
    }

    #[test]
    fn config_rejects_out_of_range_damping() {
        assert_eq!(
            PageRankConfig::new(0.0, 100, 1e-6),
            Err(AlgorithmError::InvalidDamping(0.0))
        );
        assert_eq!(
            PageRankConfig::new(1.0, 100, 1e-6),
            Err(AlgorithmError::InvalidDamping(1.0))
        );
        assert!(PageRankConfig::new(0.85, 100, 1e-6).is_ok());
    }

    #[test]
    fn config_rejects_zero_iterations_and_bad_tolerance() {
        assert_eq!(
            PageRankConfig::new(0.85, 0, 1e-6),
            Err(AlgorithmError::InvalidIterations(0))
        );
        assert_eq!(
            PageRankConfig::new(0.85, 100, -1.0),
            Err(AlgorithmError::InvalidTolerance(-1.0))
        );
        assert!(matches!(
            PageRankConfig::new(0.85, 100, f64::NAN),
            Err(AlgorithmError::InvalidTolerance(_))
        ));
    }

    #[test]
    fn empty_graph_returns_empty_converged() {
        let g = AdjacencyList::from_parts(vec![], Vec::<(u64, u64, f32)>::new());
        let r = pagerank(&g, &PageRankConfig::default());
        assert!(r.ranks.is_empty());
        assert!(r.converged);
        assert_eq!(r.iterations, 0);
    }

    #[test]
    fn single_node_has_full_rank() {
        let g = AdjacencyList::from_parts(vec![1], Vec::<(u64, u64, f32)>::new());
        let r = pagerank(&g, &PageRankConfig::default());
        assert!((rank_of(&r, 1) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ranks_sum_to_one() {
        // A small directed graph with a dangling node (4 has no out-edges).
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![(1, 2, 1.0), (2, 3, 1.0), (3, 1, 1.0), (3, 4, 1.0)],
        );
        let r = pagerank(&g, &PageRankConfig::default());
        assert!((sum(&r) - 1.0).abs() < 1e-9, "sum was {}", sum(&r));
    }

    #[test]
    fn no_outlink_nodes_are_all_dangling_uniform() {
        // No edges at all -> every node dangling -> uniform 1/n.
        let g = AdjacencyList::from_parts(vec![1, 2, 3, 4], Vec::<(u64, u64, f32)>::new());
        let r = pagerank(&g, &PageRankConfig::default());
        for id in [1, 2, 3, 4] {
            assert!((rank_of(&r, id) - 0.25).abs() < 1e-9);
        }
        assert!(r.converged);
    }

    #[test]
    fn hub_node_outranks_leaves_in_star() {
        // A star where 1,2,3 all point at hub 4. The hub should rank highest.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![(1, 4, 1.0), (2, 4, 1.0), (3, 4, 1.0)],
        );
        let r = pagerank(&g, &PageRankConfig::default());
        let hub = rank_of(&r, 4);
        for leaf in [1, 2, 3] {
            assert!(hub > rank_of(&r, leaf), "hub {} not > leaf {}", hub, leaf);
        }
    }

    #[test]
    fn symmetric_cycle_is_uniform() {
        // 1->2->3->1 : perfectly symmetric, so all ranks equal.
        let g =
            AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 1.0), (2, 3, 1.0), (3, 1, 1.0)]);
        let r = pagerank(&g, &PageRankConfig::default());
        let expected = 1.0 / 3.0;
        for id in [1, 2, 3] {
            assert!((rank_of(&r, id) - expected).abs() < 1e-6);
        }
    }

    #[test]
    fn edge_weight_biases_distribution() {
        // 1 sends 9x more weight to 2 than to 3; 2 and 3 are otherwise
        // symmetric sinks, so 2 must end up ranked above 3.
        let g = AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 9.0), (1, 3, 1.0)]);
        let r = pagerank(&g, &PageRankConfig::default());
        assert!(rank_of(&r, 2) > rank_of(&r, 3));
    }

    #[test]
    fn ranked_orders_by_descending_rank() {
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![(1, 4, 1.0), (2, 4, 1.0), (3, 4, 1.0)],
        );
        let r = pagerank(&g, &PageRankConfig::default());
        let ranked = r.ranked();
        assert_eq!(ranked[0].0, 4); // hub first
                                    // descending order
        for w in ranked.windows(2) {
            assert!(w[0].1 >= w[1].1);
        }
    }

    #[test]
    fn tight_tolerance_runs_more_iterations_than_loose() {
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![
                (1, 2, 1.0),
                (2, 3, 1.0),
                (3, 4, 1.0),
                (4, 1, 0.5),
                (4, 2, 0.5),
            ],
        );
        let loose = pagerank(&g, &PageRankConfig::new(0.85, 100, 1e-2).unwrap());
        let tight = pagerank(&g, &PageRankConfig::new(0.85, 100, 1e-12).unwrap());
        assert!(tight.iterations >= loose.iterations);
    }

    #[test]
    fn respects_max_iterations_cap() {
        // An asymmetric graph whose stationary vector differs from the uniform
        // start, so one step cannot reach the fixed point.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![
                (1, 2, 1.0),
                (2, 3, 1.0),
                (3, 4, 1.0),
                (4, 1, 0.5),
                (4, 2, 0.5),
            ],
        );
        // 1 iteration with a zero tolerance -> cannot converge.
        let r = pagerank(&g, &PageRankConfig::new(0.85, 1, 0.0).unwrap());
        assert_eq!(r.iterations, 1);
        assert!(!r.converged);
    }

    // ── Personalized PageRank ───────────────────────────────────────────────

    fn personalized(nodes: Vec<u64>, weights: Vec<f64>) -> PageRankConfig {
        PageRankConfig {
            personalization: Some(Personalization::new(nodes, weights).unwrap()),
            ..PageRankConfig::default()
        }
    }

    fn from_sources(nodes: Vec<u64>) -> PageRankConfig {
        PageRankConfig {
            personalization: Some(Personalization::uniform(nodes).unwrap()),
            ..PageRankConfig::default()
        }
    }

    /// An undirected star: centre 1, leaves 2, 3, 4 (edges both ways).
    fn star() -> AdjacencyList {
        AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![
                (1, 2, 1.0),
                (2, 1, 1.0),
                (1, 3, 1.0),
                (3, 1, 1.0),
                (1, 4, 1.0),
                (4, 1, 1.0),
            ],
        )
    }

    #[test]
    fn personalization_rejects_bad_teleport_sets() {
        assert_eq!(
            Personalization::new(vec![], vec![]),
            Err(AlgorithmError::EmptyPersonalization)
        );
        assert_eq!(
            Personalization::new(vec![1, 2], vec![1.0]),
            Err(AlgorithmError::PersonalizationLengthMismatch {
                nodes: 2,
                weights: 1
            })
        );
        assert_eq!(
            Personalization::new(vec![1, 2], vec![1.0, -0.5]),
            Err(AlgorithmError::InvalidPersonalizationWeight(-0.5))
        );
        assert!(matches!(
            Personalization::new(vec![1], vec![f64::INFINITY]),
            Err(AlgorithmError::InvalidPersonalizationWeight(_))
        ));
        assert_eq!(
            Personalization::new(vec![1, 2], vec![0.0, 0.0]),
            Err(AlgorithmError::ZeroPersonalization)
        );
        assert_eq!(
            Personalization::uniform(vec![]),
            Err(AlgorithmError::EmptyPersonalization)
        );
    }

    #[test]
    fn ppr_on_a_star_ranks_the_source_first_and_depends_on_it() {
        // Directed star: centre 1 points at leaves 2, 3, 4.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![(1, 2, 1.0), (1, 3, 1.0), (1, 4, 1.0)],
        );
        let order = |r: &[(u64, f64)]| r.iter().map(|&(id, _)| id).collect::<Vec<_>>();
        let centre = pagerank(&g, &from_sources(vec![1])).ranked();
        let leaf = pagerank(&g, &from_sources(vec![2])).ranked();
        assert_eq!(order(&centre), vec![1, 2, 3, 4], "{centre:?}");
        assert_eq!(leaf[0].0, 2, "{leaf:?}");
        assert_ne!(order(&centre), order(&leaf));

        // Undirected star: from a leaf, the walk keeps passing through the
        // centre (the leaf's only neighbour), but the source still outranks
        // the other leaves, which tie.
        let from_leaf = pagerank(&star(), &from_sources(vec![2]));
        assert!(rank_of(&from_leaf, 2) > rank_of(&from_leaf, 3));
        assert_eq!(rank_of(&from_leaf, 3), rank_of(&from_leaf, 4));
        let from_centre = pagerank(&star(), &from_sources(vec![1])).ranked();
        assert_eq!(from_centre[0].0, 1, "{from_centre:?}");
    }

    #[test]
    fn ppr_gives_unreachable_nodes_exactly_zero() {
        // Component A = {1, 2, 3}, component B = {4, 5}.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4, 5],
            vec![
                (1, 2, 1.0),
                (2, 3, 1.0),
                (3, 1, 1.0),
                (4, 5, 1.0),
                (5, 4, 1.0),
            ],
        );
        let r = pagerank(&g, &from_sources(vec![1, 2]));
        assert_eq!(rank_of(&r, 4), 0.0);
        assert_eq!(rank_of(&r, 5), 0.0);
        let a: f64 = [1, 2, 3].iter().map(|&id| rank_of(&r, id)).sum();
        assert!((a - 1.0).abs() <= 1e-6, "A sums to {a}");
    }

    #[test]
    fn ppr_over_every_node_with_uniform_weights_is_global_pagerank() {
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4, 5],
            vec![
                (1, 2, 1.0),
                (2, 3, 2.0),
                (3, 1, 1.0),
                (3, 4, 1.0),
                (5, 1, 0.5),
            ],
        );
        let tight = PageRankConfig::new(0.85, 1000, 1e-14).unwrap();
        let global = pagerank(&g, &tight);
        let ppr = pagerank(
            &g,
            &PageRankConfig {
                personalization: Some(Personalization::uniform(vec![1, 2, 3, 4, 5]).unwrap()),
                ..tight.clone()
            },
        );
        for id in 1..=5 {
            assert!(
                (rank_of(&global, id) - rank_of(&ppr, id)).abs() < 1e-9,
                "node {id}: {} vs {}",
                rank_of(&global, id),
                rank_of(&ppr, id)
            );
        }
    }

    #[test]
    fn ppr_zero_weight_source_equals_dropping_it() {
        let g = star();
        let single = pagerank(&g, &from_sources(vec![2]));
        let weighted = pagerank(&g, &personalized(vec![2, 3], vec![1.0, 0.0]));
        assert_eq!(single.ranks, weighted.ranks);
        assert_eq!(single.iterations, weighted.iterations);
    }

    #[test]
    fn ppr_weights_are_normalised() {
        let g = star();
        let small = pagerank(&g, &personalized(vec![2, 3], vec![1.0, 3.0]));
        let large = pagerank(&g, &personalized(vec![2, 3], vec![10.0, 30.0]));
        for id in 1..=4 {
            assert!((rank_of(&small, id) - rank_of(&large, id)).abs() < 1e-12);
        }
        assert!(rank_of(&small, 3) > rank_of(&small, 2));
    }

    #[test]
    fn ppr_dangling_node_next_to_the_source_does_not_leak_mass() {
        // 2 has no out-edges: its mass returns to the source, not to 3.
        let g = AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 1.0), (3, 1, 1.0)]);
        let r = pagerank(&g, &from_sources(vec![1]));
        assert!((sum(&r) - 1.0).abs() < 1e-12, "sum {}", sum(&r));
        assert_eq!(rank_of(&r, 3), 0.0, "3 is unreachable from 1");
    }

    #[test]
    fn ppr_incoming_direction_on_a_chain_ranks_by_backwards_distance() {
        // a(1) → b(2) → c(3); from c against the edges: c, then b, then a.
        let edges = super::super::oriented_edges(
            vec![(1, 2, 1.0), (2, 3, 1.0)],
            crate::model::Direction::Incoming,
        );
        let g = AdjacencyList::from_parts(vec![1, 2, 3], edges);
        let r = pagerank(&g, &from_sources(vec![3]));
        let ranked: Vec<u64> = r.ranked().iter().map(|&(id, _)| id).collect();
        assert_eq!(ranked, vec![3, 2, 1]);
        assert!(rank_of(&r, 1) > 0.0);
        // Along the edges, c (3) reaches nothing: a and b stay at 0.
        let forward = AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 1.0), (2, 3, 1.0)]);
        let r = pagerank(&forward, &from_sources(vec![3]));
        assert_eq!((rank_of(&r, 1), rank_of(&r, 2)), (0.0, 0.0));
    }

    #[test]
    fn ppr_respects_max_iterations_and_stops_early_at_tolerance() {
        let g = star();
        let capped = pagerank(
            &g,
            &PageRankConfig {
                personalization: Some(Personalization::uniform(vec![2]).unwrap()),
                ..PageRankConfig::new(0.85, 3, 0.0).unwrap()
            },
        );
        assert_eq!(capped.iterations, 3);
        assert!(!capped.converged);
        let loose = pagerank(
            &g,
            &PageRankConfig {
                personalization: Some(Personalization::uniform(vec![2]).unwrap()),
                ..PageRankConfig::new(0.85, 1000, 1e-3).unwrap()
            },
        );
        assert!(loose.converged);
        assert!(loose.iterations < 1000, "{} iterations", loose.iterations);
    }

    #[test]
    fn ppr_ignores_sources_missing_from_the_graph_and_adds_duplicates() {
        let g = star();
        let plain = pagerank(&g, &from_sources(vec![2]));
        let with_ghost = pagerank(&g, &from_sources(vec![2, 99]));
        assert_eq!(plain.ranks, with_ghost.ranks);
        let doubled = pagerank(&g, &personalized(vec![2, 2, 3], vec![1.0, 1.0, 2.0]));
        let even = pagerank(&g, &personalized(vec![2, 3], vec![1.0, 1.0]));
        for id in 1..=4 {
            assert!((rank_of(&doubled, id) - rank_of(&even, id)).abs() < 1e-12);
        }
        // Every source missing: no walk at all, every node scores 0.
        let none = pagerank(&g, &from_sources(vec![99]));
        assert!(none.ranks.iter().all(|&(_, s)| s == 0.0), "{none:?}");
    }

    #[test]
    fn ppr_serial_and_parallel_agree() {
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4, 5, 6],
            vec![
                (1, 2, 1.0),
                (2, 3, 2.0),
                (3, 1, 1.0),
                (3, 4, 1.0),
                (4, 5, 1.0),
                (6, 1, 1.0),
            ],
        );
        let cfg = personalized(vec![1, 4], vec![2.0, 1.0]);
        let serial = pagerank(&g, &cfg);
        let parallel = pagerank_parallel(&g, &cfg);
        assert_eq!(serial.iterations, parallel.iterations);
        for id in 1..=6 {
            assert!(
                (rank_of(&serial, id) - rank_of(&parallel, id)).abs() < 1e-9,
                "node {id}"
            );
        }
        assert_eq!(rank_of(&parallel, 6), 0.0);
    }
}
