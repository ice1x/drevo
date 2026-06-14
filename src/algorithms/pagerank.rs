//! PageRank centrality via weighted power iteration — Phase 15 task `00098`.
//!
//! Computes the stationary distribution of the random-surfer Markov chain over
//! the directed graph: with probability `damping` the surfer follows an
//! out-edge (chosen proportionally to edge weight), and with probability
//! `1 - damping` it teleports to a uniformly random node. Dangling nodes (no
//! out-edges, or only zero-weight out-edges) redistribute their rank uniformly
//! across all nodes, so the total rank mass is conserved at `1.0` every
//! iteration.

use super::{AdjacencyList, AlgorithmError};

/// Configuration for [`pagerank`].
///
/// Construct with [`PageRankConfig::new`] (validated) or
/// [`PageRankConfig::default`] (the canonical `damping = 0.85`,
/// `max_iterations = 100`, `tolerance = 1e-6`).
#[derive(Debug, Clone, Copy, PartialEq)]
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
}

impl Default for PageRankConfig {
    fn default() -> Self {
        Self {
            damping: 0.85,
            max_iterations: 100,
            tolerance: 1e-6,
        }
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
/// yields an empty result that reports `converged = true`.
pub fn pagerank(graph: &AdjacencyList, config: &PageRankConfig) -> PageRankResult {
    let n = graph.node_count();
    if n == 0 {
        return PageRankResult {
            ranks: Vec::new(),
            iterations: 0,
            converged: true,
        };
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
}
