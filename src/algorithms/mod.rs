//! Built-in global graph algorithms — Phase 15 task `00098`.
//!
//! drevo already ships the *local* traversal algorithms (BFS / DFS / weighted
//! Dijkstra shortest path / subgraph extraction) in [`crate::traversal`]. This
//! module adds the two *global* analytics algorithms a graph database is
//! expected to provide:
//!
//! * [`pagerank`](crate::algorithms::pagerank) — Google PageRank centrality via
//!   weighted power iteration with dangling-node redistribution and a
//!   configurable damping factor and convergence tolerance.
//! * [`louvain`](crate::algorithms::louvain) — Louvain community detection:
//!   greedy multi-level modularity optimisation that partitions the graph into
//!   communities.
//!
//! Both operate over an [`AdjacencyList`](crate::algorithms::AdjacencyList) — an
//! in-memory snapshot of the whole graph. Unlike the per-node closures the local
//! traversals use, a global algorithm needs the entire node set and adjacency at
//! once, so the caller materialises the snapshot first (see
//! [`crate::db::Drevo::pagerank`] / [`crate::db::Drevo::louvain_communities`],
//! which build it from the storage backend).
//!
//! The pure algorithm functions are **infallible** once given a valid config:
//! all validation happens up front when a
//! [`PageRankConfig`](crate::algorithms::PageRankConfig) /
//! [`LouvainConfig`](crate::algorithms::LouvainConfig) is constructed, surfacing
//! through this module's own
//! [`AlgorithmError`](crate::algorithms::AlgorithmError) channel rather than
//! widening the crate-wide `DrevoError`. Like the planner
//! (`00085`) and replication (`00095`) substrates, the algorithms have no
//! reason to add a variant to the core error type's exhaustive match sites.
//!
//! ## Weight precondition
//!
//! Both algorithms interpret [`crate::model::Edge::weight`] as a non-negative
//! connection strength (the same precondition Dijkstra documents). The model
//! layer guarantees finiteness (NaN / ±∞ are rejected at write time). Negative
//! finite weights are admitted by storage but violate the assumptions of both
//! algorithms; they are clamped to `0.0` here so a single bad edge cannot
//! produce a `NaN` rank or a negative-probability transition.
//!
//! Dependency-free (only `thiserror` for the error type, already in-tree),
//! always compiled, and WASM-safe.

mod betweenness;
mod closeness;
mod louvain;
mod pagerank;
mod scc;
mod triangles;
mod wcc;

pub use betweenness::{betweenness, BetweennessResult};
pub use closeness::{closeness, ClosenessResult};
pub use louvain::{louvain, LouvainConfig, LouvainResult};
pub use pagerank::{pagerank, pagerank_parallel, PageRankConfig, PageRankResult};
pub use scc::{scc, SccResult};
pub use triangles::{triangles, TriangleResult};
pub use wcc::{wcc, WccResult};

use std::collections::HashMap;

/// Build an [`AdjacencyList`] from a single consistent MVCC snapshot of the
/// native engine (RFC #307 Phase 8). Freezing once with
/// [`NativeGraph::snapshot`](crate::native::NativeGraph::snapshot) means a
/// concurrent writer can never perturb an algorithm mid-run.
fn native_adjacency(engine: &crate::native::NativeGraph) -> AdjacencyList {
    let snap = engine.snapshot();
    let node_ids: Vec<u64> = snap.all_nodes().into_iter().map(|n| n.id).collect();
    let edges = snap
        .all_edges()
        .into_iter()
        .map(|e| (e.from_id, e.to_id, e.weight));
    AdjacencyList::from_parts(node_ids, edges)
}

/// PageRank over the **native engine**, over a consistent MVCC snapshot.
///
/// Uses the serial [`pagerank`]. The CSR layout landed (#382:
/// [`AdjacencyList`] is now Compressed-Sparse-Row internally), and
/// `benches/pagerank_bench.rs` was re-measured over it: the naive rayon
/// [`pagerank_parallel`] is still **~4–17× SLOWER** than serial (~17× at 10k
/// nodes, ~8× at 100k, ~4.4× even at 500k / 3M edges). PageRank is
/// memory-bandwidth-bound, so the per-iteration fork/join and the pull-based
/// reverse index cost more than the cores save — the CSR layout did not change
/// that verdict. Serial-over-CSR is therefore the default; a real parallel win
/// would need a fundamentally different scheme (NUMA-aware block partitioning),
/// not just more threads. See `docs/native-load.md`.
pub fn pagerank_native(
    engine: &crate::native::NativeGraph,
    config: &PageRankConfig,
) -> PageRankResult {
    pagerank(&native_adjacency(engine), config)
}

/// Louvain community detection over the **native engine**, over a consistent
/// MVCC snapshot (RFC #307 Phase 8). Serial — Louvain's local-moving phase is
/// inherently sequential.
pub fn louvain_native(
    engine: &crate::native::NativeGraph,
    config: &LouvainConfig,
) -> LouvainResult {
    louvain(&native_adjacency(engine), config)
}

/// Weakly connected components over the **native engine**, over a consistent
/// MVCC snapshot (RFC #307 Phase 8). A single near-linear union-find pass —
/// no config, always serial (the work is dominated by the snapshot build, not
/// the union-find).
pub fn wcc_native(engine: &crate::native::NativeGraph) -> WccResult {
    wcc(&native_adjacency(engine))
}

/// Strongly connected components over the **native engine**, over a consistent
/// MVCC snapshot (RFC #307 Phase 8). Iterative Tarjan — linear time, serial,
/// no native recursion.
pub fn scc_native(engine: &crate::native::NativeGraph) -> SccResult {
    scc(&native_adjacency(engine))
}

/// Triangle counts and local clustering coefficients over the **native
/// engine**, over a consistent MVCC snapshot (RFC #307 Phase 8). Serial.
pub fn triangles_native(engine: &crate::native::NativeGraph) -> TriangleResult {
    triangles(&native_adjacency(engine))
}

/// Betweenness centrality over the **native engine**, over a consistent MVCC
/// snapshot (RFC #307 Phase 8). Brandes' algorithm, serial.
pub fn betweenness_native(engine: &crate::native::NativeGraph) -> BetweennessResult {
    betweenness(&native_adjacency(engine))
}

/// Harmonic closeness centrality over the **native engine**, over a consistent
/// MVCC snapshot (RFC #307 Phase 8). One BFS per node, serial.
pub fn closeness_native(engine: &crate::native::NativeGraph) -> ClosenessResult {
    closeness(&native_adjacency(engine))
}

/// A failure raised while configuring a graph algorithm.
///
/// Algorithm *execution* is infallible; only invalid configuration (a damping
/// factor outside `(0, 1)`, a non-positive iteration cap, a negative tolerance
/// or resolution) is rejected — at construction time, before any work starts.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AlgorithmError {
    /// The PageRank damping factor was outside the open interval `(0.0, 1.0)`.
    #[error("damping factor must be in the open interval (0, 1), got {0}")]
    InvalidDamping(f64),

    /// A maximum-iterations / maximum-passes cap was zero.
    #[error("iteration cap must be at least 1, got {0}")]
    InvalidIterations(usize),

    /// A convergence tolerance was negative or not finite.
    #[error("tolerance must be a finite, non-negative number, got {0}")]
    InvalidTolerance(f64),

    /// The Louvain resolution parameter was negative or not finite.
    #[error("resolution must be a finite, non-negative number, got {0}")]
    InvalidResolution(f64),
}

/// An in-memory, directed, weighted adjacency snapshot over a set of node IDs.
///
/// This is the input both global algorithms consume. It is built once from the
/// full node + edge set ([`AdjacencyList::from_parts`]); nodes are stored in a
/// stable, caller-supplied order so results are deterministic across runs over
/// the same data.
///
/// Edges referencing a node ID that is not in the node set are silently
/// dropped (a defensive guard — the storage layer never produces dangling
/// adjacency, but the snapshot must not panic if it ever does). Negative edge
/// weights are clamped to `0.0`.
#[derive(Debug, Clone)]
pub struct AdjacencyList {
    /// Node IDs in stable order. Result vectors are keyed by these IDs.
    nodes: Vec<u64>,
    /// Directed out-adjacency in **Compressed-Sparse-Row** layout (RFC #307
    /// Phase 8, #382): node `i`'s out-edges are the contiguous slice
    /// `edges[offsets[i]..offsets[i + 1]]`, each `(dst_index, weight)`. Parallel
    /// edges are kept as separate entries (their weights add up naturally
    /// everywhere they are summed). Flattening the old `Vec<Vec<…>>` into two
    /// arrays gives the cache-friendly sweep every global algorithm here wants,
    /// with no change to their `out_edges`/`undirected` view.
    offsets: Vec<usize>,
    /// The flat `(dst_index, weight)` edge array; sliced per node by
    /// [`offsets`](Self::offsets).
    edges: Vec<(usize, f64)>,
}

impl AdjacencyList {
    /// Build a snapshot from a node-ID list and a directed, weighted edge
    /// iterator yielding `(from_id, to_id, weight)` tuples.
    ///
    /// `node_ids` defines both membership and iteration order. Duplicate IDs
    /// in `node_ids` are ignored after the first. Edges whose endpoints are
    /// not both present are dropped; negative weights are clamped to `0.0`.
    pub fn from_parts<I>(node_ids: Vec<u64>, edges: I) -> Self
    where
        I: IntoIterator<Item = (u64, u64, f32)>,
    {
        let mut nodes = Vec::with_capacity(node_ids.len());
        let mut index = HashMap::with_capacity(node_ids.len());
        for id in node_ids {
            if let std::collections::hash_map::Entry::Vacant(e) = index.entry(id) {
                e.insert(nodes.len());
                nodes.push(id);
            }
        }

        // Bucket by source (preserving edge order within a node), then flatten
        // into CSR: offsets[i]..offsets[i+1] delimits node i's edge slice. The
        // per-node order is identical to the old `Vec<Vec<…>>`, so every
        // algorithm's output is unchanged.
        let mut buckets: Vec<Vec<(usize, f64)>> = vec![Vec::new(); nodes.len()];
        for (from, to, weight) in edges {
            let (Some(&i), Some(&j)) = (index.get(&from), index.get(&to)) else {
                continue;
            };
            let w = (weight as f64).max(0.0);
            buckets[i].push((j, w));
        }

        let mut offsets = Vec::with_capacity(nodes.len() + 1);
        let mut flat = Vec::new();
        offsets.push(0);
        for bucket in &buckets {
            flat.extend_from_slice(bucket);
            offsets.push(flat.len());
        }

        Self {
            nodes,
            offsets,
            edges: flat,
        }
    }

    /// The node IDs, in the stable order results are keyed by.
    pub fn nodes(&self) -> &[u64] {
        &self.nodes
    }

    /// Number of nodes in the snapshot.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Total number of directed edges retained in the snapshot.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// `true` when there are no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The node ID at a dense index.
    pub(crate) fn id_at(&self, idx: usize) -> u64 {
        self.nodes[idx]
    }

    /// Directed out-edges of node at dense index `i` as `(dst_index, weight)` —
    /// a slice into the flat CSR edge array.
    pub(crate) fn out_edges(&self, i: usize) -> &[(usize, f64)] {
        &self.edges[self.offsets[i]..self.offsets[i + 1]]
    }

    /// Build the **undirected** weighted projection used by community
    /// detection. A directed edge `i -> j` (with `i != j`) contributes its
    /// weight to the undirected pair `{i, j}` in both directions; reciprocal
    /// edges therefore sum. Self-loops `i -> i` are returned separately in
    /// `loops[i]` (a self-loop of weight `s` contributes `2s` to the weighted
    /// degree of `i`, the standard modularity convention).
    ///
    /// Returns `(adj, loops)` where `adj[i]` is the symmetric neighbour list
    /// `(j, weight)` for `j != i`, and `loops[i]` is the accumulated self-loop
    /// weight at `i`.
    pub(crate) fn undirected(&self) -> (Vec<Vec<(usize, f64)>>, Vec<f64>) {
        let n = self.nodes.len();
        let mut maps: Vec<HashMap<usize, f64>> = vec![HashMap::new(); n];
        let mut loops = vec![0.0; n];
        for i in 0..n {
            for &(j, w) in self.out_edges(i) {
                if i == j {
                    loops[i] += w;
                } else {
                    *maps[i].entry(j).or_insert(0.0) += w;
                    *maps[j].entry(i).or_insert(0.0) += w;
                }
            }
        }
        let adj = maps
            .into_iter()
            .map(|m| {
                let mut v: Vec<(usize, f64)> = m.into_iter().collect();
                v.sort_by_key(|&(j, _)| j);
                v
            })
            .collect();
        (adj, loops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dense index of a node ID — test helper (the algorithms work purely on
    /// dense indices, so the production type exposes no public lookup).
    fn pos(g: &AdjacencyList, id: u64) -> usize {
        g.nodes().iter().position(|&n| n == id).unwrap()
    }

    #[test]
    fn empty_snapshot_is_empty() {
        let g = AdjacencyList::from_parts(vec![], Vec::<(u64, u64, f32)>::new());
        assert!(g.is_empty());
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
        assert_eq!(g.nodes(), &[] as &[u64]);
    }

    #[test]
    fn dedupes_node_ids_preserving_first_order() {
        let g = AdjacencyList::from_parts(vec![7, 3, 7, 9, 3], Vec::<(u64, u64, f32)>::new());
        assert_eq!(g.nodes(), &[7, 3, 9]);
        assert_eq!(g.node_count(), 3);
    }

    #[test]
    fn drops_edges_with_unknown_endpoints() {
        let g =
            AdjacencyList::from_parts(vec![1, 2], vec![(1, 2, 1.0), (1, 99, 1.0), (42, 2, 1.0)]);
        assert_eq!(g.edge_count(), 1);
    }

    #[test]
    fn clamps_negative_weights_to_zero() {
        let g = AdjacencyList::from_parts(vec![1, 2], vec![(1, 2, -5.0)]);
        let i = pos(&g, 1);
        assert_eq!(g.out_edges(i), &[(pos(&g, 2), 0.0)]);
    }

    #[test]
    fn undirected_projection_sums_reciprocal_edges() {
        // 1 -> 2 (w=1) and 2 -> 1 (w=3) collapse to an undirected edge of w=4.
        let g = AdjacencyList::from_parts(vec![1, 2], vec![(1, 2, 1.0), (2, 1, 3.0)]);
        let (adj, loops) = g.undirected();
        let i1 = pos(&g, 1);
        let i2 = pos(&g, 2);
        assert_eq!(adj[i1], vec![(i2, 4.0)]);
        assert_eq!(adj[i2], vec![(i1, 4.0)]);
        assert_eq!(loops, vec![0.0, 0.0]);
    }

    #[test]
    fn undirected_projection_separates_self_loops() {
        let g = AdjacencyList::from_parts(vec![1, 2], vec![(1, 1, 2.5), (1, 2, 1.0)]);
        let (adj, loops) = g.undirected();
        let i1 = pos(&g, 1);
        let i2 = pos(&g, 2);
        assert_eq!(loops[i1], 2.5);
        assert_eq!(loops[i2], 0.0);
        assert_eq!(adj[i1], vec![(i2, 1.0)]);
    }
}
