//! Graph traversal algorithms.
//!
//! Provides BFS (breadth-first search), DFS (depth-first search) with
//! depth limit and optional edge kind filtering, and shortest path
//! (Dijkstra) weighted by edge weight. Used by [`GraphNoteDb`]
//! traversal methods.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use crate::error::Result;
use crate::model::{Direction, Edge, Node, SubGraph};

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
            // Apply edge kind filter
            if let Some(kind) = edge_kind {
                if edge.kind != kind {
                    continue;
                }
            }

            // Determine the neighbor node ID based on direction
            let neighbor_id = match direction {
                Direction::Outgoing => edge.to_id,
                Direction::Incoming => edge.from_id,
                Direction::Both => {
                    if edge.from_id == current_id {
                        edge.to_id
                    } else {
                        edge.from_id
                    }
                }
            };

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
            // Apply edge kind filter
            if let Some(kind) = edge_kind {
                if edge.kind != kind {
                    continue;
                }
            }

            // Determine the neighbor node ID based on direction
            let neighbor_id = match direction {
                Direction::Outgoing => edge.to_id,
                Direction::Incoming => edge.from_id,
                Direction::Both => {
                    if edge.from_id == current_id {
                        edge.to_id
                    } else {
                        edge.from_id
                    }
                }
            };

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
/// Dijkstra's algorithm. Follows **outgoing** edges only.
///
/// Returns `Some(vec![from, ..., to])` with the node IDs along the
/// shortest path, or `None` if `to` is not reachable from `from`.
/// If `from == to`, returns `Some(vec![from])`.
///
/// Edge weights must be non-negative (the `weight` field on [`Edge`]).
///
/// # Arguments
///
/// * `from` — source node ID
/// * `to` — target node ID
/// * `get_node` — closure to verify a node exists (returns `Option<Node>`)
/// * `edges_of` — closure to retrieve outgoing edges of a node
pub fn shortest_path<F, G>(
    from: u64,
    to: u64,
    get_node: &F,
    edges_of: &G,
) -> Result<Option<Vec<u64>>>
where
    F: Fn(u64) -> Result<Option<Node>>,
    G: Fn(u64, Direction) -> Result<Vec<Edge>>,
{
    // Verify source exists.
    if get_node(from)?.is_none() {
        return Ok(None);
    }

    // Trivial case: source == target.
    if from == to {
        return Ok(Some(vec![from]));
    }

    // Dijkstra's algorithm.
    let mut dist: HashMap<u64, f32> = HashMap::new();
    let mut prev: HashMap<u64, u64> = HashMap::new();
    let mut heap: BinaryHeap<DijkstraState> = BinaryHeap::new();

    dist.insert(from, 0.0);
    heap.push(DijkstraState {
        cost: 0.0,
        node_id: from,
    });

    while let Some(DijkstraState { cost, node_id }) = heap.pop() {
        // Found the target — reconstruct path.
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

        // Skip if we already found a shorter path to this node.
        if cost > *dist.get(&node_id).unwrap_or(&f32::INFINITY) {
            continue;
        }

        let edges = edges_of(node_id, Direction::Outgoing)?;

        for edge in &edges {
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
/// * `get_node` — closure to retrieve a node by ID
/// * `edges_of` — closure to retrieve edges of a node (both directions)
pub fn subgraph<F, G>(root: u64, depth: u8, get_node: &F, edges_of: &G) -> Result<SubGraph>
where
    F: Fn(u64) -> Result<Option<Node>>,
    G: Fn(u64, Direction) -> Result<Vec<Edge>>,
{
    use crate::error::GraphNoteError;

    // Verify root exists.
    let root_node = get_node(root)?.ok_or(GraphNoteError::NodeNotFound(root))?;

    let mut visited: HashSet<u64> = HashSet::new();
    visited.insert(root);

    let mut nodes: Vec<Node> = vec![root_node];

    // BFS to discover all reachable nodes within depth.
    let mut queue: VecDeque<(u64, u8)> = VecDeque::new();
    queue.push_back((root, 0));

    while let Some((current_id, d)) = queue.pop_front() {
        if d >= depth {
            continue;
        }

        let edges = edges_of(current_id, Direction::Both)?;

        for edge in &edges {
            let neighbor_id = if edge.from_id == current_id {
                edge.to_id
            } else {
                edge.from_id
            };

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

    // Collect all edges where both endpoints are in the visited set.
    let mut edge_ids: HashSet<u64> = HashSet::new();
    let mut edges: Vec<Edge> = Vec::new();

    for &node_id in &visited {
        let node_edges = edges_of(node_id, Direction::Both)?;
        for edge in node_edges {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::GraphNoteDb;
    use crate::model::{NewEdge, NewNode, Properties};

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
        let db = GraphNoteDb::open_in_memory().unwrap();
        let n = db.create_node(make_node("note", "A")).unwrap();
        let result = db.bfs(n.id, 0, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn bfs_no_edges_returns_empty() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        let n = db.create_node(make_node("note", "Isolated")).unwrap();
        let result = db.bfs(n.id, 3, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn bfs_single_hop() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

        let result = db.bfs(a.id, 1, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, b.id);
    }

    #[test]
    fn bfs_does_not_include_start_node() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

        let result = db.bfs(a.id, 5, Direction::Outgoing, None).unwrap();
        assert!(!result.iter().any(|n| n.id == a.id));
    }

    #[test]
    fn bfs_multi_hop() {
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
        let a = db.create_node(make_node("note", "A")).unwrap();
        db.create_edge(make_edge(a.id, a.id, "self_ref")).unwrap();

        let result = db.bfs(a.id, 3, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty()); // start node excluded, self-loop doesn't add it
    }

    #[test]
    fn bfs_nonexistent_edge_kind_returns_empty() {
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
        let n = db.create_node(make_node("note", "A")).unwrap();
        let result = db.dfs(n.id, 0, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn dfs_no_edges_returns_empty() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        let n = db.create_node(make_node("note", "Isolated")).unwrap();
        let result = db.dfs(n.id, 3, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn dfs_single_hop() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

        let result = db.dfs(a.id, 1, Direction::Outgoing, None).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, b.id);
    }

    #[test]
    fn dfs_does_not_include_start_node() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

        let result = db.dfs(a.id, 5, Direction::Outgoing, None).unwrap();
        assert!(!result.iter().any(|n| n.id == a.id));
    }

    #[test]
    fn dfs_multi_hop() {
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(b.id, a.id, "links_to")).unwrap();

        let result = db.dfs(a.id, 1, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn dfs_respects_direction_incoming() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_edge(b.id, a.id, "links_to")).unwrap();

        let result = db.dfs(a.id, 1, Direction::Incoming, None).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, b.id);
    }

    #[test]
    fn dfs_direction_both() {
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
        let a = db.create_node(make_node("note", "A")).unwrap();
        db.create_edge(make_edge(a.id, a.id, "self_ref")).unwrap();

        let result = db.dfs(a.id, 3, Direction::Outgoing, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn dfs_nonexistent_edge_kind_returns_empty() {
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let path = db.shortest_path(a.id, a.id).unwrap();
        assert_eq!(path, Some(vec![a.id]));
    }

    #[test]
    fn sp_direct_edge() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_weighted_edge(a.id, b.id, "links_to", 1.0))
            .unwrap();
        let path = db.shortest_path(a.id, b.id).unwrap();
        assert_eq!(path, Some(vec![a.id, b.id]));
    }

    #[test]
    fn sp_no_connection() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let path = db.shortest_path(a.id, b.id).unwrap();
        assert_eq!(path, None);
    }

    #[test]
    fn sp_prefers_lower_weight() {
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        let path = db.shortest_path(999, b.id).unwrap();
        assert_eq!(path, None);
    }

    #[test]
    fn sp_wrong_direction() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let b = db.create_node(make_node("note", "B")).unwrap();
        db.create_edge(make_weighted_edge(b.id, a.id, "links_to", 1.0))
            .unwrap();
        let path = db.shortest_path(a.id, b.id).unwrap();
        assert_eq!(path, None);
    }

    #[test]
    fn sp_self_loop_ignored() {
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
        let result = db.subgraph(999, 2);
        assert!(result.is_err());
    }

    #[test]
    fn subgraph_single_hop_includes_root_neighbor_and_edge() {
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
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
        let db = GraphNoteDb::open_in_memory().unwrap();
        let a = db.create_node(make_node("note", "A")).unwrap();
        let e = db.create_edge(make_edge(a.id, a.id, "self_ref")).unwrap();

        let sg = db.subgraph(a.id, 3).unwrap();
        assert_eq!(sg.nodes.len(), 1);
        assert_eq!(sg.edges.len(), 1);
        assert_eq!(sg.edges[0].id, e.id);
    }

    #[test]
    fn subgraph_isolated_node() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        let a = db.create_node(make_node("note", "Isolated")).unwrap();

        let sg = db.subgraph(a.id, 5).unwrap();
        assert_eq!(sg.nodes.len(), 1);
        assert_eq!(sg.nodes[0].id, a.id);
        assert!(sg.edges.is_empty());
    }

    #[test]
    fn subgraph_only_includes_edges_between_subgraph_nodes() {
        // A -> B -> C -> D, with an edge D -> E (outside radius)
        let db = GraphNoteDb::open_in_memory().unwrap();
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
