//! Triangle counting and local clustering coefficient — RFC #307 Phase 8.
//!
//! A *triangle* is three mutually adjacent nodes. Per node it measures how
//! tightly its neighbours are interconnected: the **local clustering
//! coefficient** of a node with degree `d` and `t` triangles through it is
//! `2t / (d(d-1))` — the fraction of the node's neighbour pairs that are
//! themselves adjacent (0 when `d < 2`). This is the standard structural-density
//! metric, distinct from the connectivity ([`wcc`](crate::algorithms::wcc) /
//! [`scc`](crate::algorithms::scc)) and centrality
//! ([`pagerank`](crate::algorithms::pagerank)) analytics: it answers "how
//! cliquey is the neighbourhood around this node".
//!
//! Computed over the **undirected** projection of the graph (a directed cycle
//! `1 -> 2 -> 3 -> 1` is a triangle), so edge direction and weight do not
//! affect the counts. Deterministic and engine-independent: results are keyed
//! by node ID.
//!
//! Dependency-free, infallible, always compiled, WASM-safe.

use std::collections::HashSet;

use super::AdjacencyList;

/// The result of a [`triangles`] run.
#[derive(Debug, Clone, PartialEq)]
pub struct TriangleResult {
    /// `(node_id, triangle_count, local_clustering_coefficient)` per node,
    /// sorted by ascending node ID. The coefficient is in `[0.0, 1.0]` and is
    /// `0.0` for any node of degree `< 2`.
    pub per_node: Vec<(u64, u64, f64)>,
    /// Total number of distinct triangles in the graph (each counted once).
    pub total_triangles: u64,
}

/// Count triangles and compute the local clustering coefficient of every node.
///
/// Always terminates. Works on the undirected projection: reciprocal and
/// parallel directed edges collapse to a single undirected edge (so a triangle
/// is never double-counted), and self-loops are ignored. An empty graph yields
/// an empty result with `total_triangles = 0`.
pub fn triangles(graph: &AdjacencyList) -> TriangleResult {
    let n = graph.node_count();
    if n == 0 {
        return TriangleResult {
            per_node: Vec::new(),
            total_triangles: 0,
        };
    }

    // Undirected, de-duplicated neighbour lists (no self-loops).
    let (adj, _loops) = graph.undirected();
    let neighbours: Vec<Vec<usize>> = adj
        .iter()
        .map(|nb| nb.iter().map(|&(j, _)| j).collect())
        .collect();
    let sets: Vec<HashSet<usize>> = neighbours
        .iter()
        .map(|nb| nb.iter().copied().collect())
        .collect();

    // Triangles through each node: count adjacent neighbour pairs.
    let mut tri = vec![0u64; n];
    for (u, neigh) in neighbours.iter().enumerate() {
        let mut count = 0u64;
        for (a, &v) in neigh.iter().enumerate() {
            for &w in &neigh[a + 1..] {
                if sets[v].contains(&w) {
                    count += 1;
                }
            }
        }
        tri[u] = count;
    }

    let total_triangles = tri.iter().sum::<u64>() / 3;

    let mut per_node: Vec<(u64, u64, f64)> = (0..n)
        .map(|i| {
            let degree = neighbours[i].len() as u64;
            let coefficient = if degree >= 2 {
                2.0 * tri[i] as f64 / (degree * (degree - 1)) as f64
            } else {
                0.0
            };
            (graph.id_at(i), tri[i], coefficient)
        })
        .collect();
    per_node.sort_unstable_by_key(|&(id, _, _)| id);

    TriangleResult {
        per_node,
        total_triangles,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(r: &TriangleResult, id: u64) -> (u64, f64) {
        r.per_node
            .iter()
            .find(|&&(n, _, _)| n == id)
            .map(|&(_, t, c)| (t, c))
            .unwrap()
    }

    #[test]
    fn empty_graph_has_no_triangles() {
        let g = AdjacencyList::from_parts(vec![], Vec::<(u64, u64, f32)>::new());
        let r = triangles(&g);
        assert_eq!(r.total_triangles, 0);
        assert!(r.per_node.is_empty());
    }

    #[test]
    fn a_directed_cycle_is_one_triangle_undirected() {
        // 1 -> 2 -> 3 -> 1: an undirected triangle. Each node sees 1 triangle,
        // coefficient 1.0 (its two neighbours are adjacent).
        let g =
            AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 1.0), (2, 3, 1.0), (3, 1, 1.0)]);
        let r = triangles(&g);
        assert_eq!(r.total_triangles, 1);
        for id in [1, 2, 3] {
            assert_eq!(row(&r, id), (1, 1.0));
        }
    }

    #[test]
    fn a_path_has_no_triangles() {
        // 1 - 2 - 3: the middle node has degree 2 but its neighbours (1,3) are
        // not adjacent, so coefficient 0; the ends have degree 1 → 0.
        let g = AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 1.0), (2, 3, 1.0)]);
        let r = triangles(&g);
        assert_eq!(r.total_triangles, 0);
        assert_eq!(row(&r, 1), (0, 0.0));
        assert_eq!(row(&r, 2), (0, 0.0));
        assert_eq!(row(&r, 3), (0, 0.0));
    }

    #[test]
    fn a_four_clique_has_four_triangles() {
        // K4: every triple is a triangle → 4 triangles, each node in 3 of them,
        // coefficient 1.0 (all 3 neighbours mutually adjacent).
        let edges = vec![
            (1, 2, 1.0),
            (1, 3, 1.0),
            (1, 4, 1.0),
            (2, 3, 1.0),
            (2, 4, 1.0),
            (3, 4, 1.0),
        ];
        let g = AdjacencyList::from_parts(vec![1, 2, 3, 4], edges);
        let r = triangles(&g);
        assert_eq!(r.total_triangles, 4);
        for id in [1, 2, 3, 4] {
            assert_eq!(row(&r, id), (3, 1.0));
        }
    }

    #[test]
    fn coefficient_is_a_half_for_a_degree_two_apex_with_one_triangle() {
        // Square 1-2-3-4 plus one diagonal 1-3. Node 1 has neighbours {2,3,4};
        // the only adjacent pair among them is (2,3)? No: with diagonal 1-3,
        // node 1's neighbours are 2,3,4 and among them 2-3 and 3-4 are edges.
        // Use a cleaner shape: triangle {1,2,3} plus a pendant 3-4.
        // Node 3 has neighbours {1,2,4}: pair (1,2) is an edge, (1,4)/(2,4) are
        // not → 1 triangle of 3 possible pairs → coefficient 2*1/(3*2)=1/3.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![(1, 2, 1.0), (2, 3, 1.0), (3, 1, 1.0), (3, 4, 1.0)],
        );
        let r = triangles(&g);
        assert_eq!(r.total_triangles, 1);
        assert_eq!(row(&r, 3).0, 1);
        assert!((row(&r, 3).1 - 1.0 / 3.0).abs() < 1e-12);
        // The pendant node 4 has degree 1 → coefficient 0.
        assert_eq!(row(&r, 4), (0, 0.0));
    }

    #[test]
    fn parallel_and_reciprocal_edges_do_not_inflate_the_count() {
        // 1<->2, 2<->3, 3<->1 with both directions each: still one triangle.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3],
            vec![
                (1, 2, 1.0),
                (2, 1, 1.0),
                (2, 3, 1.0),
                (3, 2, 1.0),
                (3, 1, 1.0),
                (1, 3, 1.0),
            ],
        );
        let r = triangles(&g);
        assert_eq!(r.total_triangles, 1);
        assert_eq!(row(&r, 1), (1, 1.0));
    }
}
