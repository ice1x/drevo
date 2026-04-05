//! Integration tests for DFS traversal with depth limit and edge kind filter.
//!
//! Covers: basic DFS, depth limits, direction, edge kind filtering, cycles,
//! disconnected graphs, empty graph, self-loops, diamond/fan patterns,
//! and use-case scenarios (CBT, story editor, task manager, ERP, bug tracker).

use graphnote_db::db::GraphNoteDb;
use graphnote_db::model::{Direction, NewEdge, NewNode, Properties};

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

// ---------------------------------------------------------------
// Basic DFS behavior
// ---------------------------------------------------------------

#[test]
fn dfs_empty_graph_node_with_no_edges() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let result = db.dfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert!(result.is_empty());
}

#[test]
fn dfs_depth_zero() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    let result = db.dfs(a.id, 0, Direction::Outgoing, None).unwrap();
    assert!(result.is_empty());
}

#[test]
fn dfs_depth_one_outgoing() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();

    let result = db.dfs(a.id, 1, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, b.id);
}

#[test]
fn dfs_depth_two_reaches_grandchildren() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();

    let result = db.dfs(a.id, 2, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 2);
    let ids: Vec<u64> = result.iter().map(|n| n.id).collect();
    assert!(ids.contains(&b.id));
    assert!(ids.contains(&c.id));
}

// ---------------------------------------------------------------
// Direction
// ---------------------------------------------------------------

#[test]
fn dfs_outgoing_only() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(b.id, a.id, "links_to")).unwrap();

    let result = db.dfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert!(result.is_empty());
}

#[test]
fn dfs_incoming_only() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(b.id, a.id, "links_to")).unwrap();

    let result = db.dfs(a.id, 5, Direction::Incoming, None).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, b.id);
}

#[test]
fn dfs_both_directions() {
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

// ---------------------------------------------------------------
// Edge kind filtering
// ---------------------------------------------------------------

#[test]
fn dfs_edge_kind_filter_includes() {
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
}

#[test]
fn dfs_edge_kind_filter_excludes() {
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
fn dfs_edge_kind_filter_multi_hop() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    let d = db.create_node(make_node("note", "D")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();
    db.create_edge(make_edge(b.id, d.id, "tagged_with"))
        .unwrap();

    // Only follow "links_to" — should reach B and C but not D
    let result = db
        .dfs(a.id, 3, Direction::Outgoing, Some("links_to"))
        .unwrap();
    assert_eq!(result.len(), 2);
    let ids: Vec<u64> = result.iter().map(|n| n.id).collect();
    assert!(ids.contains(&b.id));
    assert!(ids.contains(&c.id));
    assert!(!ids.contains(&d.id));
}

// ---------------------------------------------------------------
// Cycles and special graphs
// ---------------------------------------------------------------

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
    assert_eq!(result.len(), 3);
    let ids: Vec<u64> = result.iter().map(|n| n.id).collect();
    assert!(ids.contains(&b.id));
    assert!(ids.contains(&c.id));
    assert!(ids.contains(&d.id));
}

#[test]
fn dfs_disconnected_components() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

    let result = db.dfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, b.id);
    assert!(!result.iter().any(|n| n.id == c.id));
}

#[test]
fn dfs_single_node_graph() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let result = db.dfs(a.id, 5, Direction::Both, None).unwrap();
    assert!(result.is_empty());
}

#[test]
fn dfs_fan_out_10_spokes() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let hub = db.create_node(make_node("note", "Hub")).unwrap();
    for i in 0..10 {
        let spoke = db
            .create_node(make_node("note", &format!("Spoke{}", i)))
            .unwrap();
        db.create_edge(make_edge(hub.id, spoke.id, "links_to"))
            .unwrap();
    }

    let result = db.dfs(hub.id, 1, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 10);
}

#[test]
fn dfs_long_chain_depth_limited() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let mut prev = db.create_node(make_node("note", "Node0")).unwrap();
    for i in 1..20 {
        let next = db
            .create_node(make_node("note", &format!("Node{}", i)))
            .unwrap();
        db.create_edge(make_edge(prev.id, next.id, "links_to"))
            .unwrap();
        prev = next;
    }

    // depth 5 from Node0 should reach Node1..Node5
    let result = db.dfs(1, 5, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 5);
}

// ---------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------

#[test]
fn dfs_max_depth_255() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

    let result = db.dfs(a.id, 255, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 1);
}

#[test]
fn dfs_multiple_edges_same_pair() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "tagged_with"))
        .unwrap();

    let result = db.dfs(a.id, 1, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, b.id);
}

#[test]
fn dfs_bidirectional_edges() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(b.id, a.id, "links_to")).unwrap();

    let result = db.dfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 1); // Only B, not A revisited
}

// ---------------------------------------------------------------
// Use-case scenarios
// ---------------------------------------------------------------

/// CBT Journal: trace a thought chain via DFS — goes deep along one path first.
#[test]
fn scenario_cbt_thought_chain_dfs() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let situation = db
        .create_node(make_node("situation", "Presentation at work"))
        .unwrap();
    let thought = db.create_node(make_node("thought", "I will fail")).unwrap();
    let emotion = db.create_node(make_node("emotion", "Anxiety")).unwrap();
    let distortion = db
        .create_node(make_node("cognitive_distortion", "Catastrophizing"))
        .unwrap();
    let rational = db
        .create_node(make_node("rational_response", "I have prepared well"))
        .unwrap();

    db.create_edge(make_edge(situation.id, thought.id, "triggered_by"))
        .unwrap();
    db.create_edge(make_edge(thought.id, emotion.id, "leads_to"))
        .unwrap();
    db.create_edge(make_edge(thought.id, distortion.id, "challenges"))
        .unwrap();
    db.create_edge(make_edge(distortion.id, rational.id, "reframed_as"))
        .unwrap();

    // DFS from situation, depth 3 — should reach all 4 nodes
    let result = db.dfs(situation.id, 3, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 4);

    // Filter by "triggered_by" — only thought
    let result = db
        .dfs(situation.id, 1, Direction::Outgoing, Some("triggered_by"))
        .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].title, "I will fail");
}

/// Story Editor: DFS navigates deep into the story tree.
#[test]
fn scenario_story_editor_tree_dfs() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let book = db
        .create_node(make_node("book", "The Great Novel"))
        .unwrap();
    let ch1 = db.create_node(make_node("chapter", "Chapter 1")).unwrap();
    let ch2 = db.create_node(make_node("chapter", "Chapter 2")).unwrap();
    let scene1 = db.create_node(make_node("scene", "Opening scene")).unwrap();
    let scene2 = db.create_node(make_node("scene", "Climax")).unwrap();
    let char1 = db.create_node(make_node("character", "Hero")).unwrap();

    db.create_edge(make_edge(book.id, ch1.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(book.id, ch2.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(ch1.id, scene1.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(ch2.id, scene2.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(scene1.id, char1.id, "involves"))
        .unwrap();

    // Only "contains" edges, depth 3 — DFS should reach ch1, ch2, scene1, scene2
    let result = db
        .dfs(book.id, 3, Direction::Outgoing, Some("contains"))
        .unwrap();
    assert_eq!(result.len(), 4);

    // Without kind filter, depth 3 includes character too
    let result = db.dfs(book.id, 3, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 5);
}

/// IT Task Manager: find blocking chain via DFS.
#[test]
fn scenario_task_dependency_chain_dfs() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let deploy = db.create_node(make_node("task", "Deploy v2.0")).unwrap();
    let tests = db
        .create_node(make_node("task", "Run integration tests"))
        .unwrap();
    let bugfix = db.create_node(make_node("task", "Fix auth bug")).unwrap();
    let review = db.create_node(make_node("task", "Code review")).unwrap();

    db.create_edge(make_edge(deploy.id, tests.id, "blocks"))
        .unwrap();
    db.create_edge(make_edge(tests.id, bugfix.id, "depends_on"))
        .unwrap();
    db.create_edge(make_edge(bugfix.id, review.id, "depends_on"))
        .unwrap();

    // DFS from tests via "depends_on" — reaches bugfix and review
    let result = db
        .dfs(tests.id, 5, Direction::Outgoing, Some("depends_on"))
        .unwrap();
    assert_eq!(result.len(), 2);
    let titles: Vec<&str> = result.iter().map(|n| n.title.as_str()).collect();
    assert!(titles.contains(&"Fix auth bug"));
    assert!(titles.contains(&"Code review"));
}

/// ERP: navigate order relationships via DFS.
#[test]
fn scenario_erp_order_graph_dfs() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let order = db.create_node(make_node("order", "ORD-001")).unwrap();
    let product1 = db.create_node(make_node("product", "Widget A")).unwrap();
    let product2 = db.create_node(make_node("product", "Widget B")).unwrap();
    let warehouse = db
        .create_node(make_node("warehouse", "Warehouse East"))
        .unwrap();
    let customer = db.create_node(make_node("customer", "Acme Corp")).unwrap();

    db.create_edge(make_edge(order.id, product1.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(order.id, product2.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(product1.id, warehouse.id, "stored_in"))
        .unwrap();
    db.create_edge(make_edge(order.id, customer.id, "ordered_by"))
        .unwrap();

    // DFS depth 2: reach all 4 neighbors
    let result = db.dfs(order.id, 2, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 4);
}

/// Bug Tracker: impact analysis via DFS.
#[test]
fn scenario_bug_tracker_impact_dfs() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let bug = db
        .create_node(make_node("bug", "Memory leak in parser"))
        .unwrap();
    let feature = db.create_node(make_node("feature", "JSON import")).unwrap();
    let release = db.create_node(make_node("release", "v3.0")).unwrap();
    let test_case = db
        .create_node(make_node("test_case", "Test large JSON import"))
        .unwrap();

    db.create_edge(make_edge(bug.id, feature.id, "reported_in"))
        .unwrap();
    db.create_edge(make_edge(feature.id, release.id, "blocks_release"))
        .unwrap();
    db.create_edge(make_edge(test_case.id, bug.id, "verified_by"))
        .unwrap();

    // DFS impact from bug: feature, release
    let result = db.dfs(bug.id, 2, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 2);

    // What verifies this bug? (incoming)
    let result = db.dfs(bug.id, 1, Direction::Incoming, None).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].title, "Test large JSON import");
}
