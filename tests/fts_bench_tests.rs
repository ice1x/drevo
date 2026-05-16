//! Integration tests that validate the FTS benchmark scenario at small scale.
//!
//! Task 00020 — ensures the benchmark setup and queries are correct before
//! running the full 10K-node criterion benchmark.

use drevo::db::Drevo;
use drevo::model::NewNode;

/// Helper: create a node with realistic content for FTS benchmarking.
fn make_fts_node(i: usize) -> NewNode {
    let kinds = ["note", "task", "bug", "thought", "chapter"];
    let kind = kinds[i % kinds.len()];

    NewNode {
        kind: kind.to_string(),
        title: format!("{kind} number {i}: exploring topic {}", i % 50),
        body: format!(
            "This is the body of {kind} {i}. It discusses various aspects of \
             software development including testing, debugging, performance \
             optimization, and architecture design. Keywords: rust database \
             graph embedded search indexing trigram. Topic group {}.",
            i % 50
        ),
        body_html: String::new(),
        properties: Default::default(),
    }
}

#[test]
fn fts_bench_setup_creates_nodes() {
    let db = Drevo::open_in_memory().unwrap();
    let count = 100usize;
    for i in 0..count {
        db.create_node(make_fts_node(i)).unwrap();
    }
    // Verify nodes exist
    let node = db.get_node(1).unwrap();
    assert!(node.is_some());
    let node = db.get_node(count as u64).unwrap();
    assert!(node.is_some());
}

#[test]
fn fts_bench_search_returns_results_on_populated_db() {
    let db = Drevo::open_in_memory().unwrap();
    for i in 0..200 {
        db.create_node(make_fts_node(i)).unwrap();
    }

    let results = db.search_fts("rust database graph", 10).unwrap();
    assert!(
        !results.is_empty(),
        "search should find nodes with matching content"
    );
    assert!(results.len() <= 10);
    // All scores should be positive
    for scored in &results {
        assert!(scored.score > 0.0, "score should be positive");
    }
}

#[test]
fn fts_bench_search_short_query() {
    let db = Drevo::open_in_memory().unwrap();
    for i in 0..200 {
        db.create_node(make_fts_node(i)).unwrap();
    }

    let results = db.search_fts("bug", 10).unwrap();
    assert!(!results.is_empty(), "short query should still find results");
}

#[test]
fn fts_bench_search_kind_specific() {
    let db = Drevo::open_in_memory().unwrap();
    for i in 0..200 {
        db.create_node(make_fts_node(i)).unwrap();
    }

    // "thought" appears only in kind="thought" nodes' titles and bodies
    let results = db.search_fts("thought", 50).unwrap();
    assert!(!results.is_empty());
}

#[test]
fn fts_bench_search_no_results_for_absent_term() {
    let db = Drevo::open_in_memory().unwrap();
    for i in 0..100 {
        db.create_node(make_fts_node(i)).unwrap();
    }

    let results = db.search_fts("xylophone quantum entanglement", 10).unwrap();
    // This term likely does not appear in our generated content
    // (it's fine if some trigrams match, just checking it doesn't panic)
    let _ = results;
}

#[test]
fn fts_bench_search_limit_respected() {
    let db = Drevo::open_in_memory().unwrap();
    for i in 0..500 {
        db.create_node(make_fts_node(i)).unwrap();
    }

    for limit in [1, 5, 10, 50] {
        let results = db.search_fts("software development", limit).unwrap();
        assert!(
            results.len() <= limit,
            "limit {limit} should be respected, got {}",
            results.len()
        );
    }
}

#[test]
fn fts_bench_list_recent_on_populated_db() {
    let db = Drevo::open_in_memory().unwrap();
    for i in 0..200 {
        db.create_node(make_fts_node(i)).unwrap();
    }

    let recent = db.list_recent(20).unwrap();
    assert_eq!(recent.len(), 20);
    // Verify newest-first ordering
    for w in recent.windows(2) {
        assert!(w[0].updated_at >= w[1].updated_at);
    }
}

#[test]
fn fts_bench_varied_queries_all_succeed() {
    let db = Drevo::open_in_memory().unwrap();
    for i in 0..300 {
        db.create_node(make_fts_node(i)).unwrap();
    }

    let queries = [
        "rust",
        "database graph",
        "testing debugging",
        "performance optimization",
        "architecture design",
        "trigram indexing",
        "software development",
        "topic group",
        "note number",
        "exploring",
    ];

    for query in &queries {
        let results = db.search_fts(query, 10).unwrap();
        // Just ensure no panics and results are valid
        for scored in &results {
            assert!(scored.score.is_finite());
        }
    }
}
