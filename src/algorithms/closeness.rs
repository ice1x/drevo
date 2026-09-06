//! Closeness centrality (harmonic) — RFC #307 Phase 8.
//!
//! Closeness measures how near a node is to every other node it can reach: a
//! high score means short routes to the rest of the graph. This module uses the
//! **harmonic** variant — the sum of reciprocal distances,
//! `C(v) = Σ_{u reachable, u≠v} 1 / d(v, u)` — rather than the classical
//! `(n-1) / Σ d(v,u)`. Harmonic closeness is well defined on **disconnected**
//! and **directed** graphs (an unreachable node simply contributes `1/∞ = 0`
//! instead of making the classical sum diverge), which a real knowledge graph
//! routinely is. It is a distinct lens from
//! [`betweenness`](crate::algorithms::betweenness) (brokerage) and
//! [`pagerank`](crate::algorithms::pagerank) (flow/popularity).
//!
//! Computed with one breadth-first sweep per node over the **directed,
//! unweighted** graph (hop count; edge weights ignored — the usual
//! knowledge-graph default), following edge direction (out-closeness), for
//! `O(V·(V+E))`. Parallel edges and self-loops do not affect distances and are
//! collapsed. Scores are structural, so the result is deterministic and
//! engine-independent.
//!
//! Dependency-free, infallible, always compiled, WASM-safe.

use std::collections::VecDeque;

use super::AdjacencyList;

/// The result of a [`closeness`] run.
#[derive(Debug, Clone, PartialEq)]
pub struct ClosenessResult {
    /// `(node_id, score)` pairs, most-central first and then by ascending node
    /// ID. `score` is the harmonic closeness `Σ 1/d` over all nodes reachable
    /// from the node along edge direction.
    pub scores: Vec<(u64, f64)>,
}

/// Compute harmonic closeness centrality for every node of `graph`.
///
/// Directed and unweighted: distances follow edge direction and count hops. An
/// unreachable node contributes `0` to the sum, so the metric is finite even on
/// a disconnected graph. Self-loops and parallel edges are ignored. An empty
/// graph yields an empty result; a graph with no edges yields all-zero scores.
pub fn closeness(graph: &AdjacencyList) -> ClosenessResult {
    let n = graph.node_count();
    if n == 0 {
        return ClosenessResult { scores: Vec::new() };
    }

    // De-duplicated directed successors, self-loops removed (they never shorten
    // a path between two distinct nodes).
    let succ: Vec<Vec<usize>> = (0..n)
        .map(|i| {
            let mut s: Vec<usize> = graph
                .out_edges(i)
                .iter()
                .map(|&(j, _)| j)
                .filter(|&j| j != i)
                .collect();
            s.sort_unstable();
            s.dedup();
            s
        })
        .collect();

    let mut harmonic = vec![0.0f64; n];
    for s in 0..n {
        let mut dist = vec![-1i64; n];
        dist[s] = 0;
        let mut queue: VecDeque<usize> = VecDeque::new();
        queue.push_back(s);
        let mut sum = 0.0f64;
        while let Some(v) = queue.pop_front() {
            for &w in &succ[v] {
                if dist[w] < 0 {
                    dist[w] = dist[v] + 1;
                    sum += 1.0 / dist[w] as f64;
                    queue.push_back(w);
                }
            }
        }
        harmonic[s] = sum;
    }

    let mut scores: Vec<(u64, f64)> = (0..n).map(|i| (graph.id_at(i), harmonic[i])).collect();
    // Most-central first; ties broken by ascending node ID for determinism.
    scores.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });

    ClosenessResult { scores }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score_of(r: &ClosenessResult, id: u64) -> f64 {
        r.scores
            .iter()
            .find(|&&(n, _)| n == id)
            .map(|&(_, s)| s)
            .unwrap()
    }

    #[test]
    fn empty_graph_has_no_scores() {
        let g = AdjacencyList::from_parts(vec![], Vec::<(u64, u64, f32)>::new());
        assert!(closeness(&g).scores.is_empty());
    }

    #[test]
    fn directed_path_sums_reciprocal_distances() {
        // 1 -> 2 -> 3: from 1, reach 2 at d=1 and 3 at d=2 → 1 + 1/2 = 1.5;
        // from 2 → 1.0; from 3 → 0 (reaches nothing).
        let g = AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 1.0), (2, 3, 1.0)]);
        let r = closeness(&g);
        assert!((score_of(&r, 1) - 1.5).abs() < 1e-12);
        assert!((score_of(&r, 2) - 1.0).abs() < 1e-12);
        assert_eq!(score_of(&r, 3), 0.0);
    }

    #[test]
    fn directed_triangle_is_symmetric() {
        // 1 -> 2 -> 3 -> 1: each node reaches the other two at d=1 and d=2 →
        // 1 + 1/2 = 1.5.
        let g =
            AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 1.0), (2, 3, 1.0), (3, 1, 1.0)]);
        let r = closeness(&g);
        for id in [1, 2, 3] {
            assert!((score_of(&r, id) - 1.5).abs() < 1e-12);
        }
    }

    #[test]
    fn a_hub_reaching_all_at_distance_one_is_most_central() {
        // 1 -> 2, 1 -> 3, 1 -> 4: node 1 reaches three nodes at d=1 → score 3.0,
        // the maximum; it is reported first.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![(1, 2, 1.0), (1, 3, 1.0), (1, 4, 1.0)],
        );
        let r = closeness(&g);
        assert_eq!(r.scores[0].0, 1);
        assert_eq!(score_of(&r, 1), 3.0);
        assert_eq!(score_of(&r, 2), 0.0);
    }

    #[test]
    fn disconnected_graph_stays_finite() {
        // Two separate edges 1 -> 2 and 3 -> 4: harmonic closeness is finite
        // (1.0 each for the sources) where classical closeness would divide by
        // an infinite distance sum.
        let g = AdjacencyList::from_parts(vec![1, 2, 3, 4], vec![(1, 2, 1.0), (3, 4, 1.0)]);
        let r = closeness(&g);
        assert_eq!(score_of(&r, 1), 1.0);
        assert_eq!(score_of(&r, 3), 1.0);
        assert_eq!(score_of(&r, 2), 0.0);
        assert_eq!(score_of(&r, 4), 0.0);
    }

    #[test]
    fn self_loops_and_parallel_edges_are_ignored() {
        // 1 -> 2 (twice), self-loop on 1, 2 -> 3: distances unchanged, so from 1
        // the score is 1 + 1/2 = 1.5.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3],
            vec![(1, 2, 1.0), (1, 2, 1.0), (1, 1, 1.0), (2, 3, 1.0)],
        );
        let r = closeness(&g);
        assert!((score_of(&r, 1) - 1.5).abs() < 1e-12);
    }
}
