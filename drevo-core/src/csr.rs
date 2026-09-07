//! Compressed-Sparse-Row adjacency snapshot for cache-friendly, lock-free
//! parallel scans and graph algorithms (issue #382, Phase 8).
//!
//! The live engine stores adjacency as a `HashMap<u64, Vec<AdjEntry>>` keyed by
//! sparse node id — great for point lookups and incremental writes, poor for a
//! full-graph sweep (pointer-chasing, no locality). A
//! [`CsrAdjacency`](crate::csr::CsrAdjacency) flattens one
//! [`GraphSnapshot`](crate::native::GraphSnapshot) into three contiguous arrays:
//!
//! * `vertices` — the snapshot's node ids, sorted ascending; a node's **dense
//!   index** `0..V` is its position here (local ids are sparse after deletes).
//! * `offsets` — `offsets[i]..offsets[i+1]` bounds vertex `i`'s neighbour slice.
//! * `neighbors` — every out-neighbour as a **dense index**, laid out per vertex.
//!
//! Iterating a vertex's neighbours is then a walk over one contiguous `&[u32]`,
//! and a whole-graph algorithm (PageRank, connected components) indexes a flat
//! `rank[0..V]` vector instead of a hash map. Because it is built off an
//! immutable [MVCC snapshot](crate::native::NativeGraph::snapshot), workers can
//! fan out over it with no locking — later slices of #382.
//!
//! # This slice
//!
//! Out-adjacency, **distinct** neighbours per vertex (matching
//! [`neighbor_ids`](crate::native::GraphSnapshot::neighbor_ids) with no kind
//! filter): multi-edges between the same pair collapse to one entry, as PageRank
//! and reachability want. In-adjacency and multi-edge/weighted variants are
//! follow-ups.

/// A Compressed-Sparse-Row view of a graph's out-adjacency: three contiguous
/// arrays giving each vertex a dense index and a flat neighbour slice. Built by
/// [`GraphSnapshot::csr_out`](crate::native::GraphSnapshot::csr_out); immutable
/// once built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsrAdjacency {
    /// Node ids in ascending order; the dense index of a vertex is its position.
    vertices: Vec<u64>,
    /// Per-vertex slice bounds into [`neighbors`](Self::neighbors_flat): vertex
    /// `i` owns `neighbors[offsets[i]..offsets[i+1]]`. Length `vertices.len()+1`.
    offsets: Vec<u32>,
    /// Out-neighbours as dense indices, laid out per vertex and sorted within
    /// each vertex's slice for determinism.
    neighbors: Vec<u32>,
}

impl CsrAdjacency {
    /// Build from a sorted, de-duplicated `vertices` id list and a function
    /// giving each vertex's **distinct** out-neighbour node ids. Neighbour ids
    /// not present in `vertices` (dangling) are skipped; each vertex's neighbour
    /// slice is stored as sorted dense indices.
    ///
    /// `vertices` must be sorted ascending (dense-index lookup binary-searches
    /// it); the caller — [`GraphSnapshot::csr_out`](crate::native::GraphSnapshot::csr_out)
    /// — guarantees that.
    #[must_use]
    pub fn from_out_neighbors(vertices: Vec<u64>, out_neighbors: impl Fn(u64) -> Vec<u64>) -> Self {
        let n = vertices.len();
        let index_of = |id: u64| vertices.binary_search(&id).ok().map(|i| i as u32);
        let mut offsets = Vec::with_capacity(n + 1);
        let mut neighbors = Vec::new();
        offsets.push(0);
        for &id in &vertices {
            let mut row: Vec<u32> = out_neighbors(id).into_iter().filter_map(index_of).collect();
            row.sort_unstable();
            neighbors.extend_from_slice(&row);
            offsets.push(neighbors.len() as u32);
        }
        Self {
            vertices,
            offsets,
            neighbors,
        }
    }

    /// Number of vertices (dense indices `0..vertex_count`).
    #[must_use]
    pub fn vertex_count(&self) -> usize {
        self.vertices.len()
    }

    /// Total number of out-neighbour entries across all vertices.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.neighbors.len()
    }

    /// Whether the graph has no vertices.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty()
    }

    /// The node id at dense `index`, or `None` if out of range.
    #[must_use]
    pub fn node_id(&self, index: usize) -> Option<u64> {
        self.vertices.get(index).copied()
    }

    /// The dense index of `node_id`, or `None` if absent from the snapshot.
    #[must_use]
    pub fn index_of(&self, node_id: u64) -> Option<usize> {
        self.vertices.binary_search(&node_id).ok()
    }

    /// The out-degree (distinct out-neighbours) of the vertex at `index`.
    #[must_use]
    pub fn out_degree(&self, index: usize) -> usize {
        self.neighbors_of(index).len()
    }

    /// The out-neighbours of the vertex at `index` as dense indices — a slice
    /// into the flat neighbour array. Empty for an out-of-range index.
    #[must_use]
    pub fn neighbors_of(&self, index: usize) -> &[u32] {
        match (self.offsets.get(index), self.offsets.get(index + 1)) {
            (Some(&start), Some(&end)) => &self.neighbors[start as usize..end as usize],
            _ => &[],
        }
    }

    /// The whole flat neighbour array (dense indices), for algorithms that stream
    /// every edge once.
    #[must_use]
    pub fn neighbors_flat(&self) -> &[u32] {
        &self.neighbors
    }

    /// PageRank over this out-adjacency (issue #382), returned by dense index —
    /// `rank[i]` is the score of the vertex at index `i`, and the vector sums to
    /// 1.0 (a probability distribution).
    ///
    /// Push-style power iteration: each vertex pushes `damping * rank / out_degree`
    /// along its out-edges, plus a uniform teleport `(1 - damping) / N`.
    /// Dangling vertices (no out-edges) redistribute their mass uniformly, so no
    /// rank leaks and the total stays 1.0 every iteration. `damping` is the usual
    /// 0.85; `iterations` fixed power-iteration steps (≈20 converges on typical
    /// graphs). An empty graph returns an empty vector.
    #[must_use]
    pub fn pagerank(&self, damping: f64, iterations: usize) -> Vec<f64> {
        let n = self.vertex_count();
        if n == 0 {
            return Vec::new();
        }
        let inv_n = 1.0 / n as f64;
        let teleport = (1.0 - damping) * inv_n;
        let mut rank = vec![inv_n; n];
        for _ in 0..iterations {
            let mut next = vec![teleport; n];
            let mut dangling = 0.0;
            for (i, &ri) in rank.iter().enumerate() {
                let out = self.neighbors_of(i);
                if out.is_empty() {
                    dangling += ri;
                    continue;
                }
                let share = damping * ri / out.len() as f64;
                for &k in out {
                    next[k as usize] += share;
                }
            }
            // A dangling vertex's mass would otherwise vanish; spread it evenly.
            let dangling_share = damping * dangling * inv_n;
            for r in &mut next {
                *r += dangling_share;
            }
            rank = next;
        }
        rank
    }

    /// Weakly-connected components (issue #382): the connectivity-based grouping
    /// of the graph, treating every edge as **undirected**. Returns a component
    /// label per dense vertex index; two vertices share a label iff one reaches
    /// the other ignoring edge direction. Labels are `0..component_count` in
    /// ascending order of each component's smallest dense index (deterministic).
    ///
    /// This is the simplest cluster/community primitive — union-find over the
    /// out-edges (every edge is some vertex's out-edge, so out-adjacency alone
    /// covers undirected connectivity). Modularity-based community detection is a
    /// later, heavier algorithm.
    #[must_use]
    pub fn weakly_connected_components(&self) -> Vec<u32> {
        let n = self.vertex_count();
        let mut parent: Vec<u32> = (0..n as u32).collect();
        for (i, _) in self.vertices.iter().enumerate() {
            for &k in self.neighbors_of(i) {
                uf_union(&mut parent, i as u32, k);
            }
        }
        // Relabel roots to dense component ids in first-seen (ascending) order.
        let mut label = vec![u32::MAX; n];
        let mut next_label = 0u32;
        let mut out = vec![0u32; n];
        for (i, slot) in out.iter_mut().enumerate() {
            let root = uf_find(&mut parent, i as u32) as usize;
            if label[root] == u32::MAX {
                label[root] = next_label;
                next_label += 1;
            }
            *slot = label[root];
        }
        out
    }

    /// The number of weakly-connected components (`0` for an empty graph).
    #[must_use]
    pub fn component_count(&self) -> usize {
        self.weakly_connected_components()
            .iter()
            .copied()
            .max()
            .map_or(0, |m| m as usize + 1)
    }

    /// Morsel-driven parallel scan (issue #382, Phase 8): evaluate `f(index)` for
    /// every dense vertex index across up to `threads` worker threads, returning
    /// the results in vertex order (`out[i] == f(i)`).
    ///
    /// The vertices are split into contiguous morsels, one per worker; each
    /// worker writes only its own disjoint output slice, so there is **no
    /// locking and no contention**. Because a [`CsrAdjacency`] is immutable and
    /// built off a frozen [MVCC snapshot](crate::native::NativeGraph::snapshot),
    /// `f` can read the whole structure freely — this is the parallel-execution
    /// substrate for whole-graph algorithms. `threads` is clamped to
    /// `1..=vertex_count`; an empty graph returns an empty vector. Sequential and
    /// parallel runs produce identical results.
    #[must_use]
    pub fn par_map<T, F>(&self, threads: usize, f: F) -> Vec<T>
    where
        T: Send + Default + Clone,
        F: Fn(usize) -> T + Sync,
    {
        let n = self.vertex_count();
        if n == 0 {
            return Vec::new();
        }
        let threads = threads.clamp(1, n);
        let chunk = n.div_ceil(threads);
        let mut out: Vec<T> = vec![T::default(); n];
        let f = &f;
        std::thread::scope(|s| {
            let mut base = 0usize;
            for slice in out.chunks_mut(chunk) {
                let start = base;
                base += slice.len();
                s.spawn(move || {
                    for (off, slot) in slice.iter_mut().enumerate() {
                        *slot = f(start + off);
                    }
                });
            }
        });
        out
    }
}

/// Union-find `find` with path compression: returns the root of `x` and flattens
/// the path to it. Roots are the smallest dense index in a component
/// ([`uf_union`] attaches the larger root under the smaller).
fn uf_find(parent: &mut [u32], x: u32) -> u32 {
    let mut root = x;
    while parent[root as usize] != root {
        root = parent[root as usize];
    }
    // Path-compress: point every node on the walk straight at the root.
    let mut cur = x;
    while parent[cur as usize] != root {
        let nextp = parent[cur as usize];
        parent[cur as usize] = root;
        cur = nextp;
    }
    root
}

/// Union-find `union` by smaller root index, so a component's root is its minimum
/// dense index (deterministic).
fn uf_union(parent: &mut [u32], a: u32, b: u32) {
    let ra = uf_find(parent, a);
    let rb = uf_find(parent, b);
    if ra != rb {
        let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
        parent[hi as usize] = lo;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny graph over node ids {10, 20, 30, 40} (sparse, to exercise the
    // id → dense-index mapping): 10→20, 10→30, 20→30, 30→10, 40 isolated.
    fn sample() -> CsrAdjacency {
        let vertices = vec![10u64, 20, 30, 40];
        CsrAdjacency::from_out_neighbors(vertices, |id| match id {
            10 => vec![30, 20], // unsorted on input; CSR sorts by dense index
            20 => vec![30],
            30 => vec![10],
            _ => vec![],
        })
    }

    #[test]
    fn dense_index_round_trips_with_node_id() {
        let csr = sample();
        assert_eq!(csr.vertex_count(), 4);
        assert_eq!(csr.index_of(10), Some(0));
        assert_eq!(csr.index_of(40), Some(3));
        assert_eq!(csr.index_of(99), None);
        assert_eq!(csr.node_id(0), Some(10));
        assert_eq!(csr.node_id(3), Some(40));
        assert_eq!(csr.node_id(4), None);
    }

    #[test]
    fn neighbours_are_dense_indices_sorted_per_vertex() {
        let csr = sample();
        // 10 → {20 (idx 1), 30 (idx 2)}, sorted ascending regardless of input.
        assert_eq!(csr.neighbors_of(0), &[1, 2]);
        assert_eq!(csr.out_degree(0), 2);
        // 20 → {30 (idx 2)}, 30 → {10 (idx 0)}, 40 → {}.
        assert_eq!(csr.neighbors_of(1), &[2]);
        assert_eq!(csr.neighbors_of(2), &[0]);
        assert_eq!(csr.neighbors_of(3), &[] as &[u32]);
        assert_eq!(csr.out_degree(3), 0);
        assert_eq!(csr.edge_count(), 4);
    }

    #[test]
    fn offsets_partition_the_flat_neighbour_array() {
        let csr = sample();
        // Every vertex's slice is exactly offsets[i]..offsets[i+1]; the union is
        // the whole flat array with no gaps or overlaps.
        let mut rebuilt: Vec<u32> = Vec::new();
        for i in 0..csr.vertex_count() {
            rebuilt.extend_from_slice(csr.neighbors_of(i));
        }
        assert_eq!(rebuilt, csr.neighbors_flat());
    }

    #[test]
    fn dangling_neighbours_are_skipped() {
        // A neighbour id not in the vertex set (e.g. concurrently removed) is
        // dropped rather than producing a bogus dense index.
        let csr = CsrAdjacency::from_out_neighbors(vec![1u64, 2], |id| match id {
            1 => vec![2, 999], // 999 is not a vertex
            _ => vec![],
        });
        assert_eq!(csr.neighbors_of(0), &[1]);
        assert_eq!(csr.edge_count(), 1);
    }

    #[test]
    fn empty_graph_is_well_formed() {
        let csr = CsrAdjacency::from_out_neighbors(Vec::new(), |_| Vec::new());
        assert!(csr.is_empty());
        assert_eq!(csr.vertex_count(), 0);
        assert_eq!(csr.edge_count(), 0);
        assert_eq!(csr.neighbors_of(0), &[] as &[u32]);
    }

    fn ring(n: u64) -> CsrAdjacency {
        // A directed cycle 0→1→…→(n-1)→0 over node ids [0, n).
        let vertices: Vec<u64> = (0..n).collect();
        CsrAdjacency::from_out_neighbors(vertices, move |id| vec![(id + 1) % n])
    }

    fn approx_sum(rank: &[f64]) -> f64 {
        rank.iter().sum()
    }

    #[test]
    fn pagerank_is_a_distribution_summing_to_one() {
        let csr = sample(); // has a dangling vertex (40) — mass must not leak
        let rank = csr.pagerank(0.85, 40);
        assert_eq!(rank.len(), csr.vertex_count());
        assert!(
            (approx_sum(&rank) - 1.0).abs() < 1e-9,
            "sum={}",
            approx_sum(&rank)
        );
        assert!(rank.iter().all(|&r| r > 0.0));
    }

    #[test]
    fn pagerank_of_a_symmetric_ring_is_uniform() {
        let csr = ring(5);
        let rank = csr.pagerank(0.85, 100);
        for r in &rank {
            assert!((r - 0.2).abs() < 1e-9, "every ring vertex is 1/5, got {r}");
        }
    }

    #[test]
    fn pagerank_ranks_a_hub_highest() {
        // Three vertices all point at the hub (id 0); the hub is dangling.
        let csr = CsrAdjacency::from_out_neighbors(vec![0u64, 1, 2, 3], |id| match id {
            0 => vec![],  // hub, no out-edges
            _ => vec![0], // everyone points at the hub
        });
        let rank = csr.pagerank(0.85, 60);
        let hub = rank[csr.index_of(0).unwrap()];
        for other in [1u64, 2, 3] {
            assert!(
                hub > rank[csr.index_of(other).unwrap()],
                "hub {hub} must outrank leaf"
            );
        }
        // The three leaves are symmetric → equal.
        assert!((rank[1] - rank[2]).abs() < 1e-12);
        assert!((rank[2] - rank[3]).abs() < 1e-12);
        assert!((approx_sum(&rank) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn pagerank_is_deterministic() {
        let csr = sample();
        assert_eq!(csr.pagerank(0.85, 30), csr.pagerank(0.85, 30));
    }

    #[test]
    fn pagerank_of_empty_graph_is_empty() {
        let csr = CsrAdjacency::from_out_neighbors(Vec::new(), |_| Vec::new());
        assert!(csr.pagerank(0.85, 10).is_empty());
    }

    #[test]
    fn wcc_of_empty_graph_is_empty() {
        let csr = CsrAdjacency::from_out_neighbors(Vec::new(), |_| Vec::new());
        assert!(csr.weakly_connected_components().is_empty());
        assert_eq!(csr.component_count(), 0);
    }

    #[test]
    fn wcc_of_a_ring_is_one_component() {
        let csr = ring(6);
        let comp = csr.weakly_connected_components();
        assert!(comp.iter().all(|&c| c == 0));
        assert_eq!(csr.component_count(), 1);
    }

    #[test]
    fn wcc_separates_disjoint_subgraphs() {
        // Two disjoint edges: {0→1} and {2→3}. Node ids sparse via 0..4.
        let csr = CsrAdjacency::from_out_neighbors(vec![0u64, 1, 2, 3], |id| match id {
            0 => vec![1],
            2 => vec![3],
            _ => vec![],
        });
        let comp = csr.weakly_connected_components();
        assert_eq!(csr.component_count(), 2);
        assert_eq!(comp[0], comp[1], "0 and 1 together");
        assert_eq!(comp[2], comp[3], "2 and 3 together");
        assert_ne!(comp[0], comp[2], "the two pairs are separate");
        // Labels are dense 0..k in ascending first-seen order.
        assert_eq!(comp[0], 0);
        assert_eq!(comp[2], 1);
    }

    #[test]
    fn wcc_is_direction_agnostic() {
        // a→b and c→b: following direction, a and c never reach each other, but
        // weakly (undirected) all three are one component.
        let csr = CsrAdjacency::from_out_neighbors(vec![0u64, 1, 2], |id| match id {
            0 => vec![1], // a→b
            2 => vec![1], // c→b
            _ => vec![],
        });
        let comp = csr.weakly_connected_components();
        assert_eq!(csr.component_count(), 1);
        assert!(comp.iter().all(|&c| c == 0));
    }

    #[test]
    fn par_map_matches_sequential_across_thread_counts() {
        let csr = sample();
        let seq: Vec<usize> = (0..csr.vertex_count()).map(|i| csr.out_degree(i)).collect();
        // Any thread count (including 1 and more than there are vertices) yields
        // the exact sequential result, in vertex order.
        for t in [1usize, 2, 3, 8, 100] {
            let got = csr.par_map(t, |i| csr.out_degree(i));
            assert_eq!(got, seq, "threads={t}");
        }
    }

    #[test]
    fn par_map_preserves_index_alignment() {
        let csr = sample();
        // out[i] must be f(i): map each dense index back to its node id.
        let got = csr.par_map(4, |i| csr.node_id(i).unwrap());
        let want: Vec<u64> = (0..csr.vertex_count())
            .map(|i| csr.node_id(i).unwrap())
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn par_map_of_empty_graph_is_empty() {
        let csr = CsrAdjacency::from_out_neighbors(Vec::new(), |_| Vec::new());
        let got: Vec<usize> = csr.par_map(4, |i| i);
        assert!(got.is_empty());
    }

    #[test]
    fn wcc_counts_isolated_vertices() {
        // One edge 10→20 plus two isolated nodes 30, 40 → 3 components.
        let csr = CsrAdjacency::from_out_neighbors(vec![10u64, 20, 30, 40], |id| match id {
            10 => vec![20],
            _ => vec![],
        });
        assert_eq!(csr.component_count(), 3);
        let comp = csr.weakly_connected_components();
        assert_eq!(
            comp[csr.index_of(10).unwrap()],
            comp[csr.index_of(20).unwrap()]
        );
        assert_ne!(
            comp[csr.index_of(30).unwrap()],
            comp[csr.index_of(40).unwrap()]
        );
    }
}
