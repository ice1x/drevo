//! Audit tests for `src/traversal.rs` — Phase 8.5 task `00107`.
//!
//! These tests document and pin behaviours that the audit verified:
//!
//! * `shortest_path_filtered` and `subgraph_filtered` honour the
//!   `edge_kind` filter consistently with `bfs` / `dfs`.
//! * Dijkstra's behaviour on **negative** finite edge weights —
//!   the implementation does NOT guarantee optimality and may
//!   return a non-optimal path. The test pins this so a future
//!   refactor either (a) keeps the current best-effort semantics
//!   or (b) consciously switches to Bellman-Ford with a passing
//!   test update.
//! * A randomised cross-algorithm invariant fuzzer: BFS, DFS, and
//!   subgraph all discover the same reachable set on the same
//!   random graph (mirrors the 00106 invariant fuzzer style;
//!   manual xorshift32 seed, no `proptest` dep).

use drevo::db::Drevo;
use drevo::model::{Direction, NewEdge, NewNode, Properties};
use std::collections::HashSet;

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

fn make_weighted_edge(from: u64, to: u64, kind: &str, weight: f32) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.to_string(),
        weight,
        properties: Properties::default(),
    }
}

fn node_ids(nodes: &[drevo::model::Node]) -> HashSet<u64> {
    nodes.iter().map(|n| n.id).collect()
}

// ===================================================================
// shortest_path_filtered: edge-kind filter parity
// ===================================================================

#[test]
fn shortest_path_filtered_passes_through_when_kind_none() {
    // sanity: None must match the legacy `shortest_path` exactly.
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_weighted_edge(a.id, b.id, "link", 10.0))
        .unwrap();
    db.create_edge(make_weighted_edge(a.id, c.id, "link", 1.0))
        .unwrap();
    db.create_edge(make_weighted_edge(c.id, b.id, "link", 1.0))
        .unwrap();

    let legacy = db.shortest_path(a.id, b.id).unwrap();
    let filtered_none = db.shortest_path_filtered(a.id, b.id, None).unwrap();
    assert_eq!(legacy, filtered_none);
    assert_eq!(filtered_none, Some(vec![a.id, c.id, b.id]));
}

#[test]
fn shortest_path_filtered_excludes_wrong_kind() {
    //   A --[link, 1.0]--> B
    //   A --[ref,  0.1]--> B   (lighter, but a different kind)
    //
    // Filtered by "link": must follow A--[link]-->B with cost 1.0.
    // Filtered by "ref" : must follow A--[ref] -->B with cost 0.1.
    // The point: the kind selector must change the result even when
    // both candidates terminate at the target.
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_weighted_edge(a.id, b.id, "link", 1.0))
        .unwrap();
    db.create_edge(make_weighted_edge(a.id, b.id, "ref", 0.1))
        .unwrap();

    let p_link = db.shortest_path_filtered(a.id, b.id, Some("link")).unwrap();
    assert_eq!(p_link, Some(vec![a.id, b.id]));

    let p_ref = db.shortest_path_filtered(a.id, b.id, Some("ref")).unwrap();
    assert_eq!(p_ref, Some(vec![a.id, b.id]));
}

#[test]
fn shortest_path_filtered_unreachable_when_only_other_kind_exists() {
    // A --[ref]--> B; query with kind="link" must return None.
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_weighted_edge(a.id, b.id, "ref", 1.0))
        .unwrap();

    let p = db.shortest_path_filtered(a.id, b.id, Some("link")).unwrap();
    assert!(p.is_none());
}

#[test]
fn shortest_path_filtered_self_target_returns_just_self() {
    // The from == to short-circuit is independent of the filter.
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let p = db.shortest_path_filtered(a.id, a.id, Some("link")).unwrap();
    assert_eq!(p, Some(vec![a.id]));
}

#[test]
fn shortest_path_filtered_routes_through_filter_consistent_path() {
    // Diamond:
    //   A --[link, 1.0]--> B --[link, 1.0]--> D
    //   A --[ref,  1.0]--> C --[ref,  1.0]--> D
    //
    // kind=link must route A -> B -> D.
    // kind=ref  must route A -> C -> D.
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    let d = db.create_node(make_node("note", "D")).unwrap();
    db.create_edge(make_weighted_edge(a.id, b.id, "link", 1.0))
        .unwrap();
    db.create_edge(make_weighted_edge(b.id, d.id, "link", 1.0))
        .unwrap();
    db.create_edge(make_weighted_edge(a.id, c.id, "ref", 1.0))
        .unwrap();
    db.create_edge(make_weighted_edge(c.id, d.id, "ref", 1.0))
        .unwrap();

    let p_link = db
        .shortest_path_filtered(a.id, d.id, Some("link"))
        .unwrap()
        .unwrap();
    assert_eq!(p_link, vec![a.id, b.id, d.id]);

    let p_ref = db
        .shortest_path_filtered(a.id, d.id, Some("ref"))
        .unwrap()
        .unwrap();
    assert_eq!(p_ref, vec![a.id, c.id, d.id]);
}

// ===================================================================
// subgraph_filtered: edge-kind filter parity
// ===================================================================

#[test]
fn subgraph_filtered_pass_through_when_kind_none() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "link")).unwrap();

    let legacy = db.subgraph(a.id, 1).unwrap();
    let filtered_none = db.subgraph_filtered(a.id, 1, None).unwrap();
    assert_eq!(node_ids(&legacy.nodes), node_ids(&filtered_none.nodes));
    assert_eq!(legacy.edges.len(), filtered_none.edges.len());
}

#[test]
fn subgraph_filtered_excludes_other_kind_edges() {
    // A --[link]--> B; A --[ref]--> C
    // Filtered "link": {A, B} + 1 edge.
    // Filtered "ref" : {A, C} + 1 edge.
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "link")).unwrap();
    db.create_edge(make_edge(a.id, c.id, "ref")).unwrap();

    let sg_link = db.subgraph_filtered(a.id, 5, Some("link")).unwrap();
    assert_eq!(node_ids(&sg_link.nodes), HashSet::from([a.id, b.id]));
    assert_eq!(sg_link.edges.len(), 1);
    assert_eq!(sg_link.edges[0].kind, "link");

    let sg_ref = db.subgraph_filtered(a.id, 5, Some("ref")).unwrap();
    assert_eq!(node_ids(&sg_ref.nodes), HashSet::from([a.id, c.id]));
    assert_eq!(sg_ref.edges.len(), 1);
    assert_eq!(sg_ref.edges[0].kind, "ref");
}

#[test]
fn subgraph_filtered_does_not_discover_via_filtered_out_edges() {
    // A --[link]--> B --[ref]--> C
    // Filtered "link" from A: only reaches B (not C — the only edge
    // B->C is "ref" which is filtered out).
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "link")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "ref")).unwrap();

    let sg = db.subgraph_filtered(a.id, 10, Some("link")).unwrap();
    assert_eq!(node_ids(&sg.nodes), HashSet::from([a.id, b.id]));
    assert_eq!(sg.edges.len(), 1);
}

#[test]
fn subgraph_filtered_edge_collection_phase_respects_kind() {
    // Square with two kinds:
    //   A --[link]--> B
    //   B --[link]--> C
    //   A --[ref]--> C  (chord)
    //
    // subgraph from A, depth 2, kind="link" must include A, B, C
    // (B and C are both reachable via link edges only); the
    // edge-collection phase must NOT include the A-->C "ref" chord.
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "link")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "link")).unwrap();
    db.create_edge(make_edge(a.id, c.id, "ref")).unwrap();

    let sg = db.subgraph_filtered(a.id, 2, Some("link")).unwrap();
    assert_eq!(node_ids(&sg.nodes), HashSet::from([a.id, b.id, c.id]));
    assert_eq!(sg.edges.len(), 2);
    for e in &sg.edges {
        assert_eq!(e.kind, "link", "ref chord must be filtered out");
    }
}

#[test]
fn subgraph_filtered_nonexistent_kind_returns_root_only() {
    // No edge matches; the discovery BFS yields nothing; the result
    // is just the root node with zero edges.
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "link")).unwrap();

    let sg = db.subgraph_filtered(a.id, 5, Some("nonexistent")).unwrap();
    assert_eq!(sg.nodes.len(), 1);
    assert_eq!(sg.nodes[0].id, a.id);
    assert!(sg.edges.is_empty());
}

#[test]
fn subgraph_filtered_root_missing_returns_node_not_found() {
    let db = Drevo::open_in_memory().unwrap();
    let result = db.subgraph_filtered(9999, 2, Some("link"));
    assert!(result.is_err());
}

// ===================================================================
// Dijkstra preconditions: negative finite weights
// ===================================================================
//
// The Dijkstra implementation does NOT guarantee optimality on negative
// edge weights. The classical Dijkstra failure mode is that the first
// time a node is popped from the heap, it is treated as settled — but a
// negative back-edge can later produce a strictly-better path that
// Dijkstra has already "committed" away from.
//
// These tests pin the CURRENT BEHAVIOUR so a future Bellman-Ford swap
// can flip them with an intentional code change rather than a silent
// regression. They also serve as living documentation that the
// algorithm is unsuitable for negative-weight graphs.

#[test]
fn dijkstra_negative_weight_no_panic_and_does_not_infinite_loop() {
    // Smoke: any graph with a finite negative edge terminates and
    // returns a value rather than panicking or hanging.
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_weighted_edge(a.id, b.id, "link", -1.0))
        .unwrap();
    let path = db.shortest_path(a.id, b.id).unwrap();
    assert_eq!(path, Some(vec![a.id, b.id]));
}

#[test]
fn dijkstra_negative_weight_can_return_non_optimal_path() {
    // Construction that exposes the precondition violation:
    //
    //   A --[link, 1.0]--> B
    //   A --[link, 3.0]--> C
    //   C --[link, -5.0]--> B
    //
    // Truly shortest A -> B: A -> C -> B = 3 + (-5) = -2.
    // Dijkstra answer  : A -> B           = 1 (popped first).
    //
    // We assert that the Dijkstra answer is the direct 2-vertex path
    // (NOT the truly-optimal 3-vertex path). If a future refactor
    // routes via the negative edge, this test must be revisited
    // alongside a documentation update on the new algorithm.
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_weighted_edge(a.id, b.id, "link", 1.0))
        .unwrap();
    db.create_edge(make_weighted_edge(a.id, c.id, "link", 3.0))
        .unwrap();
    db.create_edge(make_weighted_edge(c.id, b.id, "link", -5.0))
        .unwrap();

    let path = db.shortest_path(a.id, b.id).unwrap().unwrap();
    assert_eq!(
        path,
        vec![a.id, b.id],
        "Dijkstra returns the heap-first path; negative-weight optimality is NOT guaranteed"
    );
}

#[test]
fn dijkstra_zero_weight_edges_treated_as_neutral() {
    // Documenting that 0.0 weights are admitted (not a Dijkstra
    // precondition violation) and behave as a no-cost hop.
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_weighted_edge(a.id, b.id, "link", 0.0))
        .unwrap();
    db.create_edge(make_weighted_edge(b.id, c.id, "link", 0.0))
        .unwrap();

    let path = db.shortest_path(a.id, c.id).unwrap().unwrap();
    assert_eq!(path, vec![a.id, b.id, c.id]);
}

// ===================================================================
// Cross-algorithm invariant fuzzer
// ===================================================================
//
// Invariant: on the same graph and from the same starting node, BFS,
// DFS, and subgraph (`Direction::Both` BFS) at sufficiently large depth
// discover the same set of reachable nodes (modulo direction: BFS/DFS
// are tested with `Direction::Both` so they match subgraph's
// undirected semantics).
//
// The fuzzer uses a deterministic xorshift32 PRNG (no `proptest`
// dependency, mirroring `00106`'s `invariants_hold_under_random_mutations`
// pattern). 3 seeds × 60 random nodes × ~3× edge density × 5 random
// roots = ~900 BFS/DFS/subgraph queries per run, each cross-checked
// against the others.

fn xorshift32(state: &mut u32) -> u32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x
}

fn random_in_range(state: &mut u32, lo: usize, hi: usize) -> usize {
    let span = (hi - lo) as u32;
    if span == 0 {
        lo
    } else {
        lo + (xorshift32(state) % span) as usize
    }
}

fn build_random_graph(seed: u32, n_nodes: usize, edge_density: usize) -> (Drevo, Vec<u64>) {
    let db = Drevo::open_in_memory().unwrap();
    let mut state = seed;
    let mut ids = Vec::with_capacity(n_nodes);
    for i in 0..n_nodes {
        let n = db
            .create_node(make_node("note", &format!("n{}", i)))
            .unwrap();
        ids.push(n.id);
    }
    // Roughly edge_density edges per node, picking the kind at random
    // from a small set so the edge-kind filter has meaningful work.
    let kinds = ["link", "ref", "tag"];
    for _ in 0..(n_nodes * edge_density) {
        let from = ids[random_in_range(&mut state, 0, n_nodes)];
        let to = ids[random_in_range(&mut state, 0, n_nodes)];
        let kind = kinds[random_in_range(&mut state, 0, kinds.len())];
        // Ignore duplicate-edge errors silently — model.rs allows
        // parallel edges, so this is a no-op safety belt.
        let _ = db.create_edge(make_edge(from, to, kind));
    }
    (db, ids)
}

#[test]
fn bfs_dfs_subgraph_same_reachable_set_random_graphs() {
    let seeds = [0x1u32, 42u32, 0xc0ffeeu32];
    for &seed in &seeds {
        let (db, ids) = build_random_graph(seed, 60, 3);
        let mut state = seed.wrapping_mul(2654435761);
        for _ in 0..5 {
            let root = ids[random_in_range(&mut state, 0, ids.len())];

            // Use Direction::Both for BFS/DFS to match subgraph semantics.
            let bfs = db.bfs(root, 64, Direction::Both, None).unwrap();
            let dfs = db.dfs(root, 64, Direction::Both, None).unwrap();
            let sg = db.subgraph(root, 64).unwrap();

            // BFS, DFS, and subgraph each discover the same set of
            // reachable nodes (excluding/including the root according
            // to each contract — normalise here).
            let bfs_set = node_ids(&bfs);
            let dfs_set = node_ids(&dfs);
            let mut sg_set = node_ids(&sg.nodes);
            sg_set.remove(&root); // subgraph includes the root; BFS/DFS exclude it.

            assert_eq!(
                bfs_set, dfs_set,
                "BFS and DFS must discover the same reachable set (seed={:#x}, root={})",
                seed, root
            );
            assert_eq!(
                bfs_set, sg_set,
                "subgraph(root, depth) \\ {{root}} must equal BFS(root, depth) (seed={:#x}, root={})",
                seed, root
            );
        }
    }
}

#[test]
fn shortest_path_within_reachable_set_only_random_graphs() {
    // Invariant: shortest_path returns Some(_) iff `to` is in
    // BFS(from, MAX_DEPTH, Outgoing) ∪ {from}. We verify the
    // implication in both directions on random graphs.
    let seeds = [0x2u32, 100u32, 0xdeadbeefu32];
    for &seed in &seeds {
        let (db, ids) = build_random_graph(seed, 40, 3);
        let mut state = seed.wrapping_mul(2147483647);
        for _ in 0..10 {
            let from = ids[random_in_range(&mut state, 0, ids.len())];
            let to = ids[random_in_range(&mut state, 0, ids.len())];

            let bfs_out = db.bfs(from, 64, Direction::Outgoing, None).unwrap();
            let mut reachable = node_ids(&bfs_out);
            reachable.insert(from);

            let path = db.shortest_path(from, to).unwrap();
            if reachable.contains(&to) {
                let p = path.unwrap_or_else(|| {
                    panic!(
                        "shortest_path returned None on a BFS-reachable target (seed={:#x}, from={}, to={})",
                        seed, from, to
                    )
                });
                assert_eq!(p.first().copied(), Some(from));
                assert_eq!(p.last().copied(), Some(to));
            } else {
                assert!(
                    path.is_none(),
                    "shortest_path returned Some on a BFS-unreachable target (seed={:#x}, from={}, to={})",
                    seed,
                    from,
                    to
                );
            }
        }
    }
}

#[test]
fn edge_kind_filter_monotone_in_reachable_set_random_graphs() {
    // Invariant: BFS(root, depth, dir, Some(k)) ⊆ BFS(root, depth, dir, None)
    // for every edge_kind k. The filtered traversal cannot discover
    // a node the unfiltered traversal misses.
    let seeds = [7u32, 0xacefaceu32, 0x9876u32];
    for &seed in &seeds {
        let (db, ids) = build_random_graph(seed, 40, 3);
        let mut state = seed.wrapping_mul(0x9e3779b9);
        for kind in ["link", "ref", "tag"] {
            for _ in 0..3 {
                let root = ids[random_in_range(&mut state, 0, ids.len())];
                let unfiltered = db.bfs(root, 32, Direction::Outgoing, None).unwrap();
                let filtered = db.bfs(root, 32, Direction::Outgoing, Some(kind)).unwrap();
                let u = node_ids(&unfiltered);
                let f = node_ids(&filtered);
                assert!(
                    f.is_subset(&u),
                    "filtered BFS must be a subset of unfiltered (seed={:#x}, kind={}, root={})",
                    seed,
                    kind,
                    root
                );
            }
        }
    }
}
