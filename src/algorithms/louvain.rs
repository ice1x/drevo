//! Louvain community detection — Phase 15 task `00098`.
//!
//! Partitions an undirected, weighted projection of the graph into communities
//! by greedily maximising [modularity], using the multi-level method of Blondel
//! et al. (2008):
//!
//! 1. **Local moving** — start with every node in its own community; repeatedly
//!    move each node into the neighbouring community that yields the largest
//!    positive modularity gain, until no move improves modularity.
//! 2. **Aggregation** — collapse each community into a single super-node
//!    (intra-community edges become a self-loop; inter-community edges are
//!    summed), then repeat step 1 on the smaller graph.
//!
//! The passes stop when a level produces no further movement. The result maps
//! every original node to a contiguously-numbered community plus the final
//! modularity score.
//!
//! The directed graph is projected to undirected first (see
//! [`AdjacencyList::undirected`]): reciprocal edges sum, self-loops are kept.
//! The algorithm is fully deterministic — nodes are always processed in the
//! snapshot's stable order, with no randomisation.
//!
//! [modularity]: https://en.wikipedia.org/wiki/Modularity_(networks)

use super::{AdjacencyList, AlgorithmError};
use std::collections::HashMap;

/// Floating-point slack for "is this move strictly better" comparisons.
const GAIN_EPS: f64 = 1e-12;

/// Configuration for [`louvain`].
///
/// Construct with [`LouvainConfig::new`] (validated) or
/// [`LouvainConfig::default`] (`resolution = 1.0`, `max_levels = 50`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LouvainConfig {
    /// Resolution parameter `γ` of the generalised modularity. `1.0` is
    /// classic modularity; values `> 1` favour more, smaller communities,
    /// values `< 1` favour fewer, larger ones. Must be finite and
    /// non-negative.
    pub resolution: f64,
    /// Safety cap on the number of aggregation levels. The algorithm
    /// naturally converges well before this; the cap only guards against a
    /// pathological non-terminating loop. Must be at least `1`.
    pub max_levels: usize,
}

impl Default for LouvainConfig {
    fn default() -> Self {
        Self {
            resolution: 1.0,
            max_levels: 50,
        }
    }
}

impl LouvainConfig {
    /// Build a validated config.
    ///
    /// # Errors
    ///
    /// - [`AlgorithmError::InvalidResolution`] if `resolution` is negative or
    ///   not finite.
    /// - [`AlgorithmError::InvalidIterations`] if `max_levels` is `0`.
    pub fn new(resolution: f64, max_levels: usize) -> Result<Self, AlgorithmError> {
        if !(resolution.is_finite() && resolution >= 0.0) {
            return Err(AlgorithmError::InvalidResolution(resolution));
        }
        if max_levels == 0 {
            return Err(AlgorithmError::InvalidIterations(max_levels));
        }
        Ok(Self {
            resolution,
            max_levels,
        })
    }
}

/// The result of a [`louvain`] run.
#[derive(Debug, Clone, PartialEq)]
pub struct LouvainResult {
    /// `(node_id, community_id)` pairs, sorted by ascending node ID. Community
    /// IDs are contiguous integers `0..number_of_communities`.
    pub communities: Vec<(u64, usize)>,
    /// The modularity of the final partition, in `[-0.5, 1.0]`. Higher is a
    /// stronger community structure.
    pub modularity: f64,
    /// Number of aggregation levels performed.
    pub levels: usize,
}

impl LouvainResult {
    /// The number of distinct communities found.
    pub fn community_count(&self) -> usize {
        let mut ids: Vec<usize> = self.communities.iter().map(|&(_, c)| c).collect();
        ids.sort_unstable();
        ids.dedup();
        ids.len()
    }
}

/// A working undirected weighted graph used across aggregation levels.
struct Level {
    /// `adj[i]` = `(neighbour, weight)` for `neighbour != i`, symmetric.
    adj: Vec<Vec<(usize, f64)>>,
    /// Self-loop weight at each node (contributes `2 * loops[i]` to degree).
    loops: Vec<f64>,
    /// Weighted degree of each node.
    degree: Vec<f64>,
}

impl Level {
    fn node_count(&self) -> usize {
        self.adj.len()
    }
}

/// Run Louvain community detection over `graph`.
///
/// Always terminates. An empty graph — or one with no edge weight at all —
/// yields one singleton community per node, modularity `0.0`, and `0` levels.
pub fn louvain(graph: &AdjacencyList, config: &LouvainConfig) -> LouvainResult {
    let n0 = graph.node_count();
    if n0 == 0 {
        return LouvainResult {
            communities: Vec::new(),
            modularity: 0.0,
            levels: 0,
        };
    }

    let (adj0, loops0) = graph.undirected();
    let degree0: Vec<f64> = (0..n0)
        .map(|i| adj0[i].iter().map(|&(_, w)| w).sum::<f64>() + 2.0 * loops0[i])
        .collect();
    let two_m: f64 = degree0.iter().sum();

    // No edge mass: every node is its own community, modularity is 0.
    if two_m <= 0.0 {
        let communities = (0..n0).map(|i| (graph.id_at(i), i)).collect();
        return LouvainResult {
            communities,
            modularity: 0.0,
            levels: 0,
        };
    }

    // `super_of[orig]` = the current-level super-node that original node
    // `orig` belongs to. Updated after every aggregation.
    let mut super_of: Vec<usize> = (0..n0).collect();

    let mut level = Level {
        adj: adj0.clone(),
        loops: loops0.clone(),
        degree: degree0.clone(),
    };

    let mut levels = 0;
    loop {
        let (comm, moved) = local_moving(&level, two_m, config.resolution);
        if !moved {
            break;
        }
        levels += 1;

        // Renumber the communities present into a contiguous `0..k` range,
        // first-seen order, for determinism.
        let (relabel, k) = contiguous_relabel(&comm);

        // Project every original node onto its new super-node.
        for s in super_of.iter_mut() {
            *s = relabel[&comm[*s]];
        }

        level = aggregate(&level, &comm, &relabel, k);

        if level.node_count() == comm.len() {
            // No actual merge happened (already at the local optimum).
            break;
        }
        if levels >= config.max_levels {
            break;
        }
    }

    let modularity = modularity(
        &adj0,
        &loops0,
        &degree0,
        &super_of,
        two_m,
        config.resolution,
    );

    let (final_relabel, _) = contiguous_relabel(&super_of);
    let mut communities: Vec<(u64, usize)> = (0..n0)
        .map(|i| (graph.id_at(i), final_relabel[&super_of[i]]))
        .collect();
    communities.sort_by_key(|&(id, _)| id);

    LouvainResult {
        communities,
        modularity,
        levels,
    }
}

/// One local-moving phase over `level`. Returns the per-node community
/// assignment and whether any node moved out of its initial singleton.
fn local_moving(level: &Level, two_m: f64, resolution: f64) -> (Vec<usize>, bool) {
    let n = level.node_count();
    let mut comm: Vec<usize> = (0..n).collect();
    let mut sigma_tot: Vec<f64> = level.degree.clone();

    let mut any_moved = false;
    let mut inner_guard = 0;
    // Bound the sweeps; modularity rises monotonically, so this converges
    // quickly. The guard only protects against float-induced ping-pong.
    let max_sweeps = n.saturating_add(1).min(1000);
    loop {
        inner_guard += 1;
        let mut moved_this_sweep = false;

        for i in 0..n {
            let ki = level.degree[i];

            // Weight from `i` to each neighbouring community.
            let mut to_comm: HashMap<usize, f64> = HashMap::new();
            for &(j, w) in &level.adj[i] {
                *to_comm.entry(comm[j]).or_insert(0.0) += w;
            }

            let ci = comm[i];
            // Remove `i` from its community before scoring candidates.
            sigma_tot[ci] -= ki;

            // Baseline: re-inserting into the (now `i`-free) original
            // community. Isolation has gain 0, so it is the floor.
            let w_to_ci = to_comm.get(&ci).copied().unwrap_or(0.0);
            let mut best_comm = ci;
            let mut best_gain = gain(w_to_ci, sigma_tot[ci], ki, two_m, resolution).max(0.0);

            for (&c, &w_ic) in &to_comm {
                if c == ci {
                    continue;
                }
                let g = gain(w_ic, sigma_tot[c], ki, two_m, resolution);
                if g > best_gain + GAIN_EPS {
                    best_gain = g;
                    best_comm = c;
                }
            }

            sigma_tot[best_comm] += ki;
            comm[i] = best_comm;
            if best_comm != ci {
                moved_this_sweep = true;
                any_moved = true;
            }
        }

        if !moved_this_sweep || inner_guard >= max_sweeps {
            break;
        }
    }

    (comm, any_moved)
}

/// Modularity gain of inserting isolated node `i` into a community whose total
/// degree (excluding `i`) is `sigma_tot` and to which `i` connects with weight
/// `w_i_to_c`. This is the standard ΔQ scaled by a positive constant, so it is
/// valid both for comparing candidates and for the `> 0` move test.
#[inline]
fn gain(w_i_to_c: f64, sigma_tot: f64, ki: f64, two_m: f64, resolution: f64) -> f64 {
    w_i_to_c - resolution * sigma_tot * ki / two_m
}

/// Map the distinct community labels in `comm` to contiguous `0..k`, assigning
/// new IDs in first-seen order. Returns `(old -> new, k)`.
fn contiguous_relabel(comm: &[usize]) -> (HashMap<usize, usize>, usize) {
    let mut relabel: HashMap<usize, usize> = HashMap::new();
    for &c in comm {
        let next = relabel.len();
        relabel.entry(c).or_insert(next);
    }
    let k = relabel.len();
    (relabel, k)
}

/// Collapse `level` according to `comm` (relabelled to `0..k`) into a new
/// `Level` of `k` super-nodes, conserving total edge weight `two_m`.
fn aggregate(level: &Level, comm: &[usize], relabel: &HashMap<usize, usize>, k: usize) -> Level {
    let mut loops = vec![0.0; k];
    // Carry forward existing self-loops.
    for (i, &l) in level.loops.iter().enumerate() {
        loops[relabel[&comm[i]]] += l;
    }

    // Accumulate cross-community weight (each undirected edge is seen twice,
    // once from each endpoint) and internal weight.
    let mut cross: HashMap<(usize, usize), f64> = HashMap::new();
    for i in 0..level.node_count() {
        let cu = relabel[&comm[i]];
        for &(j, w) in &level.adj[i] {
            let cv = relabel[&comm[j]];
            if cu == cv {
                // Internal edge, counted twice across i and j -> halve.
                loops[cu] += w / 2.0;
            } else {
                let key = if cu < cv { (cu, cv) } else { (cv, cu) };
                *cross.entry(key).or_insert(0.0) += w;
            }
        }
    }

    let mut adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); k];
    for ((cu, cv), total) in cross {
        let w = total / 2.0; // each edge counted from both sides
        adj[cu].push((cv, w));
        adj[cv].push((cu, w));
    }
    for row in &mut adj {
        row.sort_by_key(|&(j, _)| j);
    }

    let degree: Vec<f64> = (0..k)
        .map(|c| adj[c].iter().map(|&(_, w)| w).sum::<f64>() + 2.0 * loops[c])
        .collect();

    Level { adj, loops, degree }
}

/// Modularity of the partition `super_of` over the *original* level-0 graph.
fn modularity(
    adj0: &[Vec<(usize, f64)>],
    loops0: &[f64],
    degree0: &[f64],
    super_of: &[usize],
    two_m: f64,
    resolution: f64,
) -> f64 {
    let n = adj0.len();
    // Per-community internal weight (A_ij over same-community pairs, with the
    // self-loop diagonal A_ii = 2*loops) and total degree.
    let mut internal: HashMap<usize, f64> = HashMap::new();
    let mut tot: HashMap<usize, f64> = HashMap::new();
    for i in 0..n {
        let ci = super_of[i];
        for &(j, w) in &adj0[i] {
            if super_of[j] == ci {
                *internal.entry(ci).or_insert(0.0) += w;
            }
        }
        *internal.entry(ci).or_insert(0.0) += 2.0 * loops0[i];
        *tot.entry(ci).or_insert(0.0) += degree0[i];
    }

    let mut q = 0.0;
    for (c, &inside) in &internal {
        let stot = tot[c];
        q += inside / two_m - resolution * (stot / two_m) * (stot / two_m);
    }
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    fn community_of(r: &LouvainResult, id: u64) -> usize {
        r.communities.iter().find(|&&(i, _)| i == id).unwrap().1
    }

    fn same_community(r: &LouvainResult, a: u64, b: u64) -> bool {
        community_of(r, a) == community_of(r, b)
    }

    #[test]
    fn config_validation() {
        assert_eq!(
            LouvainConfig::new(-1.0, 50),
            Err(AlgorithmError::InvalidResolution(-1.0))
        );
        assert_eq!(
            LouvainConfig::new(1.0, 0),
            Err(AlgorithmError::InvalidIterations(0))
        );
        assert!(LouvainConfig::new(1.0, 50).is_ok());
        assert!(matches!(
            LouvainConfig::new(f64::INFINITY, 50),
            Err(AlgorithmError::InvalidResolution(_))
        ));
    }

    #[test]
    fn empty_graph() {
        let g = AdjacencyList::from_parts(vec![], Vec::<(u64, u64, f32)>::new());
        let r = louvain(&g, &LouvainConfig::default());
        assert!(r.communities.is_empty());
        assert_eq!(r.modularity, 0.0);
        assert_eq!(r.levels, 0);
    }

    #[test]
    fn isolated_nodes_are_singletons() {
        let g = AdjacencyList::from_parts(vec![1, 2, 3], Vec::<(u64, u64, f32)>::new());
        let r = louvain(&g, &LouvainConfig::default());
        assert_eq!(r.community_count(), 3);
        assert_eq!(r.modularity, 0.0);
        assert_eq!(r.communities.len(), 3);
    }

    #[test]
    fn communities_sorted_by_node_id() {
        let g = AdjacencyList::from_parts(vec![30, 10, 20], vec![(10, 20, 1.0)]);
        let r = louvain(&g, &LouvainConfig::default());
        let ids: Vec<u64> = r.communities.iter().map(|&(i, _)| i).collect();
        assert_eq!(ids, vec![10, 20, 30]);
    }

    #[test]
    fn community_ids_are_contiguous_from_zero() {
        let g = AdjacencyList::from_parts(vec![1, 2, 3, 4], vec![(1, 2, 1.0), (3, 4, 1.0)]);
        let r = louvain(&g, &LouvainConfig::default());
        let mut ids: Vec<usize> = r.communities.iter().map(|&(_, c)| c).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids, vec![0, 1]);
    }

    #[test]
    fn two_cliques_joined_by_a_bridge_split_into_two() {
        // Triangle {1,2,3}, triangle {4,5,6}, single bridge 3-4.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4, 5, 6],
            vec![
                (1, 2, 1.0),
                (2, 3, 1.0),
                (3, 1, 1.0),
                (4, 5, 1.0),
                (5, 6, 1.0),
                (6, 4, 1.0),
                (3, 4, 1.0),
            ],
        );
        let r = louvain(&g, &LouvainConfig::default());
        assert_eq!(r.community_count(), 2);
        assert!(same_community(&r, 1, 2));
        assert!(same_community(&r, 1, 3));
        assert!(same_community(&r, 4, 5));
        assert!(same_community(&r, 4, 6));
        assert!(!same_community(&r, 1, 4));
        assert!(r.modularity > 0.3, "modularity was {}", r.modularity);
    }

    #[test]
    fn single_clique_is_one_community() {
        // A 4-clique — no reason to split.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![
                (1, 2, 1.0),
                (1, 3, 1.0),
                (1, 4, 1.0),
                (2, 3, 1.0),
                (2, 4, 1.0),
                (3, 4, 1.0),
            ],
        );
        let r = louvain(&g, &LouvainConfig::default());
        assert_eq!(r.community_count(), 1);
        for id in [1, 2, 3, 4] {
            assert!(same_community(&r, 1, id));
        }
    }

    #[test]
    fn disconnected_components_are_separate_communities() {
        let g = AdjacencyList::from_parts(vec![1, 2, 3, 4], vec![(1, 2, 1.0), (3, 4, 1.0)]);
        let r = louvain(&g, &LouvainConfig::default());
        assert_eq!(r.community_count(), 2);
        assert!(same_community(&r, 1, 2));
        assert!(same_community(&r, 3, 4));
        assert!(!same_community(&r, 1, 3));
    }

    #[test]
    fn modularity_in_valid_range() {
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4, 5, 6],
            vec![
                (1, 2, 1.0),
                (2, 3, 1.0),
                (3, 1, 1.0),
                (4, 5, 1.0),
                (5, 6, 1.0),
                (6, 4, 1.0),
                (3, 4, 1.0),
            ],
        );
        let r = louvain(&g, &LouvainConfig::default());
        assert!(r.modularity >= -0.5 && r.modularity <= 1.0);
    }

    #[test]
    fn deterministic_across_runs() {
        let edges = vec![
            (1, 2, 1.0f32),
            (2, 3, 1.0),
            (3, 1, 1.0),
            (4, 5, 1.0),
            (5, 6, 1.0),
            (6, 4, 1.0),
            (3, 4, 1.0),
        ];
        let g = AdjacencyList::from_parts(vec![1, 2, 3, 4, 5, 6], edges);
        let a = louvain(&g, &LouvainConfig::default());
        let b = louvain(&g, &LouvainConfig::default());
        assert_eq!(a, b);
    }

    #[test]
    fn high_resolution_yields_more_communities() {
        // Two triangles + bridge. Very high resolution penalises large
        // communities, breaking them up further than the default.
        let edges = vec![
            (1, 2, 1.0f32),
            (2, 3, 1.0),
            (3, 1, 1.0),
            (4, 5, 1.0),
            (5, 6, 1.0),
            (6, 4, 1.0),
            (3, 4, 1.0),
        ];
        let g = AdjacencyList::from_parts(vec![1, 2, 3, 4, 5, 6], edges);
        let default = louvain(&g, &LouvainConfig::default());
        let high = louvain(&g, &LouvainConfig::new(5.0, 50).unwrap());
        assert!(high.community_count() >= default.community_count());
    }

    #[test]
    fn weighted_edges_pull_strongly_linked_nodes_together() {
        // Nodes 1-2 strongly linked; 2-3 weakly. With 3 also pulled toward a
        // separate cluster, weight should keep 1 and 2 together.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![(1, 2, 10.0), (2, 3, 0.1), (3, 4, 10.0)],
        );
        let r = louvain(&g, &LouvainConfig::default());
        assert!(same_community(&r, 1, 2));
        assert!(same_community(&r, 3, 4));
    }
}
