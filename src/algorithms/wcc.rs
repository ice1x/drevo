//! Weakly connected components (WCC) — RFC #307 Phase 8.
//!
//! Two nodes are in the same weakly connected component when a path joins them
//! once every edge is treated as **undirected**. This is the third global
//! analytic (after [`crate::algorithms::pagerank`] and
//! [`crate::algorithms::louvain`]) and the cheapest: a single
//! near-linear union-find pass over the undirected projection of an
//! [`AdjacencyList`]. It is the standard tool
//! for "how many disconnected clusters does this knowledge graph have, and which
//! node sits in which one".
//!
//! Component IDs are assigned by the **minimum node ID** in each component, in
//! ascending order, so the labelling is fully independent of the order the
//! engine happened to enumerate nodes in — the native snapshot and the KV
//! backend produce byte-identical results over the same graph.
//!
//! Dependency-free, infallible, always compiled, WASM-safe.

use super::AdjacencyList;

/// The result of a [`wcc`] run.
#[derive(Debug, Clone, PartialEq)]
pub struct WccResult {
    /// `(node_id, component_id)` pairs, sorted by ascending node ID. Component
    /// IDs are contiguous integers `0..component_count`, assigned by ascending
    /// minimum node ID per component (order-independent).
    pub components: Vec<(u64, usize)>,
    /// Number of distinct weakly connected components. An empty graph has `0`;
    /// an edgeless graph of `n` nodes has `n` singleton components.
    pub component_count: usize,
}

/// A minimal union-find (disjoint-set) with path halving and union by size.
struct UnionFind {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            size: vec![1; n],
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]]; // path halving
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let (mut ra, mut rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        if self.size[ra] < self.size[rb] {
            std::mem::swap(&mut ra, &mut rb);
        }
        self.parent[rb] = ra;
        self.size[ra] += self.size[rb];
    }
}

/// Compute the weakly connected components of `graph`.
///
/// Always terminates in near-linear time. Edge direction and weight are
/// irrelevant to connectivity: a directed edge `i -> j` and a directed edge
/// `j -> i` both merge `i` and `j`. Self-loops are ignored. An empty graph
/// yields an empty result; a graph with no edges yields one singleton
/// component per node.
pub fn wcc(graph: &AdjacencyList) -> WccResult {
    let n = graph.node_count();
    if n == 0 {
        return WccResult {
            components: Vec::new(),
            component_count: 0,
        };
    }

    let mut uf = UnionFind::new(n);
    for i in 0..n {
        for &(j, _w) in graph.out_edges(i) {
            uf.union(i, j);
        }
    }

    // Assign each component a stable ID by the smallest node ID it contains, so
    // the labelling does not depend on the engine's node-enumeration order.
    let mut root_min: std::collections::HashMap<usize, u64> = std::collections::HashMap::new();
    for i in 0..n {
        let root = uf.find(i);
        let id = graph.id_at(i);
        root_min
            .entry(root)
            .and_modify(|m| {
                if id < *m {
                    *m = id;
                }
            })
            .or_insert(id);
    }

    // Order roots by their minimum node ID and number them 0..k.
    let mut roots: Vec<(usize, u64)> = root_min.into_iter().collect();
    roots.sort_unstable_by_key(|&(_, min_id)| min_id);
    let mut label: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::with_capacity(roots.len());
    for (rank, &(root, _)) in roots.iter().enumerate() {
        label.insert(root, rank);
    }
    let component_count = roots.len();

    let mut components: Vec<(u64, usize)> = (0..n)
        .map(|i| {
            let root = uf.find(i);
            (graph.id_at(i), label[&root])
        })
        .collect();
    components.sort_unstable_by_key(|&(id, _)| id);

    WccResult {
        components,
        component_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comp_of(r: &WccResult, id: u64) -> usize {
        r.components
            .iter()
            .find(|&&(n, _)| n == id)
            .map(|&(_, c)| c)
            .unwrap()
    }

    #[test]
    fn empty_graph_has_no_components() {
        let g = AdjacencyList::from_parts(vec![], Vec::<(u64, u64, f32)>::new());
        let r = wcc(&g);
        assert_eq!(r.component_count, 0);
        assert!(r.components.is_empty());
    }

    #[test]
    fn edgeless_graph_is_all_singletons() {
        let g = AdjacencyList::from_parts(vec![5, 2, 9], Vec::<(u64, u64, f32)>::new());
        let r = wcc(&g);
        assert_eq!(r.component_count, 3);
        // Distinct component per node.
        assert_ne!(comp_of(&r, 5), comp_of(&r, 2));
        assert_ne!(comp_of(&r, 2), comp_of(&r, 9));
    }

    #[test]
    fn direction_is_ignored_a_single_directed_edge_joins_both() {
        // 1 -> 2 only; weakly connected still merges them.
        let g = AdjacencyList::from_parts(vec![1, 2], vec![(1, 2, 1.0)]);
        let r = wcc(&g);
        assert_eq!(r.component_count, 1);
        assert_eq!(comp_of(&r, 1), comp_of(&r, 2));
    }

    #[test]
    fn two_disjoint_clusters_are_separate() {
        // {1,2,3} chain and {10,11} edge, no bridge.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 10, 11],
            vec![(1, 2, 1.0), (2, 3, 1.0), (10, 11, 1.0)],
        );
        let r = wcc(&g);
        assert_eq!(r.component_count, 2);
        assert_eq!(comp_of(&r, 1), comp_of(&r, 2));
        assert_eq!(comp_of(&r, 2), comp_of(&r, 3));
        assert_eq!(comp_of(&r, 10), comp_of(&r, 11));
        assert_ne!(comp_of(&r, 1), comp_of(&r, 10));
    }

    #[test]
    fn component_ids_are_stable_by_min_node_id() {
        // Enumeration order shuffled; the {2,3} cluster must still get id 0
        // (min node id 2 < 5), the {5} singleton id 1.
        let g = AdjacencyList::from_parts(vec![5, 3, 2], vec![(3, 2, 1.0)]);
        let r = wcc(&g);
        assert_eq!(comp_of(&r, 2), 0);
        assert_eq!(comp_of(&r, 3), 0);
        assert_eq!(comp_of(&r, 5), 1);
        // Output is sorted by node id.
        assert_eq!(r.components, vec![(2, 0), (3, 0), (5, 1)]);
    }

    #[test]
    fn self_loops_do_not_create_spurious_links() {
        let g = AdjacencyList::from_parts(vec![1, 2], vec![(1, 1, 1.0)]);
        let r = wcc(&g);
        assert_eq!(r.component_count, 2);
        assert_ne!(comp_of(&r, 1), comp_of(&r, 2));
    }
}
