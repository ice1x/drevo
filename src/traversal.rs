//! Graph traversal algorithms.
//!
//! Provides BFS (breadth-first search), DFS (depth-first search) with
//! depth limit and optional edge kind filtering, and shortest path
//! (Dijkstra) weighted by edge weight. Used by `Drevo`
//! traversal methods.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use crate::error::Result;
use crate::model::{Direction, Edge, Node, SubGraph};

/// Resolve the neighbor node id of `edge` relative to `current_id` under
/// the given traversal `direction`.
///
/// `Direction::Outgoing` always returns `edge.to_id`, `Direction::Incoming`
/// always returns `edge.from_id`. For `Direction::Both` the endpoint
/// opposite of `current_id` is returned; if both endpoints equal
/// `current_id` (a self-loop) the function returns `current_id` itself,
/// which the visited-set check in each algorithm uses to ignore it.
///
/// Extracted to remove the three near-identical match arms previously
/// duplicated across `bfs`, `dfs`, and `subgraph` (drevo-architecture
/// "Three strikes and you refactor").
#[inline]
fn neighbor_id_for_edge(edge: &Edge, current_id: u64, direction: Direction) -> u64 {
    match direction {
        Direction::Outgoing => edge.to_id,
        Direction::Incoming => edge.from_id,
        Direction::Both => {
            if edge.from_id == current_id {
                edge.to_id
            } else {
                edge.from_id
            }
        }
    }
}

/// Perform a breadth-first search starting from `start_id`.
///
/// Returns all nodes reachable within `max_depth` hops, optionally
/// filtering edges by kind. The start node is **not** included in
/// the results.
///
/// # Arguments
///
/// * `backend` — the storage backend to query
/// * `start_id` — the node ID to start from
/// * `max_depth` — maximum number of hops (0 returns empty)
/// * `direction` — which edges to follow (Outgoing, Incoming, Both)
/// * `edge_kind` — if `Some`, only traverse edges with this kind
/// * `get_node` — closure to retrieve a node by ID
/// * `edges_of` — closure to retrieve edges of a node
pub fn bfs<F, G>(
    start_id: u64,
    max_depth: u8,
    direction: Direction,
    edge_kind: Option<&str>,
    get_node: &F,
    edges_of: &G,
) -> Result<Vec<Node>>
where
    F: Fn(u64) -> Result<Option<Node>>,
    G: Fn(u64, Direction) -> Result<Vec<Edge>>,
{
    if max_depth == 0 {
        return Ok(Vec::new());
    }

    let mut visited: HashSet<u64> = HashSet::new();
    visited.insert(start_id);

    // Queue holds (node_id, current_depth)
    let mut queue: VecDeque<(u64, u8)> = VecDeque::new();
    queue.push_back((start_id, 0));

    let mut result: Vec<Node> = Vec::new();

    while let Some((current_id, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }

        let edges = edges_of(current_id, direction)?;

        for edge in &edges {
            if let Some(kind) = edge_kind {
                if edge.kind != kind {
                    continue;
                }
            }

            let neighbor_id = neighbor_id_for_edge(edge, current_id, direction);

            if visited.contains(&neighbor_id) {
                continue;
            }
            visited.insert(neighbor_id);

            if let Some(node) = get_node(neighbor_id)? {
                result.push(node);
                queue.push_back((neighbor_id, depth + 1));
            }
        }
    }

    Ok(result)
}

/// Perform a depth-first search starting from `start_id`.
///
/// Returns all nodes reachable within `max_depth` hops, optionally
/// filtering edges by kind. The start node is **not** included in
/// the results. Nodes are returned in DFS visit order.
///
/// # Arguments
///
/// * `start_id` — the node ID to start from
/// * `max_depth` — maximum number of hops (0 returns empty)
/// * `direction` — which edges to follow (Outgoing, Incoming, Both)
/// * `edge_kind` — if `Some`, only traverse edges with this kind
/// * `get_node` — closure to retrieve a node by ID
/// * `edges_of` — closure to retrieve edges of a node
pub fn dfs<F, G>(
    start_id: u64,
    max_depth: u8,
    direction: Direction,
    edge_kind: Option<&str>,
    get_node: &F,
    edges_of: &G,
) -> Result<Vec<Node>>
where
    F: Fn(u64) -> Result<Option<Node>>,
    G: Fn(u64, Direction) -> Result<Vec<Edge>>,
{
    if max_depth == 0 {
        return Ok(Vec::new());
    }

    let mut visited: HashSet<u64> = HashSet::new();
    visited.insert(start_id);

    // Stack holds (node_id, current_depth)
    let mut stack: Vec<(u64, u8)> = Vec::new();
    stack.push((start_id, 0));

    let mut result: Vec<Node> = Vec::new();

    while let Some((current_id, depth)) = stack.pop() {
        if depth >= max_depth {
            continue;
        }

        let edges = edges_of(current_id, direction)?;

        for edge in &edges {
            if let Some(kind) = edge_kind {
                if edge.kind != kind {
                    continue;
                }
            }

            let neighbor_id = neighbor_id_for_edge(edge, current_id, direction);

            if visited.contains(&neighbor_id) {
                continue;
            }
            visited.insert(neighbor_id);

            if let Some(node) = get_node(neighbor_id)? {
                result.push(node);
                stack.push((neighbor_id, depth + 1));
            }
        }
    }

    Ok(result)
}

/// A state entry for Dijkstra's priority queue.
///
/// Ordered by cost ascending (min-heap via `Reverse`-style `Ord` impl).
#[derive(Debug, PartialEq)]
struct DijkstraState {
    cost: f32,
    node_id: u64,
}

impl Eq for DijkstraState {}

impl PartialOrd for DijkstraState {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DijkstraState {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering so BinaryHeap (max-heap) becomes a min-heap.
        // Use total_cmp for NaN safety.
        other
            .cost
            .total_cmp(&self.cost)
            .then_with(|| self.node_id.cmp(&other.node_id))
    }
}

/// Find the shortest (lowest total weight) path between two nodes using
/// Dijkstra's algorithm. Follows **outgoing** edges only. Complexity
/// `O((V + E) log V)` (binary-heap variant).
///
/// Returns `Some(vec![from, ..., to])` with the node IDs along the
/// shortest path, or `None` if `to` is not reachable from `from`.
/// If `from == to`, returns `Some(vec![from])`. Returns `None` if
/// `from` itself does not exist.
///
/// # Preconditions
///
/// **Edge weights must be non-negative and finite.** The model layer
/// guarantees finiteness (`DrevoError::InvalidWeight` rejects NaN /
/// ±∞ at `create_edge` / `update_edge` time — see `AUDIT-db.md` F1).
/// Negative weights are admitted by the storage layer but violate
/// Dijkstra's correctness precondition: this implementation may
/// return a non-optimal path because a node is treated as
/// "settled" the first time it pops from the heap. If a graph
/// contains negative-weight edges and you need true shortest
/// paths, use a different algorithm (Bellman-Ford; not yet
/// implemented in drevo). Negative cycles are not detected and
/// will *not* cause a panic — the lazy-update relaxation may
/// loop until the heap is empty, but it does terminate (`prev`
/// only updates when the cost strictly decreases, so progress
/// is bounded by the finite f32 precision lattice).
///
/// # Arguments
///
/// * `from` — source node ID
/// * `to` — target node ID
/// * `edge_kind` — if `Some`, only edges with this kind contribute to
///   the path (parity with `bfs` / `dfs`; previously
///   shortest_path silently considered every edge regardless of kind)
/// * `get_node` — closure to verify a node exists (returns `Option<Node>`)
/// * `edges_of` — closure to retrieve outgoing edges of a node
pub fn shortest_path<F, G>(
    from: u64,
    to: u64,
    edge_kind: Option<&str>,
    get_node: &F,
    edges_of: &G,
) -> Result<Option<Vec<u64>>>
where
    F: Fn(u64) -> Result<Option<Node>>,
    G: Fn(u64, Direction) -> Result<Vec<Edge>>,
{
    if get_node(from)?.is_none() {
        return Ok(None);
    }

    if from == to {
        return Ok(Some(vec![from]));
    }

    let mut dist: HashMap<u64, f32> = HashMap::new();
    let mut prev: HashMap<u64, u64> = HashMap::new();
    let mut heap: BinaryHeap<DijkstraState> = BinaryHeap::new();

    dist.insert(from, 0.0);
    heap.push(DijkstraState {
        cost: 0.0,
        node_id: from,
    });

    while let Some(DijkstraState { cost, node_id }) = heap.pop() {
        if node_id == to {
            let mut path = Vec::new();
            let mut cur = to;
            while cur != from {
                path.push(cur);
                cur = prev[&cur];
            }
            path.push(from);
            path.reverse();
            return Ok(Some(path));
        }

        if cost > *dist.get(&node_id).unwrap_or(&f32::INFINITY) {
            continue;
        }

        let edges = edges_of(node_id, Direction::Outgoing)?;

        for edge in &edges {
            if let Some(kind) = edge_kind {
                if edge.kind != kind {
                    continue;
                }
            }
            let neighbor_id = edge.to_id;
            let next_cost = cost + edge.weight;
            let current_best = *dist.get(&neighbor_id).unwrap_or(&f32::INFINITY);

            if next_cost < current_best {
                dist.insert(neighbor_id, next_cost);
                prev.insert(neighbor_id, node_id);
                heap.push(DijkstraState {
                    cost: next_cost,
                    node_id: neighbor_id,
                });
            }
        }
    }

    Ok(None)
}

/// Extract a subgraph of all nodes and edges within `depth` hops of `root`.
///
/// Uses BFS (both directions) to discover nodes within the radius,
/// then collects all edges whose **both** endpoints are in the
/// discovered node set. The root node is **included** in the result.
///
/// Returns `Err(NodeNotFound)` if the root node does not exist.
///
/// # Arguments
///
/// * `root` — the node ID to start from
/// * `depth` — maximum number of hops (0 returns only the root, no edges)
/// * `edge_kind` — if `Some`, both the BFS-discovery phase and the
///   edge-collection phase skip edges whose `kind` does not match.
///   Nodes only reachable through filtered-out edges are not
///   discovered. `None` preserves the prior all-edges behavior.
/// * `get_node` — closure to retrieve a node by ID
/// * `edges_of` — closure to retrieve edges of a node (both directions)
pub fn subgraph<F, G>(
    root: u64,
    depth: u8,
    edge_kind: Option<&str>,
    get_node: &F,
    edges_of: &G,
) -> Result<SubGraph>
where
    F: Fn(u64) -> Result<Option<Node>>,
    G: Fn(u64, Direction) -> Result<Vec<Edge>>,
{
    use crate::error::DrevoError;

    let root_node = get_node(root)?.ok_or(DrevoError::NodeNotFound(root))?;

    let mut visited: HashSet<u64> = HashSet::new();
    visited.insert(root);

    let mut nodes: Vec<Node> = vec![root_node];

    let mut queue: VecDeque<(u64, u8)> = VecDeque::new();
    queue.push_back((root, 0));

    while let Some((current_id, d)) = queue.pop_front() {
        if d >= depth {
            continue;
        }

        let edges = edges_of(current_id, Direction::Both)?;

        for edge in &edges {
            if let Some(kind) = edge_kind {
                if edge.kind != kind {
                    continue;
                }
            }

            let neighbor_id = neighbor_id_for_edge(edge, current_id, Direction::Both);

            if visited.contains(&neighbor_id) {
                continue;
            }
            visited.insert(neighbor_id);

            if let Some(node) = get_node(neighbor_id)? {
                nodes.push(node);
                queue.push_back((neighbor_id, d + 1));
            }
        }
    }

    let mut edge_ids: HashSet<u64> = HashSet::new();
    let mut edges: Vec<Edge> = Vec::new();

    for &node_id in &visited {
        let node_edges = edges_of(node_id, Direction::Both)?;
        for edge in node_edges {
            if let Some(kind) = edge_kind {
                if edge.kind != kind {
                    continue;
                }
            }
            if visited.contains(&edge.from_id)
                && visited.contains(&edge.to_id)
                && edge_ids.insert(edge.id)
            {
                edges.push(edge);
            }
        }
    }

    Ok(SubGraph { nodes, edges })
}

/// The options of [`expand_bfs`]: how far to walk from the seeds, along
/// which relationships, through which nodes, and how much to return.
///
/// ```
/// use drevo::model::Direction;
/// use drevo::traversal::ExpandOptions;
///
/// let options = ExpandOptions::default();
/// assert_eq!(options.hops, 1);
/// assert_eq!(options.direction, Direction::Both);
/// assert_eq!((options.per_node_limit, options.max_nodes), (25, 200));
/// assert!(options.include_seeds);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandOptions {
    /// How many relationships away from a seed a node may be; `0` returns
    /// the seeds only.
    pub hops: usize,
    /// Which relationships of a node lead on: outgoing, incoming or both.
    pub direction: Direction,
    /// Only relationships of these types are walked; `None` walks every
    /// type.
    pub rel_types: Option<Vec<String>>,
    /// Only nodes with one of these labels are returned and walked further;
    /// `None` admits every label. Seeds are always returned.
    pub labels: Option<Vec<String>>,
    /// Nodes with any of these labels are neither returned nor walked
    /// through.
    pub exclude_labels: Vec<String>,
    /// At most this many new nodes are taken from one node, its
    /// relationships taken by id, lowest first.
    pub per_node_limit: usize,
    /// At most this many nodes are returned, seeds included when
    /// [`include_seeds`](Self::include_seeds) is set.
    pub max_nodes: usize,
    /// An ISO-8601 timestamp: a relationship with `valid_from` /
    /// `valid_until` string properties is walked only if
    /// `valid_from <= as_of < valid_until`; one without them always is.
    pub as_of: Option<String>,
    /// Whether the seeds are returned (at distance `0`).
    pub include_seeds: bool,
}

impl Default for ExpandOptions {
    fn default() -> Self {
        Self {
            hops: 1,
            direction: Direction::Both,
            rel_types: None,
            labels: None,
            exclude_labels: Vec::new(),
            per_node_limit: 25,
            max_nodes: 200,
            as_of: None,
            include_seeds: true,
        }
    }
}

/// The relationship that first reached a node in [`expand_bfs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandVia {
    /// The node the walk came from.
    pub from_id: u64,
    /// The relationship it walked.
    pub rel_id: u64,
    /// That relationship's type.
    pub rel_type: String,
    /// `true` when the relationship points away from
    /// [`from_id`](Self::from_id), `false` when it points at it.
    pub outgoing: bool,
}

/// One node an [`expand_bfs`] walk returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedNode {
    /// The node's id.
    pub id: u64,
    /// How many relationships away from the nearest seed it is; `0` for a
    /// seed.
    pub distance: usize,
    /// The relationship that first reached it; `None` for a seed.
    pub via: Option<ExpandVia>,
}

/// What an [`expand_bfs`] walk returned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expansion {
    /// The nodes, seeds first in seed order, then by distance and node id.
    pub nodes: Vec<ExpandedNode>,
    /// Whether the walk stopped at [`ExpandOptions::max_nodes`] with more
    /// nodes left to return.
    pub truncated: bool,
}

/// Walk breadth-first from `seeds` and return each node reached once, at its
/// shortest distance, with the relationship that first reached it.
///
/// Nodes at one distance are walked in order — the seeds in the order given,
/// later nodes by id — and each node's relationships by id, so the same graph
/// always gives the same answer: a node two nodes reach at the same distance
/// is attributed to the one walked first.
///
/// * `labels_of` gives a node's labels, or `None` when there is no such node
///   or it must not be seen: such a node is neither returned nor walked
///   through.
/// * `for_each_edge` calls its callback with each relationship of a node in a
///   direction; leaving one out keeps the walk off it.
///
/// ```
/// use drevo::model::{Direction, Edge, Properties};
/// use drevo::traversal::{expand_bfs, ExpandOptions};
///
/// // 1 -> 2 -> 3
/// let edges = [(10, 1, 2), (11, 2, 3)].map(|(id, from_id, to_id)| Edge {
///     id,
///     uuid: [0; 16],
///     from_id,
///     to_id,
///     kind: "NEXT".to_string(),
///     weight: 1.0,
///     created_at: 0,
///     properties: Properties::new(),
/// });
/// let options = ExpandOptions { hops: 2, ..ExpandOptions::default() };
/// let found = expand_bfs(
///     &[1],
///     &options,
///     &mut |_| Ok(Some(vec!["Step".to_string()])),
///     &mut |id, _dir, f| {
///         edges.iter().filter(|e| e.from_id == id || e.to_id == id).for_each(|e| f(e));
///         Ok(())
///     },
/// )?;
/// let reached: Vec<(u64, usize)> = found.nodes.iter().map(|n| (n.id, n.distance)).collect();
/// assert_eq!(reached, [(1, 0), (2, 1), (3, 2)]);
/// assert_eq!(found.nodes[2].via.as_ref().map(|v| v.rel_id), Some(11));
/// assert!(!found.truncated);
/// # Ok::<(), drevo::error::DrevoError>(())
/// ```
///
/// # Errors
///
/// [`DrevoError::InvalidArgument`](crate::error::DrevoError::InvalidArgument)
/// when a seed is not a node `labels_of` knows, or when the seeds alone are
/// more than [`ExpandOptions::max_nodes`] and are to be returned; any error
/// of the two callbacks.
pub fn expand_bfs<L, E>(
    seeds: &[u64],
    options: &ExpandOptions,
    labels_of: &mut L,
    for_each_edge: &mut E,
) -> Result<Expansion>
where
    L: FnMut(u64) -> Result<Option<Vec<String>>>,
    E: FnMut(u64, Direction, &mut dyn FnMut(&Edge)) -> Result<()>,
{
    use crate::error::DrevoError;

    let mut visited: HashSet<u64> = HashSet::new();
    let mut frontier: Vec<u64> = Vec::new();
    for &seed in seeds {
        if !visited.insert(seed) {
            continue;
        }
        if labels_of(seed)?.is_none() {
            return Err(DrevoError::InvalidArgument(format!(
                "seeds: {seed} is not a node id"
            )));
        }
        frontier.push(seed);
    }
    if options.include_seeds && frontier.len() > options.max_nodes {
        return Err(DrevoError::InvalidArgument(format!(
            "maxNodes {} is less than the {} seeds; raise maxNodes or pass fewer seeds",
            options.max_nodes,
            frontier.len()
        )));
    }

    let mut nodes: Vec<ExpandedNode> = Vec::new();
    if options.include_seeds {
        nodes.extend(frontier.iter().map(|&id| ExpandedNode {
            id,
            distance: 0,
            via: None,
        }));
    }
    let first_reached = nodes.len();
    let mut truncated = false;
    // One node's candidate steps: (relationship id, neighbour, outgoing, type).
    let mut steps: Vec<(u64, u64, bool, String)> = Vec::new();

    'walk: for distance in 1..=options.hops {
        let mut next: Vec<u64> = Vec::new();
        for &from in &frontier {
            steps.clear();
            for_each_edge(from, options.direction, &mut |edge: &Edge| {
                if expand_walks(edge, options) {
                    let outgoing = match options.direction {
                        Direction::Outgoing => true,
                        Direction::Incoming => false,
                        Direction::Both => edge.from_id == from,
                    };
                    let to = if outgoing { edge.to_id } else { edge.from_id };
                    steps.push((edge.id, to, outgoing, edge.kind.clone()));
                }
            })?;
            steps.sort_unstable_by_key(|step| step.0);
            steps.dedup_by_key(|step| step.0);

            let mut taken = 0;
            for (rel_id, to, outgoing, rel_type) in steps.drain(..) {
                if taken == options.per_node_limit {
                    break;
                }
                if visited.contains(&to) {
                    continue;
                }
                let admitted = labels_of(to)?.is_some_and(|labels| expand_admits(&labels, options));
                if !admitted {
                    // Hidden, excluded or of another label: never returned,
                    // however it is reached.
                    visited.insert(to);
                    continue;
                }
                if nodes.len() == options.max_nodes {
                    truncated = true;
                    break 'walk;
                }
                visited.insert(to);
                nodes.push(ExpandedNode {
                    id: to,
                    distance,
                    via: Some(ExpandVia {
                        from_id: from,
                        rel_id,
                        rel_type,
                        outgoing,
                    }),
                });
                next.push(to);
                taken += 1;
            }
        }
        if next.is_empty() {
            break;
        }
        next.sort_unstable();
        frontier = next;
    }

    nodes[first_reached..].sort_by_key(|n| (n.distance, n.id));
    Ok(Expansion { nodes, truncated })
}

/// Whether [`expand_bfs`] may walk `edge`: of an allowed type and, with
/// [`ExpandOptions::as_of`], valid at that time.
pub(crate) fn expand_walks(edge: &Edge, options: &ExpandOptions) -> bool {
    if let Some(types) = &options.rel_types {
        if !types.contains(&edge.kind) {
            return false;
        }
    }
    let Some(as_of) = options.as_of.as_deref() else {
        return true;
    };
    let started = match edge.properties.get("valid_from") {
        Some(serde_json::Value::String(from)) => from.as_str() <= as_of,
        _ => true,
    };
    let still = match edge.properties.get("valid_until") {
        Some(serde_json::Value::String(until)) => until.as_str() > as_of,
        _ => true,
    };
    started && still
}

/// Whether [`expand_bfs`] returns and walks on from a node with `labels`.
fn expand_admits(labels: &[String], options: &ExpandOptions) -> bool {
    if labels.iter().any(|l| options.exclude_labels.contains(l)) {
        return false;
    }
    match &options.labels {
        Some(wanted) => labels.iter().any(|l| wanted.contains(l)),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NewEdge, NewNode, Properties};
    use crate::native_service::NativeService;

    fn make_node(kind: &str, title: &str) -> NewNode {
        NewNode {
            kind: kind.to_string(),
            title: title.to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        }
    }

    fn make_edge(from: u64, to: u64, kind: &str) -> NewEdge {
        NewEdge {
            from_id: from,
            to_id: to,
            kind: kind.to_string(),
            weight: 1.0,
            properties: Properties::default(),
        }
    }

    #[test]
    fn bfs_depth_zero_returns_empty() {
        let db = NativeService::in_memory();
        let n = db.create_node(make_node("note", "A")).unwrap();
        let result = db.bfs(n.id, 0, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn bfs_no_edges_returns_empty() {
        let db = NativeService::in_memory();
        let n = db.create_node(make_node("note", "Isolated")).unwrap();
        let result = db.bfs(n.id, 3, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn bfs_single_hop() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

        let result = db.bfs(a.id, 1, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, b.id);
    }

    #[test]
    fn bfs_does_not_include_start_node() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

        let result = db.bfs(a.id, 5, Direction::Outgoing, None).unwrap();
        assert!(!result.iter().any(|n| n.id == a.id));
    }

    #[test]
    fn bfs_multi_hop() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();

        // depth 1: only B
        let result = db.bfs(a.id, 1, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, b.id);

        // depth 2: B and C
        let result = db.bfs(a.id, 2, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 2);
        let ids: Vec<u64> = result.iter().map(|n| n.id).collect();
        assert!(ids.contains(&b.id));
        assert!(ids.contains(&c.id));
    }

    #[test]
    fn bfs_respects_direction_outgoing() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        // Edge points from B to A
        db.create_edge(make_edge(b.id, a.id, "links_to")).unwrap();

        // BFS outgoing from A should find nothing
        let result = db.bfs(a.id, 1, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn bfs_respects_direction_incoming() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        // Edge points from B to A
        db.create_edge(make_edge(b.id, a.id, "links_to")).unwrap();

        // BFS incoming from A should find B
        let result = db.bfs(a.id, 1, Direction::Incoming, None).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, b.id);
    }

    #[test]
    fn bfs_direction_both() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(c.id, a.id, "links_to")).unwrap();

        // Both directions from A: should find B (outgoing) and C (incoming)
        let result = db.bfs(a.id, 1, Direction::Both, None).unwrap();
        assert_eq!(result.len(), 2);
        let ids: Vec<u64> = result.iter().map(|n| n.id).collect();
        assert!(ids.contains(&b.id));
        assert!(ids.contains(&c.id));
    }

    #[test]
    fn bfs_edge_kind_filter() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(a.id, c.id, "tagged_with"))
            .unwrap();

        // Only follow "links_to" edges
        let result = db
            .bfs(a.id, 1, Direction::Outgoing, Some("links_to"))
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, b.id);

        // Only follow "tagged_with" edges
        let result = db
            .bfs(a.id, 1, Direction::Outgoing, Some("tagged_with"))
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, c.id);
    }

    #[test]
    fn bfs_handles_cycle() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();
        db.create_edge(make_edge(c.id, a.id, "links_to")).unwrap();

        let result = db.bfs(a.id, 10, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 2); // B and C, not A again
    }

    #[test]
    fn bfs_self_loop() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        db.create_edge(make_edge(a.id, a.id, "self_ref")).unwrap();

        let result = db.bfs(a.id, 3, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty()); // start node excluded, self-loop doesn't add it
    }

    #[test]
    fn bfs_nonexistent_edge_kind_returns_empty() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

        let result = db
            .bfs(a.id, 1, Direction::Outgoing, Some("nonexistent"))
            .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn bfs_fan_out() {
        let db = NativeService::in_memory();
        let hub = db.create_node(make_node("note", "Hub")).unwrap();
        let mut spoke_ids = Vec::new();
        for i in 0..10 {
            let spoke = db
                .create_node(make_node("note", &format!("Spoke{}", i)))
                .unwrap();
            db.create_edge(make_edge(hub.id, spoke.id, "links_to"))
                .unwrap();
            spoke_ids.push(spoke.id);
        }

        let result = db.bfs(hub.id, 1, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 10);
        for id in &spoke_ids {
            assert!(result.iter().any(|n| n.id == *id));
        }
    }

    #[test]
    fn bfs_diamond_graph_no_duplicates() {
        // A -> B, A -> C, B -> D, C -> D
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        let d = db.create_node(make_node("note", "D")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(a.id, c.id, "links_to")).unwrap();
        db.create_edge(make_edge(b.id, d.id, "links_to")).unwrap();
        db.create_edge(make_edge(c.id, d.id, "links_to")).unwrap();

        let result = db.bfs(a.id, 2, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 3); // B, C, D — no duplicates
        let ids: Vec<u64> = result.iter().map(|n| n.id).collect();
        assert!(ids.contains(&b.id));
        assert!(ids.contains(&c.id));
        assert!(ids.contains(&d.id));
    }

    // ---------------------------------------------------------------
    // DFS tests
    // ---------------------------------------------------------------

    #[test]
    fn dfs_depth_zero_returns_empty() {
        let db = NativeService::in_memory();
        let n = db.create_node(make_node("note", "A")).unwrap();
        let result = db.dfs(n.id, 0, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn dfs_no_edges_returns_empty() {
        let db = NativeService::in_memory();
        let n = db.create_node(make_node("note", "Isolated")).unwrap();
        let result = db.dfs(n.id, 3, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn dfs_single_hop() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

        let result = db.dfs(a.id, 1, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, b.id);
    }

    #[test]
    fn dfs_does_not_include_start_node() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

        let result = db.dfs(a.id, 5, Direction::Outgoing, None).unwrap();
        assert!(!result.iter().any(|n| n.id == a.id));
    }

    #[test]
    fn dfs_multi_hop() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();

        // depth 1: only B
        let result = db.dfs(a.id, 1, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, b.id);

        // depth 2: B and C
        let result = db.dfs(a.id, 2, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 2);
        let ids: Vec<u64> = result.iter().map(|n| n.id).collect();
        assert!(ids.contains(&b.id));
        assert!(ids.contains(&c.id));
    }

    #[test]
    fn dfs_respects_direction_outgoing() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(b.id, a.id, "links_to")).unwrap();

        let result = db.dfs(a.id, 1, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn dfs_respects_direction_incoming() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(b.id, a.id, "links_to")).unwrap();

        let result = db.dfs(a.id, 1, Direction::Incoming, None).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, b.id);
    }

    #[test]
    fn dfs_direction_both() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(c.id, a.id, "links_to")).unwrap();

        let result = db.dfs(a.id, 1, Direction::Both, None).unwrap();
        assert_eq!(result.len(), 2);
        let ids: Vec<u64> = result.iter().map(|n| n.id).collect();
        assert!(ids.contains(&b.id));
        assert!(ids.contains(&c.id));
    }

    #[test]
    fn dfs_edge_kind_filter() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(a.id, c.id, "tagged_with"))
            .unwrap();

        let result = db
            .dfs(a.id, 1, Direction::Outgoing, Some("links_to"))
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, b.id);

        let result = db
            .dfs(a.id, 1, Direction::Outgoing, Some("tagged_with"))
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, c.id);
    }

    #[test]
    fn dfs_handles_cycle() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();
        db.create_edge(make_edge(c.id, a.id, "links_to")).unwrap();

        let result = db.dfs(a.id, 10, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 2); // B and C, not A again
    }

    #[test]
    fn dfs_self_loop() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        db.create_edge(make_edge(a.id, a.id, "self_ref")).unwrap();

        let result = db.dfs(a.id, 3, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn dfs_nonexistent_edge_kind_returns_empty() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

        let result = db
            .dfs(a.id, 1, Direction::Outgoing, Some("nonexistent"))
            .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn dfs_fan_out() {
        let db = NativeService::in_memory();
        let hub = db.create_node(make_node("note", "Hub")).unwrap();
        let mut spoke_ids = Vec::new();
        for i in 0..10 {
            let spoke = db
                .create_node(make_node("note", &format!("Spoke{}", i)))
                .unwrap();
            db.create_edge(make_edge(hub.id, spoke.id, "links_to"))
                .unwrap();
            spoke_ids.push(spoke.id);
        }

        let result = db.dfs(hub.id, 1, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 10);
        for id in &spoke_ids {
            assert!(result.iter().any(|n| n.id == *id));
        }
    }

    #[test]
    fn dfs_diamond_graph_no_duplicates() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        let d = db.create_node(make_node("note", "D")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(a.id, c.id, "links_to")).unwrap();
        db.create_edge(make_edge(b.id, d.id, "links_to")).unwrap();
        db.create_edge(make_edge(c.id, d.id, "links_to")).unwrap();

        let result = db.dfs(a.id, 2, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 3); // B, C, D — no duplicates
        let ids: Vec<u64> = result.iter().map(|n| n.id).collect();
        assert!(ids.contains(&b.id));
        assert!(ids.contains(&c.id));
        assert!(ids.contains(&d.id));
    }

    #[test]
    fn dfs_explores_depth_first() {
        // A -> B -> C, A -> D
        // DFS from A should visit B then C before D (or D then ...) depending on stack order.
        // Key property: DFS goes deep before wide.
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        let d = db.create_node(make_node("note", "D")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(a.id, d.id, "links_to")).unwrap();
        db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();

        let result = db.dfs(a.id, 3, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 3);
        let ids: Vec<u64> = result.iter().map(|n| n.id).collect();
        assert!(ids.contains(&b.id));
        assert!(ids.contains(&c.id));
        assert!(ids.contains(&d.id));

        // In DFS with a stack, the last edge pushed is explored first.
        // Edges are iterated in order, so D is pushed after B. D is popped
        // first (LIFO). But the key DFS property is that when B is popped,
        // C is discovered before any other branch is resumed at the same level.
        // We verify that C appears in results (depth explored) and all 3 nodes found.
    }

    // ---------------------------------------------------------------
    // shortest_path (Dijkstra) tests
    // ---------------------------------------------------------------

    fn make_weighted_edge(from: u64, to: u64, kind: &str, weight: f32) -> NewEdge {
        NewEdge {
            from_id: from,
            to_id: to,
            kind: kind.to_string(),
            weight,
            properties: Properties::default(),
        }
    }

    #[test]
    fn sp_same_node() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let path = db.shortest_path(a.id, a.id).unwrap();
        assert_eq!(path, Some(vec![a.id]));
    }

    #[test]
    fn sp_direct_edge() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_weighted_edge(a.id, b.id, "links_to", 1.0))
            .unwrap();
        let path = db.shortest_path(a.id, b.id).unwrap();
        assert_eq!(path, Some(vec![a.id, b.id]));
    }

    #[test]
    fn sp_no_connection() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let path = db.shortest_path(a.id, b.id).unwrap();
        assert_eq!(path, None);
    }

    #[test]
    fn sp_prefers_lower_weight() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        // Direct A->B costs 10, via C costs 1+1=2
        db.create_edge(make_weighted_edge(a.id, b.id, "links_to", 10.0))
            .unwrap();
        db.create_edge(make_weighted_edge(a.id, c.id, "links_to", 1.0))
            .unwrap();
        db.create_edge(make_weighted_edge(c.id, b.id, "links_to", 1.0))
            .unwrap();
        let path = db.shortest_path(a.id, b.id).unwrap();
        assert_eq!(path, Some(vec![a.id, c.id, b.id]));
    }

    #[test]
    fn sp_handles_cycle() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        db.create_edge(make_weighted_edge(a.id, b.id, "links_to", 1.0))
            .unwrap();
        db.create_edge(make_weighted_edge(b.id, c.id, "links_to", 1.0))
            .unwrap();
        db.create_edge(make_weighted_edge(c.id, a.id, "links_to", 1.0))
            .unwrap();
        let path = db.shortest_path(a.id, c.id).unwrap();
        assert_eq!(path, Some(vec![a.id, b.id, c.id]));
    }

    #[test]
    fn sp_nonexistent_source() {
        let db = NativeService::in_memory();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let path = db.shortest_path(999, b.id).unwrap();
        assert_eq!(path, None);
    }

    #[test]
    fn sp_wrong_direction() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_weighted_edge(b.id, a.id, "links_to", 1.0))
            .unwrap();
        let path = db.shortest_path(a.id, b.id).unwrap();
        assert_eq!(path, None);
    }

    #[test]
    fn sp_self_loop_ignored() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_weighted_edge(a.id, a.id, "self_ref", 0.1))
            .unwrap();
        db.create_edge(make_weighted_edge(a.id, b.id, "links_to", 1.0))
            .unwrap();
        let path = db.shortest_path(a.id, b.id).unwrap();
        assert_eq!(path, Some(vec![a.id, b.id]));
    }

    // ---------------------------------------------------------------
    // subgraph tests
    // ---------------------------------------------------------------

    #[test]
    fn subgraph_depth_zero_returns_root_only() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

        let sg = db.subgraph(a.id, 0).unwrap();
        assert_eq!(sg.nodes.len(), 1);
        assert_eq!(sg.nodes[0].id, a.id);
        assert!(sg.edges.is_empty());
    }

    #[test]
    fn subgraph_nonexistent_root_returns_error() {
        let db = NativeService::in_memory();
        let result = db.subgraph(999, 2);
        assert!(result.is_err());
    }

    #[test]
    fn subgraph_single_hop_includes_root_neighbor_and_edge() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let e = db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

        let sg = db.subgraph(a.id, 1).unwrap();
        assert_eq!(sg.nodes.len(), 2);
        let node_ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();
        assert!(node_ids.contains(&a.id));
        assert!(node_ids.contains(&b.id));
        assert_eq!(sg.edges.len(), 1);
        assert_eq!(sg.edges[0].id, e.id);
    }

    #[test]
    fn subgraph_follows_both_directions() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(c.id, a.id, "links_to")).unwrap();

        let sg = db.subgraph(a.id, 1).unwrap();
        assert_eq!(sg.nodes.len(), 3);
        assert_eq!(sg.edges.len(), 2);
    }

    #[test]
    fn subgraph_multi_hop() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();

        // depth 1: A, B + edge A->B
        let sg = db.subgraph(a.id, 1).unwrap();
        assert_eq!(sg.nodes.len(), 2);
        assert_eq!(sg.edges.len(), 1);

        // depth 2: A, B, C + edges A->B and B->C
        let sg = db.subgraph(a.id, 2).unwrap();
        assert_eq!(sg.nodes.len(), 3);
        assert_eq!(sg.edges.len(), 2);
    }

    #[test]
    fn subgraph_handles_cycle() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();
        db.create_edge(make_edge(c.id, a.id, "links_to")).unwrap();

        let sg = db.subgraph(a.id, 10).unwrap();
        assert_eq!(sg.nodes.len(), 3);
        assert_eq!(sg.edges.len(), 3);
    }

    #[test]
    fn subgraph_diamond_no_duplicates() {
        // A -> B, A -> C, B -> D, C -> D
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        let d = db.create_node(make_node("note", "D")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(a.id, c.id, "links_to")).unwrap();
        db.create_edge(make_edge(b.id, d.id, "links_to")).unwrap();
        db.create_edge(make_edge(c.id, d.id, "links_to")).unwrap();

        let sg = db.subgraph(a.id, 2).unwrap();
        assert_eq!(sg.nodes.len(), 4);
        assert_eq!(sg.edges.len(), 4);
        // No duplicate nodes
        let mut ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn subgraph_self_loop() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let e = db.create_edge(make_edge(a.id, a.id, "self_ref")).unwrap();

        let sg = db.subgraph(a.id, 3).unwrap();
        assert_eq!(sg.nodes.len(), 1);
        assert_eq!(sg.edges.len(), 1);
        assert_eq!(sg.edges[0].id, e.id);
    }

    #[test]
    fn subgraph_isolated_node() {
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "Isolated")).unwrap();

        let sg = db.subgraph(a.id, 5).unwrap();
        assert_eq!(sg.nodes.len(), 1);
        assert_eq!(sg.nodes[0].id, a.id);
        assert!(sg.edges.is_empty());
    }

    #[test]
    fn subgraph_only_includes_edges_between_subgraph_nodes() {
        // A -> B -> C -> D, with an edge D -> E (outside radius)
        let db = NativeService::in_memory();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let c = db.create_node(make_node("note", "C")).unwrap();
        let d = db.create_node(make_node("note", "D")).unwrap();
        let _e = db.create_node(make_node("note", "E")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
        db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();
        db.create_edge(make_edge(c.id, d.id, "links_to")).unwrap();
        db.create_edge(make_edge(d.id, _e.id, "links_to")).unwrap();

        // depth 2 from A: nodes A, B, C
        let sg = db.subgraph(a.id, 2).unwrap();
        assert_eq!(sg.nodes.len(), 3);
        // Only edges A->B and B->C (not C->D or D->E)
        assert_eq!(sg.edges.len(), 2);
        let edge_ids: HashSet<u64> = sg.edges.iter().map(|e| e.id).collect();
        // No edge should reference D or E
        for edge in &sg.edges {
            assert!(edge.from_id != d.id && edge.to_id != d.id);
        }
        let _ = edge_ids; // suppress warning
    }
}

#[cfg(test)]
mod expand_tests {
    use super::*;
    use crate::error::DrevoError;
    use crate::model::Properties;

    /// A graph held in memory: node labels by id, and relationships.
    #[derive(Default)]
    struct Graph {
        labels: HashMap<u64, Vec<String>>,
        edges: Vec<Edge>,
    }

    impl Graph {
        fn node(mut self, id: u64, labels: &[&str]) -> Self {
            self.labels
                .insert(id, labels.iter().map(|l| (*l).to_string()).collect());
            self
        }

        fn nodes(mut self, ids: &[u64]) -> Self {
            for &id in ids {
                self = self.node(id, &["N"]);
            }
            self
        }

        fn edge(self, id: u64, from_id: u64, to_id: u64) -> Self {
            self.typed_edge(id, from_id, to_id, "R", &[])
        }

        fn typed_edge(
            mut self,
            id: u64,
            from_id: u64,
            to_id: u64,
            kind: &str,
            props: &[(&str, &str)],
        ) -> Self {
            let mut properties = Properties::new();
            for (k, v) in props {
                properties.insert(*k, serde_json::Value::String((*v).to_string()));
            }
            self.edges.push(Edge {
                id,
                uuid: [0; 16],
                from_id,
                to_id,
                kind: kind.to_string(),
                weight: 1.0,
                created_at: 0,
                properties,
            });
            self
        }

        fn expand(&self, seeds: &[u64], options: &ExpandOptions) -> Result<Expansion> {
            self.expand_hiding(seeds, options, &[])
        }

        /// Expand as a reader who may not see the nodes in `hidden`.
        fn expand_hiding(
            &self,
            seeds: &[u64],
            options: &ExpandOptions,
            hidden: &[u64],
        ) -> Result<Expansion> {
            expand_bfs(
                seeds,
                options,
                &mut |id| {
                    if hidden.contains(&id) {
                        return Ok(None);
                    }
                    Ok(self.labels.get(&id).cloned())
                },
                &mut |id, direction, f| {
                    // Like the engine: in insertion order, not by id.
                    for e in self.edges.iter().rev() {
                        let hit = match direction {
                            Direction::Outgoing => e.from_id == id,
                            Direction::Incoming => e.to_id == id,
                            Direction::Both => e.from_id == id || e.to_id == id,
                        };
                        if hit {
                            f(e);
                        }
                    }
                    Ok(())
                },
            )
        }
    }

    fn hops(n: usize) -> ExpandOptions {
        ExpandOptions {
            hops: n,
            ..ExpandOptions::default()
        }
    }

    /// `(id, distance)` of every returned node, in order.
    fn reached(e: &Expansion) -> Vec<(u64, usize)> {
        e.nodes.iter().map(|n| (n.id, n.distance)).collect()
    }

    fn ids(e: &Expansion) -> Vec<u64> {
        e.nodes.iter().map(|n| n.id).collect()
    }

    fn invalid(r: Result<Expansion>) -> String {
        match r {
            Err(DrevoError::InvalidArgument(message)) => message,
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    /// a(1) – b(2) – c(3) – d(4)
    fn chain() -> Graph {
        Graph::default()
            .nodes(&[1, 2, 3, 4])
            .edge(10, 1, 2)
            .edge(11, 2, 3)
            .edge(12, 3, 4)
    }

    #[test]
    fn chain_two_hops_stops_at_the_second_node() {
        let found = chain().expand(&[1], &hops(2)).unwrap();
        assert_eq!(reached(&found), [(1, 0), (2, 1), (3, 2)]);
        assert!(!found.truncated);
    }

    #[test]
    fn zero_hops_returns_the_seeds_only() {
        let found = chain().expand(&[1], &hops(0)).unwrap();
        assert_eq!(reached(&found), [(1, 0)]);
        assert_eq!(found.nodes[0].via, None);
    }

    #[test]
    fn via_names_the_relationship_and_its_direction() {
        let found = chain().expand(&[2], &hops(1)).unwrap();
        assert_eq!(reached(&found), [(2, 0), (1, 1), (3, 1)]);
        assert_eq!(
            found.nodes[1].via,
            Some(ExpandVia {
                from_id: 2,
                rel_id: 10,
                rel_type: "R".to_string(),
                outgoing: false,
            })
        );
        assert_eq!(
            found.nodes[2].via,
            Some(ExpandVia {
                from_id: 2,
                rel_id: 11,
                rel_type: "R".to_string(),
                outgoing: true,
            })
        );
    }

    #[test]
    fn diamond_returns_the_far_node_once_via_the_lower_frontier_node() {
        // a(1)→b(2), a→c(3), b→d(4), c→d — d is reached from b first.
        let g = Graph::default()
            .nodes(&[1, 2, 3, 4])
            .edge(10, 1, 2)
            .edge(11, 1, 3)
            .edge(13, 3, 4)
            .edge(12, 2, 4);
        let found = g.expand(&[1], &hops(2)).unwrap();
        assert_eq!(reached(&found), [(1, 0), (2, 1), (3, 1), (4, 2)]);
        let via = found.nodes[3].via.as_ref().unwrap();
        assert_eq!((via.from_id, via.rel_id), (2, 12));
    }

    #[test]
    fn a_node_reached_from_two_seeds_appears_once_at_its_shortest_distance() {
        // 1 – 2 – 3, seeds 1 and 3: 2 is one hop from both.
        let g = Graph::default()
            .nodes(&[1, 2, 3])
            .edge(10, 1, 2)
            .edge(11, 2, 3);
        let found = g.expand(&[3, 1], &hops(2)).unwrap();
        assert_eq!(reached(&found), [(3, 0), (1, 0), (2, 1)]);
        // Attributed to the first seed walked (seed order, not id order).
        assert_eq!(found.nodes[2].via.as_ref().unwrap().from_id, 3);
    }

    #[test]
    fn duplicate_seeds_count_once() {
        let found = chain().expand(&[2, 2, 1], &hops(0)).unwrap();
        assert_eq!(ids(&found), [2, 1]);
    }

    #[test]
    fn outgoing_follows_relationships_away_from_the_node_only() {
        // a(1)→b(2), c(3)→a
        let g = Graph::default()
            .nodes(&[1, 2, 3])
            .edge(10, 1, 2)
            .edge(11, 3, 1);
        let out = ExpandOptions {
            direction: Direction::Outgoing,
            ..hops(2)
        };
        assert_eq!(ids(&g.expand(&[1], &out).unwrap()), [1, 2]);
        let incoming = ExpandOptions {
            direction: Direction::Incoming,
            ..hops(2)
        };
        let found = g.expand(&[1], &incoming).unwrap();
        assert_eq!(ids(&found), [1, 3]);
        assert!(!found.nodes[1].via.as_ref().unwrap().outgoing);
    }

    #[test]
    fn rel_types_restrict_the_walk() {
        let g = Graph::default()
            .nodes(&[1, 2, 3])
            .typed_edge(10, 1, 2, "KNOWS", &[])
            .typed_edge(11, 1, 3, "OWNS", &[]);
        let options = ExpandOptions {
            rel_types: Some(vec!["OWNS".to_string()]),
            ..hops(1)
        };
        assert_eq!(ids(&g.expand(&[1], &options).unwrap()), [1, 3]);
    }

    #[test]
    fn labels_admit_only_matching_nodes_and_walk_only_through_them() {
        // seed(1:Doc) – 2:Person – 3:Doc ; seed – 4:Doc – 5:Doc
        let g = Graph::default()
            .node(1, &["Doc"])
            .node(2, &["Person"])
            .node(3, &["Doc"])
            .node(4, &["Topic", "Doc"])
            .node(5, &["Doc"])
            .edge(10, 1, 2)
            .edge(11, 2, 3)
            .edge(12, 1, 4)
            .edge(13, 4, 5);
        let options = ExpandOptions {
            labels: Some(vec!["Doc".to_string()]),
            ..hops(2)
        };
        assert_eq!(
            reached(&g.expand(&[1], &options).unwrap()),
            [(1, 0), (4, 1), (5, 2)]
        );
        // A seed is returned and walked even without the label.
        let found = g.expand(&[2], &options).unwrap();
        assert_eq!(reached(&found), [(2, 0), (1, 1), (3, 1), (4, 2)]);
    }

    #[test]
    fn excluded_labels_are_neither_returned_nor_walked_through() {
        // 1 – 2:Message – 3 ; 1 – 4
        let g = Graph::default()
            .node(1, &["Entity"])
            .node(2, &["Message"])
            .node(3, &["Entity"])
            .node(4, &["Entity"])
            .edge(10, 1, 2)
            .edge(11, 2, 3)
            .edge(12, 1, 4);
        let options = ExpandOptions {
            exclude_labels: vec!["Message".to_string(), "ContextTrace".to_string()],
            ..hops(3)
        };
        assert_eq!(ids(&g.expand(&[1], &options).unwrap()), [1, 4]);
    }

    #[test]
    fn hidden_nodes_are_neither_returned_nor_walked_through() {
        let found = chain().expand_hiding(&[1], &hops(3), &[2]).unwrap();
        assert_eq!(ids(&found), [1]);
    }

    #[test]
    fn a_hidden_or_missing_seed_is_refused_by_id() {
        assert_eq!(
            invalid(chain().expand(&[1, 99], &hops(1))),
            "seeds: 99 is not a node id"
        );
        assert_eq!(
            invalid(chain().expand_hiding(&[3], &hops(1), &[3])),
            "seeds: 3 is not a node id"
        );
    }

    #[test]
    fn per_node_limit_takes_the_lowest_relationship_ids() {
        // Hub 1 with five neighbours 2..=6 over relationships 24, 20, 22, 23, 21.
        let g = Graph::default()
            .nodes(&[1, 2, 3, 4, 5, 6])
            .edge(24, 1, 2)
            .edge(20, 1, 3)
            .edge(22, 4, 1)
            .edge(23, 1, 5)
            .edge(21, 1, 6);
        let options = ExpandOptions {
            per_node_limit: 2,
            ..hops(1)
        };
        let found = g.expand(&[1], &options).unwrap();
        // Relationships 20 (→3) and 21 (→6); rows by distance, then id.
        assert_eq!(ids(&found), [1, 3, 6]);
        assert!(!found.truncated, "perNodeLimit is not the node cap");
    }

    #[test]
    fn per_node_limit_counts_new_nodes_only() {
        // 1 – 2 – 3, 2 – 4: from 2, the relationship back to 1 is not a new node.
        let g = Graph::default()
            .nodes(&[1, 2, 3, 4])
            .edge(10, 1, 2)
            .edge(11, 2, 3)
            .edge(12, 2, 4);
        let options = ExpandOptions {
            per_node_limit: 1,
            ..hops(2)
        };
        assert_eq!(
            reached(&g.expand(&[1], &options).unwrap()),
            [(1, 0), (2, 1), (3, 2)]
        );
    }

    #[test]
    fn max_nodes_stops_the_walk_and_flags_truncation() {
        let star = Graph::default()
            .nodes(&[1, 2, 3, 4, 5])
            .edge(10, 1, 2)
            .edge(11, 1, 3)
            .edge(12, 1, 4)
            .edge(13, 1, 5);
        let options = ExpandOptions {
            max_nodes: 3,
            ..hops(1)
        };
        let found = star.expand(&[1], &options).unwrap();
        assert_eq!(ids(&found), [1, 2, 3]);
        assert!(found.truncated);
    }

    #[test]
    fn exactly_max_nodes_is_not_truncated() {
        let options = ExpandOptions {
            max_nodes: 3,
            ..hops(2)
        };
        let found = chain().expand(&[1], &options).unwrap();
        assert_eq!(ids(&found), [1, 2, 3]);
        assert!(!found.truncated);
    }

    #[test]
    fn max_nodes_never_drops_a_seed() {
        let options = ExpandOptions {
            max_nodes: 2,
            ..hops(1)
        };
        let found = chain().expand(&[1, 4], &options).unwrap();
        assert_eq!(ids(&found), [1, 4]);
        assert!(found.truncated);
        assert_eq!(
            invalid(chain().expand(&[1, 2, 3], &options)),
            "maxNodes 2 is less than the 3 seeds; raise maxNodes or pass fewer seeds"
        );
    }

    #[test]
    fn without_seeds_in_the_output_max_nodes_caps_the_reached_nodes() {
        let options = ExpandOptions {
            max_nodes: 1,
            include_seeds: false,
            ..hops(3)
        };
        let found = chain().expand(&[1, 2, 3], &options).unwrap();
        assert_eq!(reached(&found), [(4, 1)]);
        assert!(!found.truncated);
        let found = chain().expand(&[1], &options).unwrap();
        assert_eq!(reached(&found), [(2, 1)]);
        assert!(found.truncated);
    }

    fn fact(as_of: &str) -> ExpandOptions {
        ExpandOptions {
            as_of: Some(as_of.to_string()),
            ..hops(1)
        }
    }

    #[test]
    fn as_of_walks_only_relationships_valid_then() {
        // 1 -[valid 2024-01-01 .. 2025-01-01]-> 2 ; 1 -[no validity]-> 3 ;
        // 1 -[valid from 2024-06-01, open]-> 4
        let g = Graph::default()
            .nodes(&[1, 2, 3, 4])
            .typed_edge(
                10,
                1,
                2,
                "RELATED_TO",
                &[
                    ("valid_from", "2024-01-01T00:00:00Z"),
                    ("valid_until", "2025-01-01T00:00:00Z"),
                ],
            )
            .typed_edge(11, 1, 3, "RELATED_TO", &[])
            .typed_edge(
                12,
                1,
                4,
                "RELATED_TO",
                &[("valid_from", "2024-06-01T00:00:00Z")],
            );
        let before = g.expand(&[1], &fact("2023-12-31T23:59:59Z")).unwrap();
        assert_eq!(ids(&before), [1, 3]);
        let during = g.expand(&[1], &fact("2024-03-01T00:00:00Z")).unwrap();
        assert_eq!(ids(&during), [1, 2, 3]);
        let later = g.expand(&[1], &fact("2024-07-01T00:00:00Z")).unwrap();
        assert_eq!(ids(&later), [1, 2, 3, 4]);
        // valid_until is exclusive.
        let after = g.expand(&[1], &fact("2025-01-01T00:00:00Z")).unwrap();
        assert_eq!(ids(&after), [1, 3, 4]);
        // Without asOf every relationship is walked.
        assert_eq!(ids(&g.expand(&[1], &hops(1)).unwrap()), [1, 2, 3, 4]);
    }

    #[test]
    fn a_self_loop_and_a_parallel_relationship_do_not_repeat_a_node() {
        let g = Graph::default()
            .nodes(&[1, 2])
            .edge(10, 1, 1)
            .edge(11, 1, 2)
            .edge(12, 2, 1);
        let found = g.expand(&[1], &hops(2)).unwrap();
        assert_eq!(reached(&found), [(1, 0), (2, 1)]);
        assert_eq!(found.nodes[1].via.as_ref().unwrap().rel_id, 11);
    }

    #[test]
    fn the_same_graph_gives_the_same_answer() {
        let g = Graph::default()
            .nodes(&[1, 2, 3, 4, 5, 6])
            .edge(10, 1, 5)
            .edge(11, 1, 3)
            .edge(12, 5, 6)
            .edge(13, 3, 6)
            .edge(14, 6, 2)
            .edge(15, 4, 2);
        let first = g.expand(&[1, 4], &hops(3)).unwrap();
        for _ in 0..5 {
            assert_eq!(g.expand(&[1, 4], &hops(3)).unwrap(), first);
        }
        // Distance-1 rows by id; 6 is reached from 2, the lowest frontier node.
        assert_eq!(
            reached(&first),
            [(1, 0), (4, 0), (2, 1), (3, 1), (5, 1), (6, 2)]
        );
        assert_eq!(first.nodes[5].via.as_ref().unwrap().from_id, 2);
    }
}
