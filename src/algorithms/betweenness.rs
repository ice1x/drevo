//! Betweenness centrality — RFC #307 Phase 8.
//!
//! The betweenness of a node is the number of shortest paths between other
//! pairs of nodes that pass **through** it (each pair's contribution split
//! evenly when it has several equally-shortest paths). It surfaces the brokers
//! and bottlenecks of a graph — the nodes whose removal would most lengthen or
//! sever routes — which centrality metrics like
//! [`pagerank`](crate::algorithms::pagerank) (a popularity/flow measure) do not
//! capture.
//!
//! Computed with **Brandes' algorithm** — a single-source shortest-path sweep
//! plus back-propagated dependency accumulation, `O(V·(V+E))` — over the
//! **directed, unweighted** graph (hop count, the usual knowledge-graph
//! default; edge weights are ignored). Parallel edges and self-loops do not
//! affect shortest paths and are collapsed. Scores are structural, so the
//! result is deterministic and engine-independent.
//!
//! Dependency-free, infallible, always compiled, WASM-safe.

use std::collections::VecDeque;

use super::AdjacencyList;

/// The result of a [`betweenness`] run.
#[derive(Debug, Clone, PartialEq)]
pub struct BetweennessResult {
    /// `(node_id, score)` pairs, most-central first and then by ascending node
    /// ID. `score` is the (un-normalised) number of shortest paths through the
    /// node, summed over all ordered source/target pairs.
    pub scores: Vec<(u64, f64)>,
}

/// Compute betweenness centrality for every node of `graph`.
///
/// Directed and unweighted: a shortest path follows edge direction and is
/// measured in hops. Self-loops and parallel edges are ignored (they never lie
/// on a shortest path between two distinct nodes). An empty graph yields an
/// empty result; a graph with no two-hop routes yields all-zero scores.
pub fn betweenness(graph: &AdjacencyList) -> BetweennessResult {
    let n = graph.node_count();
    if n == 0 {
        return BetweennessResult { scores: Vec::new() };
    }

    // De-duplicated directed successors, self-loops removed — Brandes needs a
    // simple graph so parallel edges don't inflate the path counts.
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

    let mut centrality = vec![0.0f64; n];

    for s in 0..n {
        // Single-source shortest-path counting from `s` (BFS, unit weights).
        let mut stack: Vec<usize> = Vec::new();
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut sigma = vec![0.0f64; n];
        sigma[s] = 1.0;
        let mut dist = vec![-1i64; n];
        dist[s] = 0;
        let mut queue: VecDeque<usize> = VecDeque::new();
        queue.push_back(s);

        while let Some(v) = queue.pop_front() {
            stack.push(v);
            for &w in &succ[v] {
                if dist[w] < 0 {
                    dist[w] = dist[v] + 1;
                    queue.push_back(w);
                }
                if dist[w] == dist[v] + 1 {
                    sigma[w] += sigma[v];
                    preds[w].push(v);
                }
            }
        }

        // Back-propagate dependencies in reverse BFS order.
        let mut delta = vec![0.0f64; n];
        while let Some(w) = stack.pop() {
            let coeff = (1.0 + delta[w]) / sigma[w];
            for &v in &preds[w] {
                delta[v] += sigma[v] * coeff;
            }
            if w != s {
                centrality[w] += delta[w];
            }
        }
    }

    let mut scores: Vec<(u64, f64)> = (0..n).map(|i| (graph.id_at(i), centrality[i])).collect();
    // Most-central first; ties broken by ascending node ID for determinism.
    scores.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });

    BetweennessResult { scores }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score_of(r: &BetweennessResult, id: u64) -> f64 {
        r.scores
            .iter()
            .find(|&&(n, _)| n == id)
            .map(|&(_, s)| s)
            .unwrap()
    }

    #[test]
    fn empty_graph_has_no_scores() {
        let g = AdjacencyList::from_parts(vec![], Vec::<(u64, u64, f32)>::new());
        assert!(betweenness(&g).scores.is_empty());
    }

    #[test]
    fn directed_path_puts_all_weight_on_the_middle() {
        // 1 -> 2 -> 3: node 2 is the sole intermediary of the pair (1,3).
        let g = AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 1.0), (2, 3, 1.0)]);
        let r = betweenness(&g);
        assert_eq!(score_of(&r, 2), 1.0);
        assert_eq!(score_of(&r, 1), 0.0);
        assert_eq!(score_of(&r, 3), 0.0);
    }

    #[test]
    fn directed_triangle_is_symmetric() {
        // 1 -> 2 -> 3 -> 1: each node is the unique intermediary of exactly one
        // ordered pair, so every score is 1.0.
        let g =
            AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 1.0), (2, 3, 1.0), (3, 1, 1.0)]);
        let r = betweenness(&g);
        for id in [1, 2, 3] {
            assert_eq!(score_of(&r, id), 1.0);
        }
    }

    #[test]
    fn diamond_splits_dependency_across_two_shortest_paths() {
        // 1 -> {2,3} -> 4: the pair (1,4) has two equally short paths, so nodes
        // 2 and 3 each carry half the dependency.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![(1, 2, 1.0), (1, 3, 1.0), (2, 4, 1.0), (3, 4, 1.0)],
        );
        let r = betweenness(&g);
        assert!((score_of(&r, 2) - 0.5).abs() < 1e-12);
        assert!((score_of(&r, 3) - 0.5).abs() < 1e-12);
        assert_eq!(score_of(&r, 1), 0.0);
        assert_eq!(score_of(&r, 4), 0.0);
    }

    #[test]
    fn most_central_is_reported_first() {
        // 1 -> 2 -> 3 -> 4: nodes 2 and 3 are brokers; 2 lies on (1,3) and (1,4),
        // 3 lies on (1,4),(2,4) — both score 2, so the head of the list is one
        // of them, ties broken by ascending id → node 2 first.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![(1, 2, 1.0), (2, 3, 1.0), (3, 4, 1.0)],
        );
        let r = betweenness(&g);
        assert_eq!(r.scores[0].0, 2);
        assert_eq!(score_of(&r, 2), 2.0);
        assert_eq!(score_of(&r, 3), 2.0);
        assert_eq!(score_of(&r, 1), 0.0);
    }

    #[test]
    fn self_loops_and_parallel_edges_are_ignored() {
        // 1 -> 2 (twice) -> 3, plus a self-loop on 2: still a simple path, so
        // node 2 scores exactly 1.0.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3],
            vec![(1, 2, 1.0), (1, 2, 1.0), (2, 2, 1.0), (2, 3, 1.0)],
        );
        let r = betweenness(&g);
        assert_eq!(score_of(&r, 2), 1.0);
    }
}
