//! Integration tests for the built-in global graph algorithms — Phase 15 task
//! `00098`. Drives [`drevo::db::Drevo::pagerank`] and
//! [`drevo::db::Drevo::louvain_communities`] against a real storage-backed
//! graph, plus realistic use-case scenarios (task-manager dependency ranking,
//! ERP org-chart community detection, bug-tracker triage clusters).

use drevo::algorithms::{LouvainConfig, PageRankConfig};
use drevo::db::Drevo;
use drevo::model::{NewEdge, NewNode, Properties};

fn node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn edge(from: u64, to: u64, kind: &str, weight: f32) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.to_string(),
        weight,
        properties: Properties::default(),
    }
}

fn rank_of(r: &drevo::algorithms::PageRankResult, id: u64) -> f64 {
    r.ranks.iter().find(|&&(i, _)| i == id).unwrap().1
}

fn community_of(r: &drevo::algorithms::LouvainResult, id: u64) -> usize {
    r.communities.iter().find(|&&(i, _)| i == id).unwrap().1
}

// ---------------------------------------------------------------
// PageRank
// ---------------------------------------------------------------

#[test]
fn pagerank_empty_graph_is_empty() {
    let db = Drevo::open_in_memory().unwrap();
    let r = db.pagerank(&PageRankConfig::default()).unwrap();
    assert!(r.ranks.is_empty());
    assert!(r.converged);
}

#[test]
fn pagerank_single_node_has_full_rank() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(node("note", "solo")).unwrap();
    let r = db.pagerank(&PageRankConfig::default()).unwrap();
    assert!((rank_of(&r, a.id) - 1.0).abs() < 1e-9);
}

#[test]
fn pagerank_ranks_sum_to_one_over_real_storage() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(node("page", "A")).unwrap();
    let b = db.create_node(node("page", "B")).unwrap();
    let c = db.create_node(node("page", "C")).unwrap();
    db.create_edge(edge(a.id, b.id, "links", 1.0)).unwrap();
    db.create_edge(edge(b.id, c.id, "links", 1.0)).unwrap();
    db.create_edge(edge(c.id, a.id, "links", 1.0)).unwrap();

    let r = db.pagerank(&PageRankConfig::default()).unwrap();
    let total: f64 = r.ranks.iter().map(|&(_, v)| v).sum();
    assert!((total - 1.0).abs() < 1e-9, "sum was {total}");
}

#[test]
fn pagerank_task_manager_ranks_most_depended_on_task_highest() {
    // Task-manager scenario: many tasks all "depend_on" a foundational task.
    // PageRank over the dependency edges should surface that task as the most
    // central one — the natural "what unblocks the most work?" query.
    let db = Drevo::open_in_memory().unwrap();
    let foundation = db
        .create_node(node("task", "Set up database schema"))
        .unwrap();
    let ui = db.create_node(node("task", "Build UI")).unwrap();
    let api = db.create_node(node("task", "Build API")).unwrap();
    let auth = db.create_node(node("task", "Add auth")).unwrap();

    // Everything depends on the foundation.
    db.create_edge(edge(ui.id, foundation.id, "depends_on", 1.0))
        .unwrap();
    db.create_edge(edge(api.id, foundation.id, "depends_on", 1.0))
        .unwrap();
    db.create_edge(edge(auth.id, foundation.id, "depends_on", 1.0))
        .unwrap();
    // auth also depends on api.
    db.create_edge(edge(auth.id, api.id, "depends_on", 1.0))
        .unwrap();

    let r = db.pagerank(&PageRankConfig::default()).unwrap();
    let ranked = r.ranked();
    assert_eq!(ranked[0].0, foundation.id, "foundation should rank first");
    assert!(rank_of(&r, foundation.id) > rank_of(&r, ui.id));
}

#[test]
fn pagerank_edge_weight_changes_ranking() {
    let db = Drevo::open_in_memory().unwrap();
    let src = db.create_node(node("page", "src")).unwrap();
    let heavy = db.create_node(node("page", "heavy")).unwrap();
    let light = db.create_node(node("page", "light")).unwrap();
    db.create_edge(edge(src.id, heavy.id, "links", 9.0))
        .unwrap();
    db.create_edge(edge(src.id, light.id, "links", 1.0))
        .unwrap();

    let r = db.pagerank(&PageRankConfig::default()).unwrap();
    assert!(rank_of(&r, heavy.id) > rank_of(&r, light.id));
}

#[test]
fn pagerank_config_rejects_invalid_damping() {
    assert!(PageRankConfig::new(1.5, 100, 1e-6).is_err());
    assert!(PageRankConfig::new(0.85, 0, 1e-6).is_err());
}

// ---------------------------------------------------------------
// Louvain community detection
// ---------------------------------------------------------------

#[test]
fn louvain_empty_graph_is_empty() {
    let db = Drevo::open_in_memory().unwrap();
    let r = db.louvain_communities(&LouvainConfig::default()).unwrap();
    assert!(r.communities.is_empty());
    assert_eq!(r.modularity, 0.0);
}

#[test]
fn louvain_erp_org_chart_finds_departments() {
    // ERP scenario: two departments, each a tightly-knit team, with a single
    // cross-department liaison edge. Community detection should recover the
    // two departments.
    let db = Drevo::open_in_memory().unwrap();
    let eng_a = db.create_node(node("person", "Eng Alice")).unwrap();
    let eng_b = db.create_node(node("person", "Eng Bob")).unwrap();
    let eng_c = db.create_node(node("person", "Eng Carol")).unwrap();
    let sales_x = db.create_node(node("person", "Sales Xavier")).unwrap();
    let sales_y = db.create_node(node("person", "Sales Yvonne")).unwrap();
    let sales_z = db.create_node(node("person", "Sales Zane")).unwrap();

    // Engineering team — fully connected.
    db.create_edge(edge(eng_a.id, eng_b.id, "collaborates", 1.0))
        .unwrap();
    db.create_edge(edge(eng_b.id, eng_c.id, "collaborates", 1.0))
        .unwrap();
    db.create_edge(edge(eng_c.id, eng_a.id, "collaborates", 1.0))
        .unwrap();
    // Sales team — fully connected.
    db.create_edge(edge(sales_x.id, sales_y.id, "collaborates", 1.0))
        .unwrap();
    db.create_edge(edge(sales_y.id, sales_z.id, "collaborates", 1.0))
        .unwrap();
    db.create_edge(edge(sales_z.id, sales_x.id, "collaborates", 1.0))
        .unwrap();
    // Single liaison link between departments.
    db.create_edge(edge(eng_c.id, sales_x.id, "liaison", 1.0))
        .unwrap();

    let r = db.louvain_communities(&LouvainConfig::default()).unwrap();
    assert_eq!(r.community_count(), 2);
    // Engineering stays together.
    assert_eq!(community_of(&r, eng_a.id), community_of(&r, eng_b.id));
    assert_eq!(community_of(&r, eng_a.id), community_of(&r, eng_c.id));
    // Sales stays together.
    assert_eq!(community_of(&r, sales_x.id), community_of(&r, sales_y.id));
    assert_eq!(community_of(&r, sales_x.id), community_of(&r, sales_z.id));
    // The two departments are distinct.
    assert_ne!(community_of(&r, eng_a.id), community_of(&r, sales_x.id));
    assert!(r.modularity > 0.3);
}

#[test]
fn louvain_disconnected_bug_clusters_are_separate() {
    // Bug-tracker scenario: two unrelated bug clusters (each a bug linked to
    // its duplicates) with no connection — must be separate communities.
    let db = Drevo::open_in_memory().unwrap();
    let bug1 = db.create_node(node("bug", "Login crash")).unwrap();
    let dup1 = db.create_node(node("bug", "Cannot log in")).unwrap();
    let bug2 = db.create_node(node("bug", "Export fails")).unwrap();
    let dup2 = db.create_node(node("bug", "CSV broken")).unwrap();
    db.create_edge(edge(dup1.id, bug1.id, "duplicate_of", 1.0))
        .unwrap();
    db.create_edge(edge(dup2.id, bug2.id, "duplicate_of", 1.0))
        .unwrap();

    let r = db.louvain_communities(&LouvainConfig::default()).unwrap();
    assert_eq!(r.community_count(), 2);
    assert_eq!(community_of(&r, bug1.id), community_of(&r, dup1.id));
    assert_eq!(community_of(&r, bug2.id), community_of(&r, dup2.id));
    assert_ne!(community_of(&r, bug1.id), community_of(&r, bug2.id));
}

#[test]
fn louvain_communities_cover_every_node_exactly_once() {
    let db = Drevo::open_in_memory().unwrap();
    let mut ids = Vec::new();
    for i in 0..6 {
        ids.push(db.create_node(node("n", &format!("n{i}"))).unwrap().id);
    }
    db.create_edge(edge(ids[0], ids[1], "e", 1.0)).unwrap();
    db.create_edge(edge(ids[2], ids[3], "e", 1.0)).unwrap();
    db.create_edge(edge(ids[4], ids[5], "e", 1.0)).unwrap();

    let r = db.louvain_communities(&LouvainConfig::default()).unwrap();
    assert_eq!(r.communities.len(), 6);
    let returned: Vec<u64> = r.communities.iter().map(|&(id, _)| id).collect();
    let mut sorted_ids = ids.clone();
    sorted_ids.sort_unstable();
    assert_eq!(returned, sorted_ids);
}

#[test]
fn louvain_is_deterministic_over_storage() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(node("n", "a")).unwrap();
    let b = db.create_node(node("n", "b")).unwrap();
    let c = db.create_node(node("n", "c")).unwrap();
    let d = db.create_node(node("n", "d")).unwrap();
    db.create_edge(edge(a.id, b.id, "e", 1.0)).unwrap();
    db.create_edge(edge(c.id, d.id, "e", 1.0)).unwrap();

    let r1 = db.louvain_communities(&LouvainConfig::default()).unwrap();
    let r2 = db.louvain_communities(&LouvainConfig::default()).unwrap();
    assert_eq!(r1, r2);
}

#[test]
fn louvain_config_rejects_invalid_resolution() {
    assert!(LouvainConfig::new(-0.5, 50).is_err());
    assert!(LouvainConfig::new(1.0, 0).is_err());
    assert!(LouvainConfig::new(1.0, 50).is_ok());
}
