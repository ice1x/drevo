//! Integration tests for FTS index: trigram -> posting list storage.
//!
//! Tests that the FTS index is maintained during node CRUD operations
//! and that posting list queries return correct results.

use graphnote_db::db::GraphNoteDb;
use graphnote_db::model::NewNode;

/// Helper to create a node with given title and body.
fn make_node(title: &str, body: &str) -> NewNode {
    NewNode {
        kind: "note".to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: String::new(),
        properties: Default::default(),
    }
}

// ---------------------------------------------------------------
// Basic index maintenance
// ---------------------------------------------------------------

#[test]
fn fts_index_created_on_node_create() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let node = db.create_node(make_node("Hello World", "")).unwrap();

    // "hello world" -> trigrams include "hel", "ell", "llo", etc.
    let results = db.fts_node_ids_for_trigram("hel").unwrap();
    assert!(
        results.contains(&node.id),
        "node should be in 'hel' posting list"
    );

    let results = db.fts_node_ids_for_trigram("wor").unwrap();
    assert!(
        results.contains(&node.id),
        "node should be in 'wor' posting list"
    );
}

#[test]
fn fts_index_includes_body() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let node = db
        .create_node(make_node("Title", "This is the body text"))
        .unwrap();

    let results = db.fts_node_ids_for_trigram("bod").unwrap();
    assert!(
        results.contains(&node.id),
        "body text trigrams should be indexed"
    );
}

#[test]
fn fts_index_multiple_nodes_same_trigram() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let n1 = db.create_node(make_node("Hello Alice", "")).unwrap();
    let n2 = db.create_node(make_node("Hello Bob", "")).unwrap();

    let results = db.fts_node_ids_for_trigram("hel").unwrap();
    assert!(results.contains(&n1.id));
    assert!(results.contains(&n2.id));
}

#[test]
fn fts_index_removed_on_node_delete() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let node = db.create_node(make_node("Hello World", "")).unwrap();
    let node_id = node.id;

    db.delete_node(node_id).unwrap();

    let results = db.fts_node_ids_for_trigram("hel").unwrap();
    assert!(
        !results.contains(&node_id),
        "deleted node should be removed from FTS index"
    );
}

#[test]
fn fts_index_updated_on_node_update_title() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let node = db.create_node(make_node("Hello World", "")).unwrap();
    let node_id = node.id;

    // Update title from "Hello World" to "Goodbye World"
    let patch = graphnote_db::model::NodePatch {
        title: Some("Goodbye World".to_string()),
        ..Default::default()
    };
    db.update_node(node_id, patch).unwrap();

    // Old trigrams should be gone
    let results = db.fts_node_ids_for_trigram("hel").unwrap();
    assert!(
        !results.contains(&node_id),
        "old title trigrams should be removed"
    );

    // New trigrams should be present
    let results = db.fts_node_ids_for_trigram("goo").unwrap();
    assert!(
        results.contains(&node_id),
        "new title trigrams should be indexed"
    );
}

#[test]
fn fts_index_updated_on_node_update_body() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let node = db.create_node(make_node("Title", "original body")).unwrap();
    let node_id = node.id;

    let patch = graphnote_db::model::NodePatch {
        body: Some("updated content".to_string()),
        ..Default::default()
    };
    db.update_node(node_id, patch).unwrap();

    // Old body trigrams should be gone
    let results = db.fts_node_ids_for_trigram("ori").unwrap();
    assert!(
        !results.contains(&node_id),
        "old body trigrams should be removed"
    );

    // New body trigrams should be present
    let results = db.fts_node_ids_for_trigram("upd").unwrap();
    assert!(
        results.contains(&node_id),
        "new body trigrams should be indexed"
    );
}

#[test]
fn fts_index_unchanged_fields_not_reindexed_unnecessarily() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let node = db
        .create_node(make_node("Hello World", "some body"))
        .unwrap();
    let node_id = node.id;

    // Update only kind — title/body unchanged, FTS index should survive
    let patch = graphnote_db::model::NodePatch {
        kind: Some("article".to_string()),
        ..Default::default()
    };
    db.update_node(node_id, patch).unwrap();

    let results = db.fts_node_ids_for_trigram("hel").unwrap();
    assert!(
        results.contains(&node_id),
        "FTS index should remain when title/body unchanged"
    );
}

// ---------------------------------------------------------------
// Posting list intersection (search)
// ---------------------------------------------------------------

#[test]
fn fts_search_single_trigram() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let n1 = db.create_node(make_node("Hello", "")).unwrap();
    let _n2 = db.create_node(make_node("World", "")).unwrap();

    let results = db.fts_node_ids_for_trigram("hel").unwrap();
    assert_eq!(results, vec![n1.id]);
}

#[test]
fn fts_search_intersect_trigrams() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let n1 = db.create_node(make_node("Rust programming", "")).unwrap();
    let _n2 = db.create_node(make_node("Rust language", "")).unwrap();
    let _n3 = db.create_node(make_node("Python programming", "")).unwrap();

    // Searching for "rust prog" should match only n1
    // trigrams of "rust prog": "rus", "ust", "st ", "t p", " pr", "pro", "rog"
    let ids = db
        .fts_intersect_trigrams(&["rus".to_string(), "pro".to_string()])
        .unwrap();
    assert!(ids.contains(&n1.id), "n1 matches both trigrams");
    // n2 has "rus" but not "pro"; n3 has "pro" but not "rus"
    assert_eq!(ids.len(), 1);
}

#[test]
fn fts_intersect_empty_trigrams_returns_empty() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let _n1 = db.create_node(make_node("Hello", "")).unwrap();

    let ids = db.fts_intersect_trigrams(&[]).unwrap();
    assert!(ids.is_empty());
}

#[test]
fn fts_intersect_no_match_returns_empty() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let _n1 = db.create_node(make_node("Hello", "")).unwrap();

    let ids = db.fts_intersect_trigrams(&["zzz".to_string()]).unwrap();
    assert!(ids.is_empty());
}

// ---------------------------------------------------------------
// CJK support
// ---------------------------------------------------------------

#[test]
fn fts_index_cjk_bigrams() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let node = db.create_node(make_node("你好世界", "")).unwrap();

    let results = db.fts_node_ids_for_trigram("你好").unwrap();
    assert!(results.contains(&node.id), "CJK bigram should be indexed");

    let results = db.fts_node_ids_for_trigram("世界").unwrap();
    assert!(results.contains(&node.id));
}

// ---------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------

#[test]
fn fts_index_empty_title_and_body() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    // Node with very short title, no body — no trigrams produced
    let _node = db.create_node(make_node("Hi", "")).unwrap();
    // Should not crash, just no FTS entries
}

#[test]
fn fts_index_survives_delete_of_other_node() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let n1 = db.create_node(make_node("Hello Alice", "")).unwrap();
    let n2 = db.create_node(make_node("Hello Bob", "")).unwrap();

    db.delete_node(n1.id).unwrap();

    // n2 should still be in the index
    let results = db.fts_node_ids_for_trigram("hel").unwrap();
    assert!(results.contains(&n2.id));
    assert!(!results.contains(&n1.id));
}

// ---------------------------------------------------------------
// Use-case integration: CBT Journal
// ---------------------------------------------------------------

#[test]
fn fts_index_cbt_journal_scenario() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    let thought = db
        .create_node(make_node(
            "Catastrophizing about work",
            "I think I will be fired because I made one mistake",
        ))
        .unwrap();
    let response = db
        .create_node(make_node(
            "Rational response",
            "One mistake does not define my career",
        ))
        .unwrap();

    // Search for "mistake" trigrams
    let ids = db
        .fts_intersect_trigrams(&["mis".to_string(), "ist".to_string(), "sta".to_string()])
        .unwrap();
    assert!(ids.contains(&thought.id));
    assert!(ids.contains(&response.id));
}

// ---------------------------------------------------------------
// Use-case integration: Story Editor
// ---------------------------------------------------------------

#[test]
fn fts_index_story_editor_scenario() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    let chapter = db
        .create_node(make_node(
            "Chapter 1: The Beginning",
            "It was a dark and stormy night",
        ))
        .unwrap();
    let _char = db
        .create_node(make_node("Character: Alice", "Brave explorer"))
        .unwrap();

    // Search for "stormy" -> trigrams "sto", "tor", "orm", "rmy"
    let ids = db
        .fts_intersect_trigrams(&["sto".to_string(), "tor".to_string(), "orm".to_string()])
        .unwrap();
    assert_eq!(ids, vec![chapter.id]);
}

// ---------------------------------------------------------------
// Use-case integration: Bug Tracker
// ---------------------------------------------------------------

#[test]
fn fts_index_bug_tracker_scenario() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    let bug = db
        .create_node(make_node(
            "Login page crash on Safari",
            "NullPointerException when clicking submit button on Safari 17",
        ))
        .unwrap();
    let _feature = db
        .create_node(make_node(
            "Add dark mode toggle",
            "Users want a toggle in settings",
        ))
        .unwrap();

    // Search for "safari" -> trigrams "saf", "afa", "far", "ari"
    let ids = db
        .fts_intersect_trigrams(&["saf".to_string(), "afa".to_string(), "far".to_string()])
        .unwrap();
    assert_eq!(ids, vec![bug.id]);
}
