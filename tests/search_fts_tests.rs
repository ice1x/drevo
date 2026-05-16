//! Integration tests for `search_fts` with TF-IDF ranking.

use drevo::db::Drevo;
use drevo::model::NewNode;

fn db() -> Drevo {
    Drevo::open_in_memory().unwrap()
}

fn new_node(kind: &str, title: &str, body: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: String::new(),
        properties: Default::default(),
    }
}

// ---------------------------------------------------------------
// Basic search
// ---------------------------------------------------------------

#[test]
fn search_fts_empty_query_returns_empty() {
    let db = db();
    db.create_node(new_node("note", "Hello World", "some body"))
        .unwrap();
    let results = db.search_fts("", 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn search_fts_short_query_returns_empty() {
    let db = db();
    db.create_node(new_node("note", "Hi", "")).unwrap();
    // "hi" is too short for trigrams
    let results = db.search_fts("hi", 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn search_fts_single_match() {
    let db = db();
    db.create_node(new_node("note", "Rust programming language", ""))
        .unwrap();
    db.create_node(new_node("note", "Python scripting", ""))
        .unwrap();

    let results = db.search_fts("rust", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].node.title, "Rust programming language");
    assert!(results[0].score > 0.0);
}

#[test]
fn search_fts_multiple_matches_ranked() {
    let db = db();
    // Node 1: "rust" in title only
    db.create_node(new_node("note", "Rust", "")).unwrap();
    // Node 2: "rust" in title and body — should rank higher (more trigram hits)
    db.create_node(new_node(
        "note",
        "Rust programming",
        "Rust is a systems programming language focusing on safety and rust compiler",
    ))
    .unwrap();
    // Node 3: no match
    db.create_node(new_node("note", "Python", "dynamic typing"))
        .unwrap();

    let results = db.search_fts("rust", 10).unwrap();
    assert!(!results.is_empty());
    // All results should contain "rust"-related trigrams
    for r in &results {
        let lower = format!("{} {}", r.node.title, r.node.body).to_lowercase();
        assert!(lower.contains("rust"));
    }
    // Higher-ranked result should have higher score
    if results.len() >= 2 {
        assert!(results[0].score >= results[1].score);
    }
}

#[test]
fn search_fts_respects_limit() {
    let db = db();
    for i in 0..20 {
        db.create_node(new_node(
            "note",
            &format!("Rust note {}", i),
            "rust content",
        ))
        .unwrap();
    }

    let results = db.search_fts("rust", 5).unwrap();
    assert_eq!(results.len(), 5);
}

#[test]
fn search_fts_limit_zero_returns_empty() {
    let db = db();
    db.create_node(new_node("note", "Rust programming", ""))
        .unwrap();

    let results = db.search_fts("rust", 0).unwrap();
    assert!(results.is_empty());
}

#[test]
fn search_fts_no_matches() {
    let db = db();
    db.create_node(new_node("note", "Hello World", "")).unwrap();

    let results = db.search_fts("zzzzz", 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn search_fts_multi_word_query() {
    let db = db();
    db.create_node(new_node(
        "note",
        "Rust programming language",
        "systems level",
    ))
    .unwrap();
    db.create_node(new_node("note", "Rust compiler", ""))
        .unwrap();
    db.create_node(new_node("note", "Programming in Python", ""))
        .unwrap();

    // "rust programming" should match nodes containing both terms
    let results = db.search_fts("rust programming", 10).unwrap();
    assert!(!results.is_empty());
    // The node with both "rust" and "programming" should rank first
    assert_eq!(results[0].node.title, "Rust programming language");
}

#[test]
fn search_fts_after_update() {
    let db = db();
    let node = db.create_node(new_node("note", "Rust", "")).unwrap();

    let results = db.search_fts("python", 10).unwrap();
    assert!(results.is_empty());

    db.update_node(
        node.id,
        drevo::model::NodePatch {
            title: Some("Python".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let results = db.search_fts("python", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].node.title, "Python");
}

#[test]
fn search_fts_after_delete() {
    let db = db();
    let node = db
        .create_node(new_node("note", "Rust programming", ""))
        .unwrap();

    let results = db.search_fts("rust", 10).unwrap();
    assert_eq!(results.len(), 1);

    db.delete_node(node.id).unwrap();

    let results = db.search_fts("rust", 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn search_fts_cjk_query() {
    let db = db();
    db.create_node(new_node("note", "你好世界", "中文内容"))
        .unwrap();
    db.create_node(new_node("note", "Hello World", "")).unwrap();

    let results = db.search_fts("你好", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].node.title, "你好世界");
}

#[test]
fn search_fts_case_insensitive() {
    let db = db();
    db.create_node(new_node("note", "RUST Programming", ""))
        .unwrap();

    let results = db.search_fts("rust", 10).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn search_fts_scored_node_has_positive_score() {
    let db = db();
    db.create_node(new_node("note", "Rust language", ""))
        .unwrap();

    let results = db.search_fts("rust", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].score > 0.0, "score must be positive");
}

// ---------------------------------------------------------------
// TF-IDF ranking quality
// ---------------------------------------------------------------

#[test]
fn tfidf_rare_term_scores_higher() {
    let db = db();
    // Create many nodes with "common" but only one with "rare"
    for i in 0..10 {
        db.create_node(new_node(
            "note",
            &format!("Common topic {}", i),
            "common text content",
        ))
        .unwrap();
    }
    db.create_node(new_node("note", "Unique artifact", "rare special content"))
        .unwrap();

    // Searching for "rare" — the unique node should appear
    let results = db.search_fts("rare special", 10).unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].node.title, "Unique artifact");
}

#[test]
fn search_fts_body_matches() {
    let db = db();
    db.create_node(new_node("note", "Untitled", "the quick brown fox jumps"))
        .unwrap();

    let results = db.search_fts("quick brown", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].node.title, "Untitled");
}

// ---------------------------------------------------------------
// Use-case scenarios
// ---------------------------------------------------------------

#[test]
fn scenario_cbt_journal_search() {
    let db = db();
    db.create_node(new_node(
        "thought",
        "Catastrophizing about work deadline",
        "I will fail and lose my job",
    ))
    .unwrap();
    db.create_node(new_node(
        "thought",
        "Positive morning reflection",
        "Today will be a good day",
    ))
    .unwrap();
    db.create_node(new_node(
        "distortion",
        "Catastrophizing",
        "Expecting the worst outcome",
    ))
    .unwrap();

    let results = db.search_fts("catastrophizing", 10).unwrap();
    assert!(results.len() >= 2);
}

#[test]
fn scenario_bug_tracker_search() {
    let db = db();
    db.create_node(new_node(
        "bug",
        "NullPointerException in login flow",
        "User reports crash on login",
    ))
    .unwrap();
    db.create_node(new_node(
        "bug",
        "Memory leak in dashboard",
        "Heap grows unbounded after 24h",
    ))
    .unwrap();
    db.create_node(new_node("feature", "Add dark mode", "UI enhancement"))
        .unwrap();

    let results = db.search_fts("login", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].node.title, "NullPointerException in login flow");
}

#[test]
fn scenario_story_editor_search() {
    let db = db();
    db.create_node(new_node(
        "chapter",
        "The Beginning",
        "Our hero sets out on a journey through the forest",
    ))
    .unwrap();
    db.create_node(new_node(
        "chapter",
        "The Dark Forest",
        "Deep in the forest the trees block all light",
    ))
    .unwrap();
    db.create_node(new_node(
        "character",
        "The Forest Guardian",
        "Ancient protector of the woodland",
    ))
    .unwrap();

    let results = db.search_fts("forest", 10).unwrap();
    assert!(results.len() >= 2);
}

#[test]
fn search_fts_empty_db() {
    let db = db();
    let results = db.search_fts("anything", 10).unwrap();
    assert!(results.is_empty());
}
