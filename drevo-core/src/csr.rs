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
}
