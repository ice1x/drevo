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
fn search_fts_finds_text_in_a_non_title_body_property() {
    // #227: text stored under a property key other than `title`/`body` must be
    // BM25-searchable (not just reachable via a slow CONTAINS scan).
    let db = db();
    let mut props = std::collections::HashMap::new();
    props.insert(
        "name".to_string(),
        serde_json::json!("Zebra crossing paths"),
    );
    db.create_node(NewNode {
        kind: "thing".to_string(),
        title: String::new(),
        body: String::new(),
        body_html: String::new(),
        properties: props.into(),
    })
    .unwrap();
    // A distractor with no matching text anywhere.
    db.create_node(new_node("note", "Quarterly revenue report", ""))
        .unwrap();

    let results = db.search_fts("zebra", 10).unwrap();
    assert_eq!(
        results.len(),
        1,
        "the zebra node must be found by its `name` property"
    );
    assert!(results[0].score > 0.0);
}

#[test]
fn search_fts_relationships_finds_and_reindexes_edges() {
    // #227-B: relationship string properties (e.g. `fact`) are BM25-searchable
    // via search_fts_relationships, and update/delete keep the index in sync.
    let db = db();
    let a = db.create_node(new_node("n", "A", "")).unwrap();
    let b = db.create_node(new_node("n", "B", "")).unwrap();

    let mut props = std::collections::HashMap::new();
    props.insert(
        "fact".to_string(),
        serde_json::json!("acquired wolverine corp"),
    );
    let edge = db
        .create_edge(drevo::model::NewEdge {
            from_id: a.id,
            to_id: b.id,
            kind: "relates_to".to_string(),
            weight: 1.0,
            properties: props.into(),
        })
        .unwrap();

    let hits = db.search_fts_relationships("wolverine", 10).unwrap();
    assert_eq!(hits.len(), 1, "edge found by its `fact` property");
    assert!(hits[0].score > 0.0);
    assert_eq!(hits[0].edge.id, edge.id);

    // Update the property → old term gone, new term found.
    let mut new_props = std::collections::HashMap::new();
    new_props.insert("fact".to_string(), serde_json::json!("acquired badger inc"));
    db.update_edge(
        edge.id,
        drevo::model::EdgePatch {
            properties: Some(new_props.into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(db
        .search_fts_relationships("wolverine", 10)
        .unwrap()
        .is_empty());
    assert_eq!(db.search_fts_relationships("badger", 10).unwrap().len(), 1);

    // Delete → gone from the edge index.
    db.delete_edge(edge.id).unwrap();
    assert!(db
        .search_fts_relationships("badger", 10)
        .unwrap()
        .is_empty());
}

#[test]
fn search_fts_reindexes_on_property_only_change() {
    // A change to an indexed property (with title/body untouched) must
    // re-index: the old term disappears from search, the new one appears.
    let db = db();
    let mut props = std::collections::HashMap::new();
    props.insert("name".to_string(), serde_json::json!("alphaword marker"));
    let node = db
        .create_node(NewNode {
            kind: "thing".to_string(),
            title: String::new(),
            body: String::new(),
            body_html: String::new(),
            properties: props.into(),
        })
        .unwrap();
    assert_eq!(db.search_fts("alphaword", 10).unwrap().len(), 1);

    let mut new_props = std::collections::HashMap::new();
    new_props.insert("name".to_string(), serde_json::json!("bravoword marker"));
    db.update_node(
        node.id,
        drevo::model::NodePatch {
            properties: Some(new_props.into()),
            ..Default::default()
        },
    )
    .unwrap();

    assert!(
        db.search_fts("alphaword", 10).unwrap().is_empty(),
        "the old property term must be de-indexed"
    );
    assert_eq!(
        db.search_fts("bravoword", 10).unwrap().len(),
        1,
        "the new property term must be indexed"
    );
}

#[test]
fn search_fts_finds_text_in_array_property_elements() {
    // Array-of-string property values (e.g. a KG entity's `observations`) are
    // indexed element-by-element.
    let db = db();
    let mut props = std::collections::HashMap::new();
    props.insert(
        "observations".to_string(),
        serde_json::json!(["prefers wolverine over badger"]),
    );
    db.create_node(NewNode {
        kind: "entity".to_string(),
        title: String::new(),
        body: String::new(),
        body_html: String::new(),
        properties: props.into(),
    })
    .unwrap();

    let results = db.search_fts("wolverine", 10).unwrap();
    assert_eq!(results.len(), 1);
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
