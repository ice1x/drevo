//! Integration tests for shortest_path (Dijkstra).

use drevo::db::Drevo;
use drevo::model::{NewEdge, NewNode, Properties};

fn make_node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn make_edge(from: u64, to: u64, kind: &str, weight: f32) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.to_string(),
        weight,
        properties: Properties::default(),
    }
}

// ---------------------------------------------------------------
// Basic cases
// ---------------------------------------------------------------

#[test]
fn shortest_path_same_node() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();

    let path = db.shortest_path(a.id, a.id).unwrap();
    assert_eq!(path, Some(vec![a.id]));
}

#[test]
fn shortest_path_direct_edge() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to", 1.0))
        .unwrap();

    let path = db.shortest_path(a.id, b.id).unwrap();
    assert_eq!(path, Some(vec![a.id, b.id]));
}

#[test]
fn shortest_path_no_connection() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();

    let path = db.shortest_path(a.id, b.id).unwrap();
    assert_eq!(path, None);
}

#[test]
fn shortest_path_wrong_direction() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    // Edge from B to A, not from A to B
    db.create_edge(make_edge(b.id, a.id, "links_to", 1.0))
        .unwrap();

    let path = db.shortest_path(a.id, b.id).unwrap();
    assert_eq!(path, None);
}

#[test]
fn shortest_path_two_hops() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to", 1.0))
        .unwrap();
    db.create_edge(make_edge(b.id, c.id, "links_to", 1.0))
        .unwrap();

    let path = db.shortest_path(a.id, c.id).unwrap();
    assert_eq!(path, Some(vec![a.id, b.id, c.id]));
}

// ---------------------------------------------------------------
// Weight-based routing
// ---------------------------------------------------------------

#[test]
fn shortest_path_prefers_lower_weight() {
    // A -> B (weight 10), A -> C -> B (weight 1 + 1 = 2)
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to", 10.0))
        .unwrap();
    db.create_edge(make_edge(a.id, c.id, "links_to", 1.0))
        .unwrap();
    db.create_edge(make_edge(c.id, b.id, "links_to", 1.0))
        .unwrap();

    let path = db.shortest_path(a.id, b.id).unwrap();
    assert_eq!(path, Some(vec![a.id, c.id, b.id]));
}

#[test]
fn shortest_path_diamond_lower_path() {
    // A -> B (1), A -> C (1), B -> D (1), C -> D (10)
    // Shortest: A -> B -> D (cost 2) vs A -> C -> D (cost 11)
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    let d = db.create_node(make_node("note", "D")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to", 1.0))
        .unwrap();
    db.create_edge(make_edge(a.id, c.id, "links_to", 1.0))
        .unwrap();
    db.create_edge(make_edge(b.id, d.id, "links_to", 1.0))
        .unwrap();
    db.create_edge(make_edge(c.id, d.id, "links_to", 10.0))
        .unwrap();

    let path = db.shortest_path(a.id, d.id).unwrap();
    assert_eq!(path, Some(vec![a.id, b.id, d.id]));
}

#[test]
fn shortest_path_equal_weights_finds_a_path() {
    // Multiple paths with equal weight — any valid shortest path is acceptable
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    let d = db.create_node(make_node("note", "D")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to", 1.0))
        .unwrap();
    db.create_edge(make_edge(a.id, c.id, "links_to", 1.0))
        .unwrap();
    db.create_edge(make_edge(b.id, d.id, "links_to", 1.0))
        .unwrap();
    db.create_edge(make_edge(c.id, d.id, "links_to", 1.0))
        .unwrap();

    let path = db.shortest_path(a.id, d.id).unwrap();
    let path = path.unwrap();
    assert_eq!(path.len(), 3); // A -> ? -> D
    assert_eq!(path[0], a.id);
    assert_eq!(path[2], d.id);
    assert!(path[1] == b.id || path[1] == c.id);
}

// ---------------------------------------------------------------
// Cycles
// ---------------------------------------------------------------

#[test]
fn shortest_path_with_cycle() {
    // A -> B -> C -> A (cycle), B -> D
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    let d = db.create_node(make_node("note", "D")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to", 1.0))
        .unwrap();
    db.create_edge(make_edge(b.id, c.id, "links_to", 1.0))
        .unwrap();
    db.create_edge(make_edge(c.id, a.id, "links_to", 1.0))
        .unwrap();
    db.create_edge(make_edge(b.id, d.id, "links_to", 1.0))
        .unwrap();

    let path = db.shortest_path(a.id, d.id).unwrap();
    assert_eq!(path, Some(vec![a.id, b.id, d.id]));
}

#[test]
fn shortest_path_self_loop_ignored() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, a.id, "self_ref", 0.1))
        .unwrap();
    db.create_edge(make_edge(a.id, b.id, "links_to", 1.0))
        .unwrap();

    let path = db.shortest_path(a.id, b.id).unwrap();
    assert_eq!(path, Some(vec![a.id, b.id]));
}

// ---------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------

#[test]
fn shortest_path_nonexistent_source() {
    let db = Drevo::open_in_memory().unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();

    let path = db.shortest_path(999, b.id).unwrap();
    assert_eq!(path, None);
}

#[test]
fn shortest_path_nonexistent_target() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();

    let path = db.shortest_path(a.id, 999).unwrap();
    assert_eq!(path, None);
}

#[test]
fn shortest_path_long_chain() {
    let db = Drevo::open_in_memory().unwrap();
    let mut nodes = Vec::new();
    for i in 0..10 {
        nodes.push(
            db.create_node(make_node("note", &format!("N{}", i)))
                .unwrap(),
        );
    }
    for i in 0..9 {
        db.create_edge(make_edge(nodes[i].id, nodes[i + 1].id, "links_to", 1.0))
            .unwrap();
    }

    let path = db.shortest_path(nodes[0].id, nodes[9].id).unwrap();
    let expected: Vec<u64> = nodes.iter().map(|n| n.id).collect();
    assert_eq!(path, Some(expected));
}

#[test]
fn shortest_path_long_chain_vs_shortcut() {
    // Chain: N0 -> N1 -> N2 -> ... -> N9 (each weight 1.0)
    // Shortcut: N0 -> N9 (weight 5.0)
    // Chain cost = 9, shortcut cost = 5 => shortcut wins
    let db = Drevo::open_in_memory().unwrap();
    let mut nodes = Vec::new();
    for i in 0..10 {
        nodes.push(
            db.create_node(make_node("note", &format!("N{}", i)))
                .unwrap(),
        );
    }
    for i in 0..9 {
        db.create_edge(make_edge(nodes[i].id, nodes[i + 1].id, "links_to", 1.0))
            .unwrap();
    }
    db.create_edge(make_edge(nodes[0].id, nodes[9].id, "links_to", 5.0))
        .unwrap();

    let path = db.shortest_path(nodes[0].id, nodes[9].id).unwrap();
    assert_eq!(path, Some(vec![nodes[0].id, nodes[9].id]));
}

// ---------------------------------------------------------------
// Use-case scenarios
// ---------------------------------------------------------------

#[test]
fn scenario_cbt_journal_thought_chain() {
    // Trace shortest path from a trigger situation to a rational response
    let db = Drevo::open_in_memory().unwrap();
    let situation = db
        .create_node(make_node("situation", "Argument with coworker"))
        .unwrap();
    let thought = db
        .create_node(make_node("thought", "They hate me"))
        .unwrap();
    let distortion = db
        .create_node(make_node("distortion", "Mind reading"))
        .unwrap();
    let response = db
        .create_node(make_node("response", "I cannot read minds"))
        .unwrap();

    db.create_edge(make_edge(situation.id, thought.id, "triggered", 1.0))
        .unwrap();
    db.create_edge(make_edge(thought.id, distortion.id, "contains", 1.0))
        .unwrap();
    db.create_edge(make_edge(distortion.id, response.id, "reframed_as", 1.0))
        .unwrap();

    let path = db.shortest_path(situation.id, response.id).unwrap();
    assert_eq!(
        path,
        Some(vec![situation.id, thought.id, distortion.id, response.id])
    );
}

#[test]
fn scenario_story_editor_narrative_path() {
    // Shortest path from Chapter 1 to the climax scene
    let db = Drevo::open_in_memory().unwrap();
    let ch1 = db.create_node(make_node("chapter", "Chapter 1")).unwrap();
    let scene_a = db.create_node(make_node("scene", "Opening")).unwrap();
    let scene_b = db
        .create_node(make_node("scene", "Rising tension"))
        .unwrap();
    let climax = db.create_node(make_node("scene", "Climax")).unwrap();

    db.create_edge(make_edge(ch1.id, scene_a.id, "contains", 1.0))
        .unwrap();
    db.create_edge(make_edge(scene_a.id, scene_b.id, "follows", 2.0))
        .unwrap();
    db.create_edge(make_edge(scene_b.id, climax.id, "follows", 3.0))
        .unwrap();
    // Direct but expensive shortcut
    db.create_edge(make_edge(ch1.id, climax.id, "contains", 10.0))
        .unwrap();

    let path = db.shortest_path(ch1.id, climax.id).unwrap();
    // Via scenes: 1 + 2 + 3 = 6, direct: 10 => via scenes wins
    assert_eq!(path, Some(vec![ch1.id, scene_a.id, scene_b.id, climax.id]));
}

#[test]
fn scenario_task_manager_dependency_chain() {
    // Find shortest dependency path from a blocked task to the root blocker
    let db = Drevo::open_in_memory().unwrap();
    let deploy = db.create_node(make_node("task", "Deploy to prod")).unwrap();
    let test = db
        .create_node(make_node("task", "Run integration tests"))
        .unwrap();
    let fix_bug = db.create_node(make_node("task", "Fix auth bug")).unwrap();
    let review = db.create_node(make_node("task", "Code review")).unwrap();

    db.create_edge(make_edge(deploy.id, test.id, "depends_on", 1.0))
        .unwrap();
    db.create_edge(make_edge(test.id, fix_bug.id, "depends_on", 1.0))
        .unwrap();
    db.create_edge(make_edge(fix_bug.id, review.id, "depends_on", 1.0))
        .unwrap();

    let path = db.shortest_path(deploy.id, review.id).unwrap();
    assert_eq!(path, Some(vec![deploy.id, test.id, fix_bug.id, review.id]));
}

#[test]
fn scenario_erp_order_to_warehouse() {
    // Find cheapest logistics path from order to warehouse
    let db = Drevo::open_in_memory().unwrap();
    let order = db.create_node(make_node("order", "Order #1001")).unwrap();
    let product = db.create_node(make_node("product", "Widget A")).unwrap();
    let wh_local = db.create_node(make_node("warehouse", "Local WH")).unwrap();
    let wh_central = db
        .create_node(make_node("warehouse", "Central WH"))
        .unwrap();

    db.create_edge(make_edge(order.id, product.id, "contains", 1.0))
        .unwrap();
    db.create_edge(make_edge(product.id, wh_local.id, "stored_in", 2.0))
        .unwrap();
    db.create_edge(make_edge(product.id, wh_central.id, "stored_in", 5.0))
        .unwrap();

    let path = db.shortest_path(order.id, wh_local.id).unwrap();
    assert_eq!(path, Some(vec![order.id, product.id, wh_local.id]));
}

#[test]
fn scenario_bug_tracker_impact_path() {
    // Find shortest impact path from a bug to a release
    let db = Drevo::open_in_memory().unwrap();
    let bug = db.create_node(make_node("bug", "Login crash")).unwrap();
    let feature = db.create_node(make_node("feature", "Auth module")).unwrap();
    let release = db.create_node(make_node("release", "v2.0")).unwrap();

    db.create_edge(make_edge(bug.id, feature.id, "affects", 1.0))
        .unwrap();
    db.create_edge(make_edge(feature.id, release.id, "blocks_release", 1.0))
        .unwrap();

    let path = db.shortest_path(bug.id, release.id).unwrap();
    assert_eq!(path, Some(vec![bug.id, feature.id, release.id]));
}
