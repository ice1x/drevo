//! Ollivier–Ricci edge curvature — issue #526.
//!
//! Ricci curvature adapts a differential-geometry notion of curvature to a
//! graph, per **edge**. It marks the graph's bridges and bottlenecks:
//!
//! * a **negatively** curved edge tends to lie *between* communities — mass in
//!   the two endpoints' neighbourhoods is hard to align, so it must travel far;
//!   these are good cut candidates (community boundaries, bottlenecks).
//! * a **positively** curved edge sits *inside* a dense cluster — the two
//!   neighbourhoods overlap, so almost no mass has to move.
//!
//! # Definition (Ollivier, 2009)
//!
//! Each node `x` carries a probability measure — a *lazy one-step random walk*
//! with idleness `alpha`:
//!
//! ```text
//! mu_x(x)          = alpha
//! mu_x(neighbour)  = (1 - alpha) / degree(x)   for each neighbour
//! ```
//!
//! For an edge `{x, y}` the curvature is
//!
//! ```text
//! kappa(x, y) = 1 - W1(mu_x, mu_y) / d(x, y)
//! ```
//!
//! where `d(x, y)` is the shortest-path distance between the endpoints (`1` for
//! an edge) and `W1` is the **Wasserstein-1 / earth-mover distance**: the
//! minimum total `mass × distance` needed to reshape `mu_x` into `mu_y`, over
//! the graph's shortest-path ground metric. `W1` is solved **exactly** here as
//! a transportation problem (min-cost flow), so the curvature is exact, not a
//! combinatorial bound.
//!
//! # Conventions (matching the rest of the suite)
//!
//! Computed over the **undirected** projection of the graph, and — like
//! [`betweenness`](crate::algorithms::betweenness) and
//! [`closeness`](crate::algorithms::closeness) — the ground metric is the
//! **unweighted** hop distance; edge weights do not affect the result. Every
//! undirected edge is scored once, keyed by `(from_id, to_id)` with
//! `from_id < to_id`, so results are deterministic across runs.
//!
//! Because every point in `mu_x`'s support is a neighbour of `x` (or `x`
//! itself) and likewise for `y`, and `x`–`y` are adjacent, the ground distance
//! between any two support points is at most `3` (`a → x → y → b`). The metric
//! is therefore computed directly (`0`/`1`/`2`/`3` by adjacency and
//! common-neighbour tests) with no traversal.
//!
//! Dependency-free, infallible once configured, always compiled, WASM-safe.
//! Ricci *flow* (iteratively re-weighting edges by curvature for community
//! detection) is a heavier, separate follow-up and is not implemented here.

use std::collections::HashSet;

use super::{AdjacencyList, AlgorithmError};

/// Configuration for [`ricci_curvature`].
///
/// Construct with [`RicciConfig::new`] (validated) or [`RicciConfig::default`]
/// (the canonical idleness `alpha = 0.5`, the value the reference
/// implementations use).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RicciConfig {
    /// Idleness `alpha` — the probability mass the lazy random walk keeps at
    /// the node itself; the remaining `1 - alpha` is spread uniformly over the
    /// node's neighbours. Must be finite and in the closed interval `[0, 1]`.
    /// The canonical value is `0.5`; `alpha = 0` is the plain one-step walk and
    /// `alpha = 1` the fully lazy walk (every edge then has curvature `0`).
    pub alpha: f64,
}

impl Default for RicciConfig {
    fn default() -> Self {
        Self { alpha: 0.5 }
    }
}

impl RicciConfig {
    /// Build a validated config.
    ///
    /// # Errors
    ///
    /// - [`AlgorithmError::InvalidAlpha`] if `alpha` is not a finite number in
    ///   the closed interval `[0, 1]`.
    pub fn new(alpha: f64) -> Result<Self, AlgorithmError> {
        if !(alpha.is_finite() && (0.0..=1.0).contains(&alpha)) {
            return Err(AlgorithmError::InvalidAlpha(alpha));
        }
        Ok(Self { alpha })
    }
}

/// The Ollivier–Ricci curvature of one undirected edge.
#[derive(Debug, Clone, PartialEq)]
pub struct RicciEdge {
    /// Smaller endpoint node ID.
    pub from_id: u64,
    /// Larger endpoint node ID.
    pub to_id: u64,
    /// Ollivier–Ricci curvature `kappa(from, to)`. Negative on bridge-like
    /// edges between communities, positive inside dense clusters. Bounded above
    /// by `1`; the lower bound depends on the graph (roughly `-2` for a long
    /// path's interior at `alpha = 0`).
    pub curvature: f64,
}

/// The result of a [`ricci_curvature`] run.
#[derive(Debug, Clone, PartialEq)]
pub struct RicciResult {
    /// One entry per undirected edge, sorted ascending by `(from_id, to_id)`.
    pub per_edge: Vec<RicciEdge>,
}

/// Compute the Ollivier–Ricci curvature of every undirected edge of `graph`.
///
/// Always terminates. Works on the undirected projection: reciprocal and
/// parallel directed edges collapse to a single undirected edge (scored once),
/// and self-loops are ignored. An empty or edgeless graph yields an empty
/// result.
///
/// Cost is one optimal-transport solve per edge over the two endpoints'
/// 1-hop neighbourhoods, so it is `O(edges × d²·log d)`-ish in the endpoint
/// degrees `d` — cheap on the sparse graphs the target scenarios produce, but
/// quadratic per edge in the degree, so a very dense hub is the worst case.
pub fn ricci_curvature(graph: &AdjacencyList, config: &RicciConfig) -> RicciResult {
    let n = graph.node_count();
    if n == 0 {
        return RicciResult {
            per_edge: Vec::new(),
        };
    }

    // Undirected, de-duplicated neighbour lists (no self-loops), plus membership
    // sets for O(1) adjacency / common-neighbour tests.
    let (adj, _loops) = graph.undirected();
    let neighbours: Vec<Vec<usize>> = adj
        .iter()
        .map(|nb| nb.iter().map(|&(j, _)| j).collect())
        .collect();
    let sets: Vec<HashSet<usize>> = neighbours
        .iter()
        .map(|nb| nb.iter().copied().collect())
        .collect();

    let alpha = config.alpha;
    let mut per_edge = Vec::new();
    for x in 0..n {
        for &y in &neighbours[x] {
            // Score each undirected edge exactly once.
            if x >= y {
                continue;
            }
            let curvature = edge_curvature(x, y, &neighbours, &sets, alpha);
            let (id_x, id_y) = (graph.id_at(x), graph.id_at(y));
            let (from_id, to_id) = if id_x <= id_y {
                (id_x, id_y)
            } else {
                (id_y, id_x)
            };
            per_edge.push(RicciEdge {
                from_id,
                to_id,
                curvature,
            });
        }
    }
    per_edge.sort_unstable_by_key(|e| (e.from_id, e.to_id));

    RicciResult { per_edge }
}

/// Curvature of the single edge `{x, y}` (dense indices). `d(x, y) = 1`.
fn edge_curvature(
    x: usize,
    y: usize,
    neighbours: &[Vec<usize>],
    sets: &[HashSet<usize>],
    alpha: f64,
) -> f64 {
    let (support_x, mass_x) = measure(x, neighbours, alpha);
    let (support_y, mass_y) = measure(y, neighbours, alpha);
    let cost = |i: usize, j: usize| node_distance(support_x[i], support_y[j], sets);
    let w1 = earth_mover_distance(&mass_x, &mass_y, cost);
    1.0 - w1
}

/// The lazy-walk measure at node `x`: support `[x, neighbours…]` with mass
/// `alpha` at `x` and `(1 - alpha) / degree` on each neighbour. `x` always has
/// at least one neighbour here (it is an edge endpoint), so `degree >= 1`.
fn measure(x: usize, neighbours: &[Vec<usize>], alpha: f64) -> (Vec<usize>, Vec<f64>) {
    let deg = neighbours[x].len();
    let mut support = Vec::with_capacity(deg + 1);
    let mut mass = Vec::with_capacity(deg + 1);
    support.push(x);
    mass.push(alpha);
    let share = if deg > 0 {
        (1.0 - alpha) / deg as f64
    } else {
        0.0
    };
    for &v in &neighbours[x] {
        support.push(v);
        mass.push(share);
    }
    (support, mass)
}

/// Exact shortest-path hop distance between two support points. Both lie in the
/// closed 1-hop neighbourhood of adjacent endpoints, so the true distance is at
/// most `3` and is resolved directly: `0` (same node), `1` (adjacent), `2` (a
/// shared neighbour exists), else `3`.
fn node_distance(a: usize, b: usize, sets: &[HashSet<usize>]) -> f64 {
    if a == b {
        return 0.0;
    }
    if sets[a].contains(&b) {
        return 1.0;
    }
    // Distance 2 iff they share at least one common neighbour. Scan the smaller
    // set for cache friendliness.
    let (small, large) = if sets[a].len() <= sets[b].len() {
        (&sets[a], &sets[b])
    } else {
        (&sets[b], &sets[a])
    };
    if small.iter().any(|z| large.contains(z)) {
        return 2.0;
    }
    3.0
}

/// Exact Wasserstein-1 (earth-mover) distance between two discrete measures
/// with equal total mass, over a supplied ground-cost matrix.
///
/// Solved as a transportation problem via successive-shortest-path min-cost
/// flow. `mass_a` are the supplies (source `i`), `mass_b` the demands (sink
/// `j`), and `cost(i, j)` the per-unit transport cost. Returns the minimum
/// total `mass × cost`. Both mass vectors are assumed to sum to the same total
/// (here `1.0`); zero-mass points are harmless (their arcs simply never carry
/// flow).
fn earth_mover_distance(mass_a: &[f64], mass_b: &[f64], cost: impl Fn(usize, usize) -> f64) -> f64 {
    let s = mass_a.len();
    let t = mass_b.len();
    // Node layout: 0 = super-source, 1..=s suppliers, s+1..=s+t sinks,
    // s + t + 1 = super-sink.
    let source = 0;
    let sink = s + t + 1;
    let mut mcmf = Mcmf::new(s + t + 2);
    for (i, &m) in mass_a.iter().enumerate() {
        mcmf.add_edge(source, 1 + i, m, 0.0);
    }
    for (j, &m) in mass_b.iter().enumerate() {
        mcmf.add_edge(1 + s + j, sink, m, 0.0);
    }
    for i in 0..s {
        for j in 0..t {
            mcmf.add_edge(1 + i, 1 + s + j, f64::INFINITY, cost(i, j));
        }
    }
    mcmf.min_cost_flow(source, sink)
}

/// A minimal successive-shortest-path min-cost max-flow solver over real-valued
/// capacities, specialised for the tiny transportation instances Ricci curvature
/// produces (a few dozen nodes at most).
///
/// Residual edges are stored in adjacency-paired form: edge `e` and its reverse
/// `e ^ 1`. Shortest (min-cost) augmenting paths are found with a
/// Bellman-Ford/SPFA relaxation — costs are non-negative on forward arcs and the
/// min-cost-flow invariant keeps the residual graph free of negative cycles, so
/// the relaxation is safe and each augmentation pushes flow along a cheapest
/// path.
struct Mcmf {
    /// Head of each residual arc.
    to: Vec<usize>,
    /// Remaining capacity of each residual arc.
    cap: Vec<f64>,
    /// Per-unit cost of each residual arc (negative on reverse arcs).
    cost: Vec<f64>,
    /// Incident arc indices per node.
    incident: Vec<Vec<usize>>,
}

impl Mcmf {
    fn new(nodes: usize) -> Self {
        Self {
            to: Vec::new(),
            cap: Vec::new(),
            cost: Vec::new(),
            incident: vec![Vec::new(); nodes],
        }
    }

    /// Add a directed arc `u -> v` with the given capacity and cost, plus its
    /// zero-capacity residual reverse `v -> u` at cost `-cost`.
    fn add_edge(&mut self, u: usize, v: usize, cap: f64, cost: f64) {
        let forward = self.to.len();
        self.to.push(v);
        self.cap.push(cap);
        self.cost.push(cost);
        self.incident[u].push(forward);

        let backward = self.to.len();
        self.to.push(u);
        self.cap.push(0.0);
        self.cost.push(-cost);
        self.incident[v].push(backward);
    }

    /// Push all feasible flow from `source` to `sink` along successively
    /// cheapest paths; return the total `flow × cost` (the optimal transport
    /// cost). Terminates when the sink is unreachable in the residual graph.
    fn min_cost_flow(&mut self, source: usize, sink: usize) -> f64 {
        const EPS: f64 = 1e-12;
        let n = self.incident.len();
        let mut total_cost = 0.0;

        loop {
            // SPFA: cheapest cost from `source` to every node, with the arc used
            // to reach it (for path reconstruction).
            let mut dist = vec![f64::INFINITY; n];
            let mut prev_arc = vec![usize::MAX; n];
            let mut in_queue = vec![false; n];
            dist[source] = 0.0;
            let mut queue = std::collections::VecDeque::new();
            queue.push_back(source);
            in_queue[source] = true;

            while let Some(u) = queue.pop_front() {
                in_queue[u] = false;
                let du = dist[u];
                for &arc in &self.incident[u] {
                    if self.cap[arc] <= EPS {
                        continue;
                    }
                    let v = self.to[arc];
                    let nd = du + self.cost[arc];
                    if nd + EPS < dist[v] {
                        dist[v] = nd;
                        prev_arc[v] = arc;
                        if !in_queue[v] {
                            in_queue[v] = true;
                            queue.push_back(v);
                        }
                    }
                }
            }

            if !dist[sink].is_finite() {
                break; // no augmenting path — all supply routed.
            }

            // Bottleneck residual capacity along the reconstructed path.
            let mut bottleneck = f64::INFINITY;
            let mut v = sink;
            while v != source {
                let arc = prev_arc[v];
                bottleneck = bottleneck.min(self.cap[arc]);
                v = self.to[arc ^ 1];
            }
            if bottleneck <= EPS {
                break;
            }

            // Apply the augmentation.
            let mut v = sink;
            while v != source {
                let arc = prev_arc[v];
                self.cap[arc] -= bottleneck;
                self.cap[arc ^ 1] += bottleneck;
                v = self.to[arc ^ 1];
            }
            total_cost += bottleneck * dist[sink];
        }

        total_cost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curv(r: &RicciResult, from: u64, to: u64) -> f64 {
        let (a, b) = if from <= to { (from, to) } else { (to, from) };
        r.per_edge
            .iter()
            .find(|e| e.from_id == a && e.to_id == b)
            .unwrap_or_else(|| panic!("no edge {a}-{b} in result"))
            .curvature
    }

    // ---- config validation --------------------------------------------------

    #[test]
    fn default_alpha_is_one_half() {
        assert_eq!(RicciConfig::default().alpha, 0.5);
    }

    #[test]
    fn alpha_must_be_a_finite_probability() {
        assert!(RicciConfig::new(0.0).is_ok());
        assert!(RicciConfig::new(1.0).is_ok());
        assert!(RicciConfig::new(0.5).is_ok());
        assert_eq!(
            RicciConfig::new(-0.1),
            Err(AlgorithmError::InvalidAlpha(-0.1))
        );
        assert_eq!(
            RicciConfig::new(1.1),
            Err(AlgorithmError::InvalidAlpha(1.1))
        );
        assert!(matches!(
            RicciConfig::new(f64::NAN),
            Err(AlgorithmError::InvalidAlpha(_))
        ));
    }

    // ---- the earth-mover solver, hand-computable instances -------------------

    #[test]
    fn emd_moves_one_unit_across_distance_one() {
        // All supply at source 0, all demand at sink 0, cost 1 → W1 = 1.
        let w = earth_mover_distance(&[1.0], &[1.0], |_, _| 1.0);
        assert!((w - 1.0).abs() < 1e-9, "got {w}");
    }

    #[test]
    fn emd_is_zero_when_measures_coincide() {
        // Identity cost 0 on the diagonal, expensive off it → nothing moves.
        let w = earth_mover_distance(
            &[0.5, 0.5],
            &[0.5, 0.5],
            |i, j| {
                if i == j {
                    0.0
                } else {
                    5.0
                }
            },
        );
        assert!(w.abs() < 1e-9, "got {w}");
    }

    #[test]
    fn emd_splits_supply_across_two_sinks() {
        // 1 unit at source 0 → 0.5 to sink 0 (cost 1), 0.5 to sink 1 (cost 2).
        let w = earth_mover_distance(&[1.0], &[0.5, 0.5], |_, j| if j == 0 { 1.0 } else { 2.0 });
        assert!((w - 1.5).abs() < 1e-9, "got {w}");
    }

    #[test]
    fn emd_prefers_the_cheaper_assignment() {
        // Two units of mass, a 2×2 assignment. Diagonal costs 1+1=2 beats the
        // anti-diagonal 10+10, so the optimum is 2, not a naive greedy pick.
        let costs = [[1.0, 10.0], [10.0, 1.0]];
        let w = earth_mover_distance(&[0.5, 0.5], &[0.5, 0.5], |i, j| costs[i][j]);
        assert!((w - 1.0).abs() < 1e-9, "got {w}");
    }

    // ---- curvature on known shapes ------------------------------------------

    #[test]
    fn empty_graph_has_no_edges() {
        let g = AdjacencyList::from_parts(vec![], Vec::<(u64, u64, f32)>::new());
        let r = ricci_curvature(&g, &RicciConfig::default());
        assert!(r.per_edge.is_empty());
    }

    #[test]
    fn a_lone_edge_has_curvature_one_at_half_idleness() {
        // K2: mu_1 = mu_2 = (0.5 self, 0.5 other) → identical → W1 = 0 → k = 1.
        let g = AdjacencyList::from_parts(vec![1, 2], vec![(1, 2, 1.0)]);
        let r = ricci_curvature(&g, &RicciConfig::default());
        assert_eq!(r.per_edge.len(), 1);
        assert!((curv(&r, 1, 2) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_lone_edge_has_curvature_zero_without_idleness() {
        // alpha = 0: mu_1 = delta_2, mu_2 = delta_1, must cross distance 1 →
        // W1 = 1 → k = 0.
        let g = AdjacencyList::from_parts(vec![1, 2], vec![(1, 2, 1.0)]);
        let r = ricci_curvature(&g, &RicciConfig::new(0.0).unwrap());
        assert!((curv(&r, 1, 2) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn every_edge_of_a_triangle_is_positively_curved() {
        // K3: neighbourhoods overlap heavily, so mass barely moves → k > 0.
        let g =
            AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 1.0), (2, 3, 1.0), (3, 1, 1.0)]);
        let r = ricci_curvature(&g, &RicciConfig::default());
        assert_eq!(r.per_edge.len(), 3);
        for (a, b) in [(1, 2), (2, 3), (1, 3)] {
            assert!(curv(&r, a, b) > 0.0, "edge {a}-{b} should be positive");
        }
    }

    #[test]
    fn a_clique_internal_edge_beats_a_bridge_edge() {
        // Two triangles {1,2,3} and {4,5,6} joined by a single bridge 3-4.
        // The bridge lies between the two clusters → negative; the intra-triangle
        // edges sit inside a cluster → strictly more curved (positive).
        let edges = vec![
            (1, 2, 1.0),
            (2, 3, 1.0),
            (3, 1, 1.0),
            (4, 5, 1.0),
            (5, 6, 1.0),
            (6, 4, 1.0),
            (3, 4, 1.0), // bridge
        ];
        let g = AdjacencyList::from_parts(vec![1, 2, 3, 4, 5, 6], edges);
        let r = ricci_curvature(&g, &RicciConfig::default());

        let bridge = curv(&r, 3, 4);
        let intra = curv(&r, 1, 2);
        assert!(
            bridge < 0.0,
            "bridge should be negatively curved, got {bridge}"
        );
        assert!(
            intra > 0.0,
            "intra-cluster edge should be positive, got {intra}"
        );
        assert!(bridge < intra);
    }

    #[test]
    fn the_interior_edge_of_a_path_is_ricci_flat() {
        // Path 1-2-3-4. Its interior edge 2-3 joins two structurally identical
        // degree-2 nodes: the lazy walk reshapes from one to the other at zero
        // net transport cost, so the curvature is exactly 0 — a path is the
        // graph analogue of a flat 1-D lattice. The end edges touch a degree-1
        // leaf and are positively curved, so they sit strictly above the flat
        // middle.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![(1, 2, 1.0), (2, 3, 1.0), (3, 4, 1.0)],
        );
        let r = ricci_curvature(&g, &RicciConfig::default());
        let mid = curv(&r, 2, 3);
        let end = curv(&r, 1, 2);
        assert!(
            mid.abs() < 1e-9,
            "interior edge of a path should be flat, got {mid}"
        );
        assert!(
            end > mid,
            "end edge {end} should exceed the flat middle {mid}"
        );
    }

    #[test]
    fn direction_and_weight_do_not_change_the_curvature() {
        // Reciprocal + parallel + differently-weighted edges collapse to the
        // same undirected triangle as the plain one.
        let plain =
            AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 1.0), (2, 3, 1.0), (3, 1, 1.0)]);
        let messy = AdjacencyList::from_parts(
            vec![1, 2, 3],
            vec![
                (1, 2, 5.0),
                (2, 1, 0.1),
                (2, 3, 9.0),
                (3, 2, 2.0),
                (3, 1, 1.0),
                (1, 3, 4.0),
            ],
        );
        let a = ricci_curvature(&plain, &RicciConfig::default());
        let b = ricci_curvature(&messy, &RicciConfig::default());
        assert_eq!(a.per_edge.len(), b.per_edge.len());
        for (ea, eb) in a.per_edge.iter().zip(b.per_edge.iter()) {
            assert_eq!((ea.from_id, ea.to_id), (eb.from_id, eb.to_id));
            assert!((ea.curvature - eb.curvature).abs() < 1e-9);
        }
    }

    #[test]
    fn results_are_sorted_and_scored_once_per_undirected_edge() {
        let g =
            AdjacencyList::from_parts(vec![3, 1, 2], vec![(1, 2, 1.0), (2, 3, 1.0), (3, 1, 1.0)]);
        let r = ricci_curvature(&g, &RicciConfig::default());
        assert_eq!(r.per_edge.len(), 3);
        let keys: Vec<(u64, u64)> = r.per_edge.iter().map(|e| (e.from_id, e.to_id)).collect();
        assert_eq!(keys, vec![(1, 2), (1, 3), (2, 3)]);
        for e in &r.per_edge {
            assert!(e.from_id < e.to_id);
        }
    }

    #[test]
    fn fully_lazy_walk_flattens_every_edge_to_zero() {
        // alpha = 1: mu_x = delta_x, so W1 = d(x, y) = 1 → k = 0 everywhere.
        let g =
            AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 1.0), (2, 3, 1.0), (3, 1, 1.0)]);
        let r = ricci_curvature(&g, &RicciConfig::new(1.0).unwrap());
        for e in &r.per_edge {
            assert!(e.curvature.abs() < 1e-9, "got {}", e.curvature);
        }
    }
}
