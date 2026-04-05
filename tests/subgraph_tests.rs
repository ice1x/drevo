//! Integration tests for subgraph extraction (task 00024).
//!
//! Tests cover edge cases, graph topologies, and all 5 use-case scenarios.

use graphnote_db::db::GraphNoteDb;
use graphnote_db::model::{NewEdge, NewNode, Properties};
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

// ---------------------------------------------------------------
// Basic / edge-case tests
// ---------------------------------------------------------------

#[test]
fn subgraph_empty_graph_nonexistent_root() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    assert!(db.subgraph(1, 3).is_err());
}

#[test]
fn subgraph_single_node_no_edges() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "Alone")).unwrap();
    let sg = db.subgraph(a.id, 5).unwrap();
    assert_eq!(sg.nodes.len(), 1);
    assert!(sg.edges.is_empty());
}

#[test]
fn subgraph_depth_zero() {
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
fn subgraph_bidirectional_edges() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let e1 = db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    let e2 = db.create_edge(make_edge(b.id, a.id, "links_to")).unwrap();

    let sg = db.subgraph(a.id, 1).unwrap();
    assert_eq!(sg.nodes.len(), 2);
    assert_eq!(sg.edges.len(), 2);
    let edge_ids: HashSet<u64> = sg.edges.iter().map(|e| e.id).collect();
    assert!(edge_ids.contains(&e1.id));
    assert!(edge_ids.contains(&e2.id));
}

#[test]
fn subgraph_self_loop_counted_once() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    db.create_edge(make_edge(a.id, a.id, "self")).unwrap();

    let sg = db.subgraph(a.id, 1).unwrap();
    assert_eq!(sg.nodes.len(), 1);
    assert_eq!(sg.edges.len(), 1);
}

#[test]
fn subgraph_linear_chain_bounded_by_depth() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let mut ids = Vec::new();
    for i in 0..10 {
        let n = db
            .create_node(make_node("note", &format!("N{}", i)))
            .unwrap();
        ids.push(n.id);
    }
    for i in 0..9 {
        db.create_edge(make_edge(ids[i], ids[i + 1], "next"))
            .unwrap();
    }

    // depth 3 from node 0: should get nodes 0,1,2,3
    let sg = db.subgraph(ids[0], 3).unwrap();
    assert_eq!(sg.nodes.len(), 4);
    assert_eq!(sg.edges.len(), 3);
}

#[test]
fn subgraph_disconnected_nodes_not_included() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "Disconnected")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();

    let sg = db.subgraph(a.id, 10).unwrap();
    assert_eq!(sg.nodes.len(), 2);
    let node_ids: HashSet<u64> = sg.nodes.iter().map(|n| n.id).collect();
    assert!(!node_ids.contains(&c.id));
}

#[test]
fn subgraph_hub_spoke_topology() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let hub = db.create_node(make_node("note", "Hub")).unwrap();
    let mut spoke_ids = Vec::new();
    for i in 0..20 {
        let s = db
            .create_node(make_node("note", &format!("Spoke{}", i)))
            .unwrap();
        db.create_edge(make_edge(hub.id, s.id, "links_to")).unwrap();
        spoke_ids.push(s.id);
    }

    let sg = db.subgraph(hub.id, 1).unwrap();
    assert_eq!(sg.nodes.len(), 21);
    assert_eq!(sg.edges.len(), 20);
}

#[test]
fn subgraph_edges_between_non_root_nodes_included() {
    // A -> B, A -> C, B -> C
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(a.id, c.id, "links_to")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "links_to")).unwrap();

    let sg = db.subgraph(a.id, 1).unwrap();
    assert_eq!(sg.nodes.len(), 3);
    // All 3 edges should be included since A, B, C are all in the subgraph
    assert_eq!(sg.edges.len(), 3);
}

#[test]
fn subgraph_incoming_edge_discovered() {
    // B -> A (root), depth 1 should find B via incoming edge
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(b.id, a.id, "links_to")).unwrap();

    let sg = db.subgraph(a.id, 1).unwrap();
    assert_eq!(sg.nodes.len(), 2);
    assert_eq!(sg.edges.len(), 1);
}

#[test]
fn subgraph_mixed_edge_kinds() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(make_edge(a.id, c.id, "tagged_with"))
        .unwrap();

    // subgraph should include all edge kinds
    let sg = db.subgraph(a.id, 1).unwrap();
    assert_eq!(sg.nodes.len(), 3);
    assert_eq!(sg.edges.len(), 2);
}

// ---------------------------------------------------------------
// Use-case scenario: CBT Journal
// ---------------------------------------------------------------

#[test]
fn subgraph_cbt_journal_thought_chain() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    let situation = db
        .create_node(make_node("situation", "Meeting with boss"))
        .unwrap();
    let thought = db
        .create_node(make_node("thought", "They think I'm incompetent"))
        .unwrap();
    let emotion = db.create_node(make_node("emotion", "Anxiety")).unwrap();
    let distortion = db
        .create_node(make_node("cognitive_distortion", "Mind reading"))
        .unwrap();
    let response = db
        .create_node(make_node(
            "rational_response",
            "No evidence they think that",
        ))
        .unwrap();

    db.create_edge(make_edge(situation.id, thought.id, "triggered_by"))
        .unwrap();
    db.create_edge(make_edge(thought.id, emotion.id, "leads_to"))
        .unwrap();
    db.create_edge(make_edge(thought.id, distortion.id, "classified_as"))
        .unwrap();
    db.create_edge(make_edge(distortion.id, response.id, "challenges"))
        .unwrap();

    // Subgraph from thought, depth 2: should capture the whole chain
    let sg = db.subgraph(thought.id, 2).unwrap();
    let node_ids: HashSet<u64> = sg.nodes.iter().map(|n| n.id).collect();
    assert!(node_ids.contains(&thought.id));
    assert!(node_ids.contains(&situation.id));
    assert!(node_ids.contains(&emotion.id));
    assert!(node_ids.contains(&distortion.id));
    assert!(node_ids.contains(&response.id));
    assert_eq!(sg.nodes.len(), 5);
    assert_eq!(sg.edges.len(), 4);
}

// ---------------------------------------------------------------
// Use-case scenario: Story / Book Editor
// ---------------------------------------------------------------

#[test]
fn subgraph_story_editor_scene_context() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    let book = db.create_node(make_node("book", "My Novel")).unwrap();
    let ch1 = db.create_node(make_node("chapter", "Chapter 1")).unwrap();
    let ch2 = db.create_node(make_node("chapter", "Chapter 2")).unwrap();
    let scene1 = db.create_node(make_node("scene", "Opening scene")).unwrap();
    let scene2 = db
        .create_node(make_node("scene", "Conflict scene"))
        .unwrap();
    let character = db.create_node(make_node("character", "Alice")).unwrap();
    let location = db
        .create_node(make_node("location", "Coffee shop"))
        .unwrap();

    db.create_edge(make_edge(book.id, ch1.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(book.id, ch2.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(ch1.id, scene1.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(ch1.id, scene2.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(scene1.id, character.id, "involves"))
        .unwrap();
    db.create_edge(make_edge(scene1.id, location.id, "takes_place_in"))
        .unwrap();
    db.create_edge(make_edge(scene1.id, scene2.id, "follows"))
        .unwrap();

    // Subgraph from scene1, depth 1: scene1 + ch1 + character + location + scene2
    let sg = db.subgraph(scene1.id, 1).unwrap();
    let node_ids: HashSet<u64> = sg.nodes.iter().map(|n| n.id).collect();
    assert!(node_ids.contains(&scene1.id));
    assert!(node_ids.contains(&ch1.id));
    assert!(node_ids.contains(&character.id));
    assert!(node_ids.contains(&location.id));
    assert!(node_ids.contains(&scene2.id));
    assert!(!node_ids.contains(&book.id)); // book is 2 hops away via ch1
    assert_eq!(sg.nodes.len(), 5);
}

// ---------------------------------------------------------------
// Use-case scenario: IT Task Manager
// ---------------------------------------------------------------

#[test]
fn subgraph_task_manager_dependency_chain() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    let epic = db.create_node(make_node("epic", "Auth System")).unwrap();
    let task1 = db
        .create_node(make_node("task", "Design API schema"))
        .unwrap();
    let task2 = db
        .create_node(make_node("task", "Implement login"))
        .unwrap();
    let task3 = db.create_node(make_node("task", "Write tests")).unwrap();
    let dev = db.create_node(make_node("developer", "Alice")).unwrap();
    let sprint = db.create_node(make_node("sprint", "Sprint 12")).unwrap();

    db.create_edge(make_edge(task1.id, epic.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(task2.id, epic.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(task3.id, epic.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(task2.id, task1.id, "depends_on"))
        .unwrap();
    db.create_edge(make_edge(task3.id, task2.id, "depends_on"))
        .unwrap();
    db.create_edge(make_edge(task2.id, dev.id, "assigned_to"))
        .unwrap();
    db.create_edge(make_edge(task2.id, sprint.id, "part_of"))
        .unwrap();

    // Subgraph from task2, depth 1: full context for one task
    let sg = db.subgraph(task2.id, 1).unwrap();
    let node_ids: HashSet<u64> = sg.nodes.iter().map(|n| n.id).collect();
    assert!(node_ids.contains(&task2.id));
    assert!(node_ids.contains(&epic.id));
    assert!(node_ids.contains(&task1.id));
    assert!(node_ids.contains(&task3.id));
    assert!(node_ids.contains(&dev.id));
    assert!(node_ids.contains(&sprint.id));
    assert_eq!(sg.nodes.len(), 6);
}

// ---------------------------------------------------------------
// Use-case scenario: ERP System
// ---------------------------------------------------------------

#[test]
fn subgraph_erp_order_context() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    let order = db.create_node(make_node("order", "ORD-001")).unwrap();
    let product1 = db.create_node(make_node("product", "Widget A")).unwrap();
    let product2 = db.create_node(make_node("product", "Widget B")).unwrap();
    let customer = db.create_node(make_node("customer", "Acme Corp")).unwrap();
    let warehouse = db.create_node(make_node("warehouse", "WH-East")).unwrap();
    let invoice = db.create_node(make_node("invoice", "INV-001")).unwrap();

    db.create_edge(make_edge(order.id, product1.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(order.id, product2.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(order.id, customer.id, "ordered_by"))
        .unwrap();
    db.create_edge(make_edge(product1.id, warehouse.id, "stored_in"))
        .unwrap();
    db.create_edge(make_edge(product2.id, warehouse.id, "stored_in"))
        .unwrap();
    db.create_edge(make_edge(order.id, invoice.id, "billed_to"))
        .unwrap();

    // Subgraph from order, depth 1: order + products + customer + invoice
    let sg = db.subgraph(order.id, 1).unwrap();
    let node_ids: HashSet<u64> = sg.nodes.iter().map(|n| n.id).collect();
    assert!(node_ids.contains(&order.id));
    assert!(node_ids.contains(&product1.id));
    assert!(node_ids.contains(&product2.id));
    assert!(node_ids.contains(&customer.id));
    assert!(node_ids.contains(&invoice.id));
    assert!(!node_ids.contains(&warehouse.id)); // warehouse is 2 hops
    assert_eq!(sg.nodes.len(), 5);
    assert_eq!(sg.edges.len(), 4);

    // Subgraph depth 2: now includes warehouse
    let sg2 = db.subgraph(order.id, 2).unwrap();
    let node_ids2: HashSet<u64> = sg2.nodes.iter().map(|n| n.id).collect();
    assert!(node_ids2.contains(&warehouse.id));
    assert_eq!(sg2.nodes.len(), 6);
    assert_eq!(sg2.edges.len(), 6);
}

// ---------------------------------------------------------------
// Use-case scenario: Bug Tracker
// ---------------------------------------------------------------

#[test]
fn subgraph_bug_tracker_impact_analysis() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    let bug = db
        .create_node(make_node("bug", "Login fails on Safari"))
        .unwrap();
    let feature = db.create_node(make_node("feature", "SSO Login")).unwrap();
    let release = db.create_node(make_node("release", "v2.1")).unwrap();
    let test_case = db
        .create_node(make_node("test_case", "TC-Login-Safari"))
        .unwrap();
    let assignee = db.create_node(make_node("assignee", "Bob")).unwrap();
    let related_bug = db
        .create_node(make_node("bug", "CSS rendering issue"))
        .unwrap();

    db.create_edge(make_edge(bug.id, feature.id, "reported_in"))
        .unwrap();
    db.create_edge(make_edge(bug.id, release.id, "blocks_release"))
        .unwrap();
    db.create_edge(make_edge(bug.id, assignee.id, "assigned_to"))
        .unwrap();
    db.create_edge(make_edge(test_case.id, bug.id, "verifies"))
        .unwrap();
    db.create_edge(make_edge(related_bug.id, bug.id, "related_to"))
        .unwrap();
    db.create_edge(make_edge(related_bug.id, feature.id, "reported_in"))
        .unwrap();

    // Subgraph from bug, depth 1: full impact context
    let sg = db.subgraph(bug.id, 1).unwrap();
    let node_ids: HashSet<u64> = sg.nodes.iter().map(|n| n.id).collect();
    assert!(node_ids.contains(&bug.id));
    assert!(node_ids.contains(&feature.id));
    assert!(node_ids.contains(&release.id));
    assert!(node_ids.contains(&assignee.id));
    assert!(node_ids.contains(&test_case.id));
    assert!(node_ids.contains(&related_bug.id));
    assert_eq!(sg.nodes.len(), 6);

    // Edges: bug->feature, bug->release, bug->assignee, test_case->bug, related_bug->bug
    // Also: related_bug->feature (both endpoints in subgraph)
    assert_eq!(sg.edges.len(), 6);
}
