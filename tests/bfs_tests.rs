//! Integration tests for BFS traversal with depth limit and edge kind filter.
//!
//! Covers: basic BFS, depth limits, direction, edge kind filtering, cycles,
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
// Basic BFS behavior
// ---------------------------------------------------------------

#[test]
fn bfs_empty_graph_node_with_no_edges() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let result = db.bfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert!(result.is_empty());
}

#[test]
fn bfs_depth_zero() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    let result = db.bfs(a.id, 0, Direction::Outgoing, None).unwrap();
    assert!(result.is_empty());
}

#[test]
fn bfs_depth_one_outgoing() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();

    let result = db.bfs(a.id, 1, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, b.id);
}

#[test]
fn bfs_depth_two_chain() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    let d = db.create_node(make_node("note", "D")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();
    db.create_edge(make_edge(c.id, d.id, "links_to")).unwrap();

    let result = db.bfs(a.id, 2, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 2);
    let ids: Vec<u64> = result.iter().map(|n| n.id).collect();
    assert!(ids.contains(&b.id));
    assert!(ids.contains(&c.id));
    assert!(!ids.contains(&d.id)); // beyond depth 2
}

#[test]
fn bfs_excludes_start_node() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

    let result = db.bfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert!(!result.iter().any(|n| n.id == a.id));
}

// ---------------------------------------------------------------
// Direction tests
// ---------------------------------------------------------------

#[test]
fn bfs_outgoing_ignores_incoming_edges() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(b.id, a.id, "links_to")).unwrap();

    let result = db.bfs(a.id, 1, Direction::Outgoing, None).unwrap();
    assert!(result.is_empty());
}

#[test]
fn bfs_incoming_follows_reverse() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(b.id, a.id, "links_to")).unwrap();
    db.create_edge(make_edge(c.id, b.id, "links_to")).unwrap();

    let result = db.bfs(a.id, 2, Direction::Incoming, None).unwrap();
    assert_eq!(result.len(), 2);
    let ids: Vec<u64> = result.iter().map(|n| n.id).collect();
    assert!(ids.contains(&b.id));
    assert!(ids.contains(&c.id));
}

#[test]
fn bfs_both_directions() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(c.id, a.id, "links_to")).unwrap();

    let result = db.bfs(a.id, 1, Direction::Both, None).unwrap();
    assert_eq!(result.len(), 2);
    let ids: Vec<u64> = result.iter().map(|n| n.id).collect();
    assert!(ids.contains(&b.id));
    assert!(ids.contains(&c.id));
}

// ---------------------------------------------------------------
// Edge kind filter
// ---------------------------------------------------------------

#[test]
fn bfs_edge_kind_filter_includes_matching() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(a.id, c.id, "tagged_with"))
        .unwrap();

    let result = db
        .bfs(a.id, 1, Direction::Outgoing, Some("links_to"))
        .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, b.id);
}

#[test]
fn bfs_edge_kind_filter_excludes_non_matching() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

    let result = db
        .bfs(a.id, 1, Direction::Outgoing, Some("tagged_with"))
        .unwrap();
    assert!(result.is_empty());
}

#[test]
fn bfs_edge_kind_filter_multi_hop() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    let d = db.create_node(make_node("note", "D")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();
    db.create_edge(make_edge(b.id, d.id, "tagged_with"))
        .unwrap();

    // Following only links_to: A -> B -> C (D excluded)
    let result = db
        .bfs(a.id, 3, Direction::Outgoing, Some("links_to"))
        .unwrap();
    assert_eq!(result.len(), 2);
    let ids: Vec<u64> = result.iter().map(|n| n.id).collect();
    assert!(ids.contains(&b.id));
    assert!(ids.contains(&c.id));
    assert!(!ids.contains(&d.id));
}

// ---------------------------------------------------------------
// Cycle handling
// ---------------------------------------------------------------

#[test]
fn bfs_cycle_does_not_loop() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();
    db.create_edge(make_edge(c.id, a.id, "links_to")).unwrap();

    let result = db.bfs(a.id, 100, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 2); // B and C only
}

#[test]
fn bfs_self_loop_ignored() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, a.id, "self_ref")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

    let result = db.bfs(a.id, 2, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, b.id);
}

// ---------------------------------------------------------------
// Graph topology patterns
// ---------------------------------------------------------------

#[test]
fn bfs_diamond_graph() {
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
}

#[test]
fn bfs_disconnected_components() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    // C is disconnected

    let result = db.bfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, b.id);
    assert!(!result.iter().any(|n| n.id == c.id));
}

#[test]
fn bfs_single_node_graph() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let result = db.bfs(a.id, 5, Direction::Both, None).unwrap();
    assert!(result.is_empty());
}

#[test]
fn bfs_fan_out_10_spokes() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let hub = db.create_node(make_node("note", "Hub")).unwrap();
    for i in 0..10 {
        let spoke = db
            .create_node(make_node("note", &format!("Spoke{}", i)))
            .unwrap();
        db.create_edge(make_edge(hub.id, spoke.id, "links_to"))
            .unwrap();
    }

    let result = db.bfs(hub.id, 1, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 10);
}

#[test]
fn bfs_long_chain_depth_limited() {
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
    let result = db.bfs(1, 5, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 5);
}

// ---------------------------------------------------------------
// neighbors() convenience method
// ---------------------------------------------------------------

#[test]
fn neighbors_returns_depth_1() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();

    let result = db.neighbors(a.id, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, b.id);
}

#[test]
fn neighbors_with_edge_kind_filter() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(a.id, c.id, "tagged_with"))
        .unwrap();

    let result = db
        .neighbors(a.id, Direction::Outgoing, Some("tagged_with"))
        .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, c.id);
}

// ---------------------------------------------------------------
// Use-case scenarios
// ---------------------------------------------------------------

/// CBT Journal: trace a thought chain through cognitive distortions.
#[test]
fn scenario_cbt_thought_chain() {
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

    // BFS from situation, depth 3, should reach thought, emotion, distortion, rational
    let result = db.bfs(situation.id, 3, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 4);

    // Filter by "triggered_by" — only thought
    let result = db
        .bfs(situation.id, 1, Direction::Outgoing, Some("triggered_by"))
        .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].title, "I will fail");

    // Trace chain from thought via "leads_to" edges only
    let result = db
        .bfs(thought.id, 5, Direction::Outgoing, Some("leads_to"))
        .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].title, "Anxiety");
}

/// Story Editor: navigate a book structure.
#[test]
fn scenario_story_editor_tree() {
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

    // Depth 1 from book: chapters only
    let result = db.bfs(book.id, 1, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 2);

    // Depth 2: chapters + scenes
    let result = db.bfs(book.id, 2, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 4); // ch1, ch2, scene1, scene2

    // Only "contains" edges, depth 3
    let result = db
        .bfs(book.id, 3, Direction::Outgoing, Some("contains"))
        .unwrap();
    assert_eq!(result.len(), 4); // ch1, ch2, scene1, scene2 (hero excluded)

    // Without kind filter, depth 3 includes character too
    let result = db.bfs(book.id, 3, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 5);
}

/// IT Task Manager: find blocking chain via BFS.
#[test]
fn scenario_task_dependency_chain() {
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

    // BFS incoming from deploy: what blocks the deploy?
    // From deploy outgoing "blocks" -> tests
    let result = db
        .bfs(deploy.id, 1, Direction::Outgoing, Some("blocks"))
        .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].title, "Run integration tests");

    // Full dependency chain from tests
    let result = db
        .bfs(tests.id, 5, Direction::Outgoing, Some("depends_on"))
        .unwrap();
    assert_eq!(result.len(), 2);
}

/// ERP: navigate order relationships.
#[test]
fn scenario_erp_order_graph() {
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

    // All order neighbors
    let result = db.neighbors(order.id, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 3); // product1, product2, customer

    // Only products in the order
    let result = db
        .neighbors(order.id, Direction::Outgoing, Some("contains"))
        .unwrap();
    assert_eq!(result.len(), 2);

    // BFS depth 2: reach warehouse via product
    let result = db.bfs(order.id, 2, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 4); // 2 products + warehouse + customer
}

/// Bug Tracker: impact analysis via BFS.
#[test]
fn scenario_bug_tracker_impact() {
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

    // Impact of the bug: follow outgoing edges
    let result = db.bfs(bug.id, 2, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 2); // feature, release

    // What verifies this bug? (incoming)
    let result = db.bfs(bug.id, 1, Direction::Incoming, None).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].title, "Test large JSON import");
}

// ---------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------

#[test]
fn bfs_max_depth_255() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

    // Should not panic or loop with u8::MAX depth
    let result = db.bfs(a.id, 255, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 1);
}

#[test]
fn bfs_multiple_edges_same_pair() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "tagged_with"))
        .unwrap();

    // B should appear only once
    let result = db.bfs(a.id, 1, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, b.id);
}

#[test]
fn bfs_bidirectional_edges() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(b.id, a.id, "links_to")).unwrap();

    let result = db.bfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert_eq!(result.len(), 1); // Only B, not A revisited
}
