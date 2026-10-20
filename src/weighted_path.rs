//! Lowest-cost paths over weighted relationships, for routing.
//!
//! [`weighted_path`](crate::weighted_path::weighted_path()) finds the path
//! from one node to another whose summed relationship cost is smallest. The
//! cost of a relationship is a numeric property of it (`walk_s`, `car_s`, …)
//! or its built-in weight, so one road graph serves several travel modes. The
//! search follows only the chosen relationship kinds in the chosen direction,
//! and it can stop at a cost budget.
//!
//! With a [`GeoHeuristic`](crate::weighted_path::GeoHeuristic) the search is
//! A*: the straight-line (haversine) distance to the target divided by the
//! fastest possible speed is a lower bound on the remaining cost, so the
//! search heads for the target. Without it the search is a bidirectional
//! Dijkstra that grows from both ends.
//!
//! ```
//! use std::sync::Arc;
//! use drevo::model::{Direction, Edge, Node};
//! use drevo::weighted_path::{weighted_path, EdgeCost, WeightedPathOptions};
//!
//! // a -(2)-> b -(2)-> c, and a direct a -(5)-> c.
//! let edge = |id: u64, from: u64, to: u64, cost: f64| Edge {
//!     id,
//!     uuid: [0; 16],
//!     from_id: from,
//!     to_id: to,
//!     kind: "ROAD".into(),
//!     weight: 1.0,
//!     created_at: 0,
//!     properties: drevo::model::Properties::from([("walk_s", serde_json::json!(cost))]),
//! };
//! let edges = vec![edge(10, 1, 2, 2.0), edge(11, 2, 3, 2.0), edge(12, 1, 3, 5.0)];
//! let node = |id: u64| Arc::new(Node {
//!     id,
//!     uuid: [0; 16],
//!     created_at: 0,
//!     updated_at: 0,
//!     kind: "RoadNode".into(),
//!     title: String::new(),
//!     body: String::new(),
//!     body_html: String::new(),
//!     properties: Default::default(),
//! });
//! let options = WeightedPathOptions {
//!     cost: EdgeCost::Property("walk_s".into()),
//!     ..WeightedPathOptions::default()
//! };
//! let path = weighted_path(
//!     1,
//!     3,
//!     &options,
//!     &|id| Ok((1..=3).contains(&id).then(|| node(id))),
//!     &|id, dir, visit| {
//!         edges
//!             .iter()
//!             .filter(|e| match dir {
//!                 Direction::Outgoing => e.from_id == id,
//!                 Direction::Incoming => e.to_id == id,
//!                 Direction::Both => e.from_id == id || e.to_id == id,
//!             })
//!             .for_each(&mut *visit);
//!         Ok(())
//!     },
//! )?
//! .expect("c is reachable");
//! assert_eq!(path.nodes, vec![1, 2, 3]);
//! assert_eq!(path.edges, vec![10, 11]);
//! assert_eq!(path.cost, 4.0);
//! # Ok::<(), drevo::error::DrevoError>(())
//! ```

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use crate::error::{DrevoError, Result};
use crate::model::{Direction, Edge, Node};

/// Mean Earth radius in metres (IUGG), the radius of the haversine distance.
const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// Where the cost of a relationship comes from.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum EdgeCost {
    /// The relationship's built-in `weight`.
    #[default]
    Weight,
    /// A numeric property of the relationship. A relationship without it, or
    /// with `null`, cannot be used.
    Property(String),
}

/// Straight-line lower bound on the remaining cost, which turns the search
/// into A*.
///
/// Nodes carry their position in the `lat` / `lon` properties (degrees). The
/// bound is the haversine distance to the target in metres divided by
/// `max_speed_mps`. It is exact only when no relationship is cheaper than its
/// straight-line length at that speed, so `max_speed_mps` must be at least the
/// fastest speed any relationship allows. A node without a position gets a
/// bound of zero.
#[derive(Debug, Clone, PartialEq)]
pub struct GeoHeuristic {
    /// Node property holding the latitude in degrees.
    pub lat: String,
    /// Node property holding the longitude in degrees.
    pub lon: String,
    /// The fastest speed any relationship allows, in metres per second.
    pub max_speed_mps: f64,
}

/// What [`weighted_path`] searches over.
#[derive(Debug, Clone, PartialEq)]
pub struct WeightedPathOptions {
    /// Relationship kinds that may be used; empty means every kind.
    pub kinds: Vec<String>,
    /// Which way relationships may be followed.
    pub direction: Direction,
    /// Where each relationship's cost comes from.
    pub cost: EdgeCost,
    /// A* lower bound; `None` runs a bidirectional Dijkstra.
    pub heuristic: Option<GeoHeuristic>,
    /// Paths that cost more than this are not returned.
    pub max_cost: Option<f64>,
}

impl Default for WeightedPathOptions {
    fn default() -> Self {
        Self {
            kinds: Vec::new(),
            direction: Direction::Outgoing,
            cost: EdgeCost::Weight,
            heuristic: None,
            max_cost: None,
        }
    }
}

/// A lowest-cost path: the node ids from start to end, the relationship ids
/// between them, and the summed cost.
#[derive(Debug, Clone, PartialEq)]
pub struct WeightedPath {
    /// Node ids from the start node to the end node.
    pub nodes: Vec<u64>,
    /// Relationship ids, one fewer than `nodes`.
    pub edges: Vec<u64>,
    /// Sum of the relationship costs.
    pub cost: f64,
}

/// Find the lowest-cost path from `from` to `to`.
///
/// `get_node` looks a node up by id and `edges_of(node, direction, f)` calls
/// `f` with each of a node's relationships in one direction, by reference
/// (as [`GraphEngine::for_each_edge_of`](drevo_core::engine::GraphEngine::for_each_edge_of)
/// does), so the search copies no relationship record. Returns `Ok(None)` when either node is
/// missing, when `to` cannot be reached, or when every path costs more than
/// [`WeightedPathOptions::max_cost`]. From a node to itself the path is that
/// one node at cost zero.
///
/// # Errors
///
/// [`DrevoError::InvalidArgument`] when the options are out of range (a
/// negative or non-finite `max_cost` or `max_speed_mps`), or when a
/// relationship the search reaches has a cost that is negative, not finite,
/// or not a number. Errors from the two closures are passed through.
pub fn weighted_path<N, E>(
    from: u64,
    to: u64,
    options: &WeightedPathOptions,
    get_node: &N,
    edges_of: &E,
) -> Result<Option<WeightedPath>>
where
    N: Fn(u64) -> Result<Option<Arc<Node>>>,
    E: Fn(u64, Direction, &mut dyn FnMut(&Edge)) -> Result<()>,
{
    validate(options)?;
    let (Some(start), Some(end)) = (get_node(from)?, get_node(to)?) else {
        return Ok(None);
    };
    if from == to {
        return Ok(Some(WeightedPath {
            nodes: vec![from],
            edges: Vec::new(),
            cost: 0.0,
        }));
    }
    let search = Search { options, edges_of };
    match &options.heuristic {
        Some(h) => match position(&end, h) {
            Some(target) => {
                let bound = |id: u64| -> Result<f64> {
                    let here = if id == from {
                        position(&start, h)
                    } else {
                        get_node(id)?.and_then(|n| position(&n, h))
                    };
                    Ok(here.map_or(0.0, |p| haversine_m(p, target) / h.max_speed_mps))
                };
                search.a_star(from, to, &bound)
            }
            // Without the target's position there is no bound to steer by.
            None => search.bidirectional(from, to),
        },
        None => search.bidirectional(from, to),
    }
}

/// A node's `(lat, lon)` under the heuristic's property names, when both are
/// finite numbers.
fn position(node: &Node, h: &GeoHeuristic) -> Option<(f64, f64)> {
    let lat = node.properties.get(&h.lat)?.as_f64()?;
    let lon = node.properties.get(&h.lon)?.as_f64()?;
    (lat.is_finite() && lon.is_finite()).then_some((lat, lon))
}

/// A heap entry: `key` orders the heap (smallest first), `cost` is the cost
/// from the side's start to `node`.
#[derive(Debug, Clone, Copy)]
struct Entry {
    key: f64,
    cost: f64,
    node: u64,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Entry {}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed so the max-heap `BinaryHeap` pops the smallest key; ties
        // break on the node id for a deterministic answer.
        other
            .key
            .total_cmp(&self.key)
            .then_with(|| other.node.cmp(&self.node))
    }
}

/// One side of a search: the best known cost to each node and the
/// relationship it was reached by.
#[derive(Default)]
struct Side {
    cost: HashMap<u64, f64>,
    via: HashMap<u64, (u64, u64)>,
    heap: BinaryHeap<Entry>,
}

impl Side {
    fn starting_at(node: u64, key: f64) -> Self {
        let mut side = Self::default();
        side.cost.insert(node, 0.0);
        side.heap.push(Entry {
            key,
            cost: 0.0,
            node,
        });
        side
    }

    /// Record `node` at `cost`, reached from `prev` over relationship `edge`,
    /// when that beats what is known. Returns whether it did.
    fn relax(&mut self, node: u64, cost: f64, key: f64, prev: u64, edge: u64) -> bool {
        match self.cost.entry(node) {
            std::collections::hash_map::Entry::Occupied(mut known) => {
                if cost >= *known.get() {
                    return false;
                }
                known.insert(cost);
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(cost);
            }
        }
        self.via.insert(node, (prev, edge));
        self.heap.push(Entry { key, cost, node });
        true
    }

    /// Pop the next entry that is not stale.
    fn pop(&mut self) -> Option<Entry> {
        while let Some(e) = self.heap.pop() {
            if self.cost.get(&e.node).is_some_and(|best| e.cost <= *best) {
                return Some(e);
            }
        }
        None
    }

    fn min_key(&self) -> f64 {
        self.heap.peek().map_or(f64::INFINITY, |e| e.key)
    }

    /// Nodes and relationships from this side's start to `node`, start first.
    fn walk_back(&self, node: u64) -> (Vec<u64>, Vec<u64>) {
        let mut nodes = vec![node];
        let mut edges = Vec::new();
        let mut cur = node;
        while let Some(&(prev, edge)) = self.via.get(&cur) {
            nodes.push(prev);
            edges.push(edge);
            cur = prev;
        }
        nodes.reverse();
        edges.reverse();
        (nodes, edges)
    }
}

/// The parts every search shares: which relationships are usable and what
/// they cost.
struct Search<'a, E> {
    options: &'a WeightedPathOptions,
    edges_of: &'a E,
}

impl<E> Search<'_, E>
where
    E: Fn(u64, Direction, &mut dyn FnMut(&Edge)) -> Result<()>,
{
    /// The usable relationships of `node` in `direction`, each with the node
    /// at its other end and its cost.
    fn steps(&self, node: u64, direction: Direction) -> Result<Vec<(u64, u64, f64)>> {
        let mut out = Vec::new();
        let mut failure = None;
        (self.edges_of)(node, direction, &mut |edge| {
            if failure.is_some()
                || (!self.options.kinds.is_empty() && !self.options.kinds.contains(&edge.kind))
            {
                return;
            }
            let cost = match self.cost_of(edge) {
                Ok(Some(cost)) => cost,
                Ok(None) => return,
                Err(e) => {
                    failure = Some(e);
                    return;
                }
            };
            let next = match direction {
                Direction::Outgoing => edge.to_id,
                Direction::Incoming => edge.from_id,
                Direction::Both if edge.from_id == node => edge.to_id,
                Direction::Both => edge.from_id,
            };
            out.push((next, edge.id, cost));
        })?;
        match failure {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    /// The cost of `edge`, `None` when it has no cost and cannot be used.
    fn cost_of(&self, edge: &Edge) -> Result<Option<f64>> {
        let cost = match &self.options.cost {
            EdgeCost::Weight => f64::from(edge.weight),
            EdgeCost::Property(name) => match edge.properties.get(name) {
                None | Some(serde_json::Value::Null) => return Ok(None),
                Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
                Some(other) => {
                    return Err(DrevoError::InvalidArgument(format!(
                        "cost property `{name}` of relationship {} is {other}, not a number",
                        edge.id
                    )));
                }
            },
        };
        if !cost.is_finite() || cost < 0.0 {
            return Err(DrevoError::InvalidArgument(format!(
                "relationship {} has cost {cost}; costs must be non-negative numbers",
                edge.id
            )));
        }
        Ok(Some(cost))
    }

    fn within_budget(&self, cost: f64) -> bool {
        self.options.max_cost.is_none_or(|max| cost <= max)
    }

    /// A*: settle nodes in order of cost so far plus the lower bound to `to`.
    fn a_star<H>(&self, from: u64, to: u64, bound: &H) -> Result<Option<WeightedPath>>
    where
        H: Fn(u64) -> Result<f64>,
    {
        let direction = self.options.direction;
        let mut side = Side::starting_at(from, bound(from)?);
        let mut bounds: HashMap<u64, f64> = HashMap::new();
        while let Some(Entry { cost, node, .. }) = side.pop() {
            if node == to {
                let (nodes, edges) = side.walk_back(to);
                return Ok(Some(WeightedPath { nodes, edges, cost }));
            }
            for (next, edge, step) in self.steps(node, direction)? {
                let reached = cost + step;
                let h = match bounds.get(&next) {
                    Some(h) => *h,
                    None => {
                        let h = bound(next)?;
                        bounds.insert(next, h);
                        h
                    }
                };
                // The bound never overestimates, so a path over budget here
                // stays over budget.
                if self.within_budget(reached + h) {
                    side.relax(next, reached, reached + h, node, edge);
                }
            }
        }
        Ok(None)
    }

    /// Bidirectional Dijkstra: grow from both ends, always on the side with
    /// the cheaper frontier, until the two frontiers together cost at least
    /// the best meeting found.
    fn bidirectional(&self, from: u64, to: u64) -> Result<Option<WeightedPath>> {
        let forward_dir = self.options.direction;
        let backward_dir = match forward_dir {
            Direction::Outgoing => Direction::Incoming,
            Direction::Incoming => Direction::Outgoing,
            Direction::Both => Direction::Both,
        };
        let mut forward = Side::starting_at(from, 0.0);
        let mut backward = Side::starting_at(to, 0.0);
        let mut best: Option<(f64, u64)> = None;
        loop {
            let (fk, bk) = (forward.min_key(), backward.min_key());
            if fk.is_infinite() || bk.is_infinite() {
                break;
            }
            if best.is_some_and(|(cost, _)| fk + bk >= cost) {
                break;
            }
            let go_forward = fk <= bk;
            let (this, other, dir) = if go_forward {
                (&mut forward, &backward, forward_dir)
            } else {
                (&mut backward, &forward, backward_dir)
            };
            let Some(Entry { cost, node, .. }) = this.pop() else {
                continue;
            };
            for (next, edge, step) in self.steps(node, dir)? {
                let reached = cost + step;
                if !self.within_budget(reached) {
                    continue;
                }
                this.relax(next, reached, reached, node, edge);
                if let Some(rest) = other.cost.get(&next) {
                    let total = reached + rest;
                    let better = best.is_none_or(|(cost, _)| total < cost);
                    if better && self.within_budget(total) {
                        // The meeting node's cost on this side may have been
                        // improved by `relax`; `walk_back` reads the best one.
                        best = Some((total, next));
                    }
                }
            }
        }
        let Some((cost, meet)) = best else {
            return Ok(None);
        };
        let (mut nodes, mut edges) = forward.walk_back(meet);
        let (back_nodes, back_edges) = backward.walk_back(meet);
        nodes.extend(back_nodes.into_iter().rev().skip(1));
        edges.extend(back_edges.into_iter().rev());
        Ok(Some(WeightedPath { nodes, edges, cost }))
    }
}

/// Reject options no search can honour.
fn validate(options: &WeightedPathOptions) -> Result<()> {
    if let Some(max) = options.max_cost {
        if !max.is_finite() || max < 0.0 {
            return Err(DrevoError::InvalidArgument(format!(
                "maxCost must be a non-negative number, got {max}"
            )));
        }
    }
    if let Some(h) = &options.heuristic {
        if !h.max_speed_mps.is_finite() || h.max_speed_mps <= 0.0 {
            return Err(DrevoError::InvalidArgument(format!(
                "heuristic maxSpeedMps must be a positive number, got {}",
                h.max_speed_mps
            )));
        }
    }
    Ok(())
}

/// Great-circle distance in metres between two `(lat, lon)` points in degrees.
pub fn haversine_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lon1) = (a.0.to_radians(), a.1.to_radians());
    let (lat2, lon2) = (b.0.to_radians(), b.1.to_radians());
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;
    let s = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * s.sqrt().min(1.0).asin()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    /// A small in-memory graph for the search closures.
    #[derive(Debug, Default)]
    struct Graph {
        nodes: HashMap<u64, Arc<Node>>,
        edges: Vec<Edge>,
    }

    impl Graph {
        fn node(&mut self, id: u64) -> &mut Self {
            self.node_at(id, None)
        }

        fn node_at(&mut self, id: u64, pos: Option<(f64, f64)>) -> &mut Self {
            let mut properties = crate::model::Properties::default();
            if let Some((lat, lon)) = pos {
                properties.insert("lat", json!(lat));
                properties.insert("lon", json!(lon));
            }
            self.nodes.insert(
                id,
                Arc::new(Node {
                    id,
                    uuid: [0; 16],
                    created_at: 0,
                    updated_at: 0,
                    kind: "N".into(),
                    title: String::new(),
                    body: String::new(),
                    body_html: String::new(),
                    properties,
                }),
            );
            self
        }

        /// A `kind` relationship `from -> to` with property `cost` (when
        /// `Some`) and built-in weight `weight`.
        fn edge(
            &mut self,
            id: u64,
            from: u64,
            to: u64,
            kind: &str,
            cost: Option<serde_json::Value>,
            weight: f32,
        ) -> &mut Self {
            let mut properties = crate::model::Properties::default();
            if let Some(c) = cost {
                properties.insert("cost", c);
            }
            self.edges.push(Edge {
                id,
                uuid: [0; 16],
                from_id: from,
                to_id: to,
                kind: kind.into(),
                weight,
                created_at: 0,
                properties,
            });
            self
        }

        fn road(&mut self, id: u64, from: u64, to: u64, cost: f64) -> &mut Self {
            self.edge(id, from, to, "ROAD", Some(json!(cost)), 1.0)
        }

        fn search(
            &self,
            from: u64,
            to: u64,
            options: &WeightedPathOptions,
        ) -> Result<Option<WeightedPath>> {
            weighted_path(
                from,
                to,
                options,
                &|id| Ok(self.nodes.get(&id).cloned()),
                &|id, dir, f| {
                    self.edges
                        .iter()
                        .filter(|e| match dir {
                            Direction::Outgoing => e.from_id == id,
                            Direction::Incoming => e.to_id == id,
                            Direction::Both => e.from_id == id || e.to_id == id,
                        })
                        .for_each(&mut *f);
                    Ok(())
                },
            )
        }
    }

    fn by_cost() -> WeightedPathOptions {
        WeightedPathOptions {
            cost: EdgeCost::Property("cost".into()),
            ..WeightedPathOptions::default()
        }
    }

    fn astar(max_speed_mps: f64) -> WeightedPathOptions {
        WeightedPathOptions {
            heuristic: Some(GeoHeuristic {
                lat: "lat".into(),
                lon: "lon".into(),
                max_speed_mps,
            }),
            ..by_cost()
        }
    }

    /// 1 -> 2 -> 3 -> 4 costs 3; the direct 1 -> 4 costs 10.
    fn line() -> Graph {
        let mut g = Graph::default();
        g.node(1).node(2).node(3).node(4);
        g.road(10, 1, 2, 1.0)
            .road(11, 2, 3, 1.0)
            .road(12, 3, 4, 1.0)
            .road(13, 1, 4, 10.0);
        g
    }

    #[test]
    fn picks_the_cheapest_path_not_the_shortest() {
        let path = line().search(1, 4, &by_cost()).unwrap().unwrap();
        assert_eq!(path.nodes, vec![1, 2, 3, 4]);
        assert_eq!(path.edges, vec![10, 11, 12]);
        assert_eq!(path.cost, 3.0);
    }

    #[test]
    fn a_node_to_itself_costs_nothing() {
        let path = line().search(2, 2, &by_cost()).unwrap().unwrap();
        assert_eq!(
            path,
            WeightedPath {
                nodes: vec![2],
                edges: vec![],
                cost: 0.0
            }
        );
    }

    #[test]
    fn a_missing_node_gives_no_path() {
        assert_eq!(line().search(1, 99, &by_cost()).unwrap(), None);
        assert_eq!(line().search(99, 1, &by_cost()).unwrap(), None);
    }

    #[test]
    fn an_unreachable_target_gives_no_path() {
        let mut g = line();
        g.node(5);
        assert_eq!(g.search(1, 5, &by_cost()).unwrap(), None);
        assert_eq!(g.search(1, 5, &astar(1.0)).unwrap(), None);
    }

    #[test]
    fn outgoing_paths_respect_one_way_relationships() {
        // 4 -> 1 only exists backwards.
        assert_eq!(line().search(4, 1, &by_cost()).unwrap(), None);
        let incoming = WeightedPathOptions {
            direction: Direction::Incoming,
            ..by_cost()
        };
        let path = line().search(4, 1, &incoming).unwrap().unwrap();
        assert_eq!(path.nodes, vec![4, 3, 2, 1]);
        assert_eq!(path.edges, vec![12, 11, 10]);
        let both = WeightedPathOptions {
            direction: Direction::Both,
            ..by_cost()
        };
        assert_eq!(line().search(4, 1, &both).unwrap().unwrap().cost, 3.0);
    }

    #[test]
    fn only_the_listed_kinds_are_followed() {
        // ROAD 1 -> 2 costs 5; FOOT 1 -> 3 -> 2 costs 2; SERVICE 1 -> 2 costs 1.
        let mut g = Graph::default();
        g.node(1).node(2).node(3);
        g.road(10, 1, 2, 5.0)
            .edge(11, 1, 3, "FOOT", Some(json!(1.0)), 1.0)
            .edge(12, 3, 2, "FOOT", Some(json!(1.0)), 1.0)
            .edge(13, 1, 2, "SERVICE", Some(json!(1.0)), 1.0);
        let kinds = |ks: &[&str]| WeightedPathOptions {
            kinds: ks.iter().map(|k| k.to_string()).collect(),
            ..by_cost()
        };
        assert_eq!(
            g.search(1, 2, &kinds(&["ROAD"])).unwrap().unwrap().edges,
            vec![10]
        );
        assert_eq!(
            g.search(1, 2, &kinds(&["ROAD", "FOOT"]))
                .unwrap()
                .unwrap()
                .edges,
            vec![11, 12]
        );
        assert_eq!(
            g.search(1, 2, &kinds(&[])).unwrap().unwrap().edges,
            vec![13]
        );
        assert_eq!(g.search(1, 3, &kinds(&["ROAD"])).unwrap(), None);
    }

    #[test]
    fn a_relationship_without_the_cost_property_is_not_used() {
        // The cheap 1 -> 2 has no cost (missing) and the next one has null.
        let mut g = Graph::default();
        g.node(1).node(2);
        g.edge(10, 1, 2, "ROAD", None, 1.0)
            .edge(11, 1, 2, "ROAD", Some(serde_json::Value::Null), 1.0)
            .road(12, 1, 2, 7.0);
        assert_eq!(g.search(1, 2, &by_cost()).unwrap().unwrap().edges, vec![12]);
        let mut only_missing = Graph::default();
        only_missing.node(1).node(2);
        only_missing.edge(10, 1, 2, "ROAD", None, 1.0);
        assert_eq!(only_missing.search(1, 2, &by_cost()).unwrap(), None);
    }

    #[test]
    fn without_a_cost_property_the_weight_is_the_cost() {
        let mut g = Graph::default();
        g.node(1).node(2).node(3);
        g.edge(10, 1, 3, "R", None, 9.0)
            .edge(11, 1, 2, "R", None, 2.0)
            .edge(12, 2, 3, "R", None, 2.5);
        let path = g
            .search(1, 3, &WeightedPathOptions::default())
            .unwrap()
            .unwrap();
        assert_eq!(path.edges, vec![11, 12]);
        assert_eq!(path.cost, 4.5);
    }

    #[test]
    fn max_cost_drops_paths_over_budget() {
        let within = WeightedPathOptions {
            max_cost: Some(3.0),
            ..by_cost()
        };
        assert_eq!(line().search(1, 4, &within).unwrap().unwrap().cost, 3.0);
        let below = WeightedPathOptions {
            max_cost: Some(2.5),
            ..by_cost()
        };
        assert_eq!(line().search(1, 4, &below).unwrap(), None);
        let below_astar = WeightedPathOptions {
            max_cost: Some(2.5),
            ..astar(1.0)
        };
        assert_eq!(line().search(1, 4, &below_astar).unwrap(), None);
    }

    #[test]
    fn a_negative_cost_is_an_error() {
        let mut g = Graph::default();
        g.node(1).node(2).node(3);
        g.road(10, 1, 2, 1.0)
            .road(11, 2, 3, -4.0)
            .road(12, 3, 1, 1.0);
        for options in [by_cost(), astar(1.0)] {
            let err = g.search(1, 3, &options).unwrap_err();
            assert!(
                matches!(&err, DrevoError::InvalidArgument(m) if m.contains("relationship 11") && m.contains("-4")),
                "{err}"
            );
        }
        let mut weights = Graph::default();
        weights.node(1).node(2);
        weights.edge(10, 1, 2, "R", None, -1.0);
        assert!(matches!(
            weights.search(1, 2, &WeightedPathOptions::default()),
            Err(DrevoError::InvalidArgument(_))
        ));
    }

    #[test]
    fn a_non_numeric_cost_is_an_error() {
        let mut g = Graph::default();
        g.node(1).node(2);
        g.edge(10, 1, 2, "ROAD", Some(json!("fast")), 1.0);
        let err = g.search(1, 2, &by_cost()).unwrap_err();
        assert!(
            matches!(&err, DrevoError::InvalidArgument(m) if m.contains("`cost`") && m.contains("relationship 10")),
            "{err}"
        );
    }

    #[test]
    fn out_of_range_options_are_errors() {
        for max in [-1.0, f64::NAN, f64::INFINITY] {
            let options = WeightedPathOptions {
                max_cost: Some(max),
                ..by_cost()
            };
            assert!(matches!(
                line().search(1, 4, &options),
                Err(DrevoError::InvalidArgument(_))
            ));
        }
        for speed in [0.0, -1.0, f64::NAN] {
            assert!(matches!(
                line().search(1, 4, &astar(speed)),
                Err(DrevoError::InvalidArgument(_))
            ));
        }
    }

    #[test]
    fn a_star_follows_positions_to_the_same_cost() {
        // A 3x3 grid, ~111 m per step north/east; walking at 1 m/s.
        let mut g = Graph::default();
        let id = |r: u64, c: u64| r * 3 + c + 1;
        for r in 0..3 {
            for c in 0..3 {
                g.node_at(
                    id(r, c),
                    Some((42.0 + r as f64 * 0.001, 23.0 + c as f64 * 0.001)),
                );
            }
        }
        let mut e = 100;
        for r in 0..3 {
            for c in 0..3 {
                if c < 2 {
                    g.road(e, id(r, c), id(r, c + 1), 200.0);
                    e += 1;
                }
                if r < 2 {
                    g.road(e, id(r, c), id(r + 1, c), 200.0);
                    e += 1;
                }
            }
        }
        let a = g.search(id(0, 0), id(2, 2), &astar(1.0)).unwrap().unwrap();
        let d = g.search(id(0, 0), id(2, 2), &by_cost()).unwrap().unwrap();
        assert_eq!(a.cost, 800.0);
        assert_eq!(a.cost, d.cost);
        assert_eq!(a.nodes.len(), 5);
    }

    #[test]
    fn a_node_without_a_position_still_routes() {
        let mut g = Graph::default();
        g.node_at(1, Some((42.0, 23.0)))
            .node(2)
            .node_at(3, Some((42.001, 23.0)));
        g.road(10, 1, 2, 100.0).road(11, 2, 3, 100.0);
        assert_eq!(
            g.search(1, 3, &astar(1.0)).unwrap().unwrap().nodes,
            vec![1, 2, 3]
        );
    }

    #[test]
    fn haversine_matches_a_known_distance() {
        // One degree of latitude is ~111.2 km.
        let d = haversine_m((0.0, 0.0), (1.0, 0.0));
        assert!((d - 111_195.0).abs() < 10.0, "{d}");
        assert_eq!(haversine_m((42.7, 23.3), (42.7, 23.3)), 0.0);
    }

    /// A random graph of `n` nodes on a ~1 km square with costs no smaller
    /// than the straight-line length at 10 m/s, so the A* bound holds.
    fn random_graph() -> impl Strategy<Value = (Graph, u64, u64, Direction)> {
        (2u64..12)
            .prop_flat_map(|n| {
                let positions = prop::collection::vec((0.0f64..0.01, 0.0f64..0.01), n as usize);
                let edges = prop::collection::vec((0..n, 0..n, 1.0f64..3.0), 0..40);
                let dir = prop_oneof![
                    Just(Direction::Outgoing),
                    Just(Direction::Incoming),
                    Just(Direction::Both)
                ];
                (positions, edges, 0..n, 0..n, dir)
            })
            .prop_map(|(positions, edges, from, to, dir)| {
                let mut g = Graph::default();
                for (i, (lat, lon)) in positions.iter().enumerate() {
                    g.node_at(i as u64 + 1, Some((42.0 + lat, 23.0 + lon)));
                }
                for (i, (a, b, slack)) in edges.into_iter().enumerate() {
                    let pa = (
                        42.0 + positions[a as usize].0,
                        23.0 + positions[a as usize].1,
                    );
                    let pb = (
                        42.0 + positions[b as usize].0,
                        23.0 + positions[b as usize].1,
                    );
                    let cost = (haversine_m(pa, pb) / 10.0 * slack).round() + 1.0;
                    g.road(1000 + i as u64, a + 1, b + 1, cost);
                }
                (g, from + 1, to + 1, dir)
            })
    }

    /// Reference answer: Dijkstra over every node, one direction.
    fn reference_cost(g: &Graph, from: u64, to: u64, dir: Direction) -> Option<f64> {
        let mut dist: HashMap<u64, f64> = HashMap::from([(from, 0.0)]);
        let mut done = std::collections::HashSet::new();
        loop {
            let next = dist
                .iter()
                .filter(|(id, _)| !done.contains(*id))
                .min_by(|a, b| a.1.total_cmp(b.1))
                .map(|(id, d)| (*id, *d));
            let (u, du) = next?;
            if u == to {
                return Some(du);
            }
            done.insert(u);
            for e in &g.edges {
                let v = match dir {
                    Direction::Outgoing if e.from_id == u => e.to_id,
                    Direction::Incoming if e.to_id == u => e.from_id,
                    Direction::Both if e.from_id == u => e.to_id,
                    Direction::Both if e.to_id == u => e.from_id,
                    _ => continue,
                };
                let c = e.properties["cost"].as_f64().unwrap_or(f64::INFINITY);
                let entry = dist.entry(v).or_insert(f64::INFINITY);
                if du + c < *entry {
                    *entry = du + c;
                }
            }
        }
    }

    /// The summed cost of the returned relationships equals the reported cost,
    /// and consecutive nodes are joined by those relationships.
    fn assert_consistent(g: &Graph, path: &WeightedPath, dir: Direction) {
        assert_eq!(path.edges.len() + 1, path.nodes.len());
        let mut total = 0.0;
        for (i, eid) in path.edges.iter().enumerate() {
            let e = g.edges.iter().find(|e| e.id == *eid).unwrap();
            let (a, b) = (path.nodes[i], path.nodes[i + 1]);
            let joins = match dir {
                Direction::Outgoing => e.from_id == a && e.to_id == b,
                Direction::Incoming => e.to_id == a && e.from_id == b,
                Direction::Both => {
                    (e.from_id == a && e.to_id == b) || (e.to_id == a && e.from_id == b)
                }
            };
            assert!(joins, "relationship {eid} does not join {a} and {b}");
            total += e.properties["cost"].as_f64().unwrap();
        }
        assert!((total - path.cost).abs() < 1e-9, "{total} vs {}", path.cost);
    }

    proptest! {
        #[test]
        fn a_star_and_dijkstra_agree_with_the_reference((g, from, to, dir) in random_graph()) {
            let expected = reference_cost(&g, from, to, dir);
            let dijkstra = g.search(from, to, &WeightedPathOptions { direction: dir, ..by_cost() }).unwrap();
            let a_star = g.search(from, to, &WeightedPathOptions { direction: dir, ..astar(10.0) }).unwrap();
            prop_assert_eq!(dijkstra.as_ref().map(|p| p.cost), expected);
            prop_assert_eq!(a_star.as_ref().map(|p| p.cost), expected);
            for p in dijkstra.iter().chain(a_star.iter()) {
                assert_consistent(&g, p, dir);
                prop_assert_eq!(p.nodes.first().copied(), Some(from));
                prop_assert_eq!(p.nodes.last().copied(), Some(to));
            }
        }
    }
}
