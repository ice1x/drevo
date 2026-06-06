//! Integration tests for Okapi BM25 full-text ranking (task `00131`).
//!
//! These exercise the BM25-specific behaviours that plain TF-IDF lacks —
//! term-frequency saturation (`k1`), document-length normalization (`b`),
//! and IDF salience — end-to-end through a live [`Drevo`] graph, plus the
//! back-compat [`FtsRanking::TfIdf`] flag. A golden-ranking corpus is
//! built for each of the five target domains (CBT journal, story editor,
//! task manager, ERP, bug tracker) and the expected top hit is asserted.

use drevo::db::Drevo;
use drevo::model::{FtsRanking, NewNode};

fn db() -> Drevo {
    Drevo::open_in_memory().unwrap()
}

fn note(kind: &str, title: &str, body: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: String::new(),
        properties: Default::default(),
    }
}

// ---------------------------------------------------------------
// BM25 mechanics
// ---------------------------------------------------------------

#[test]
fn term_frequency_saturates_under_k1() {
    let db = db();
    // One occurrence vs ten occurrences of the same term.
    let single = db
        .create_node(note("note", "alpha topic", "beta gamma delta epsilon"))
        .unwrap();
    let many = db
        .create_node(note(
            "note",
            "alpha alpha alpha alpha alpha alpha alpha alpha alpha alpha",
            "",
        ))
        .unwrap();

    let results = db.search_fts("alpha", 10).unwrap();
    let many_score = results.iter().find(|r| r.node.id == many.id).unwrap().score;
    let single_score = results
        .iter()
        .find(|r| r.node.id == single.id)
        .unwrap()
        .score;

    // More hits rank higher, but saturation keeps tf=10 far below 10x.
    assert!(many_score > single_score);
    assert!(
        many_score < single_score * 10.0,
        "k1 saturation violated: many={many_score}, single={single_score}"
    );
}

#[test]
fn length_normalization_prefers_focused_document() {
    let db = db();
    // Both mention "quasar" once; the longer doc should rank lower.
    // Distinct titles — Drevo enforces unique node titles.
    let focused = db.create_node(note("note", "quasar north", "")).unwrap();
    let _verbose = db
        .create_node(note(
            "note",
            "quasar south",
            "a sprawling essay touching on cooking gardening astronomy \
             philosophy economics history music and assorted other topics",
        ))
        .unwrap();

    let results = db.search_fts("quasar", 10).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].node.id, focused.id,
        "the shorter, more focused document should rank first"
    );
}

#[test]
fn rare_term_carries_more_idf_weight() {
    let db = db();
    // "report" is common; "supernova" is rare. A doc with the rare term
    // should outrank docs that only share the common term.
    for i in 0..15 {
        db.create_node(note(
            "note",
            &format!("weekly report {i}"),
            "routine report body",
        ))
        .unwrap();
    }
    let rare = db
        .create_node(note(
            "note",
            "supernova report",
            "rare supernova event report",
        ))
        .unwrap();

    let results = db.search_fts("supernova report", 10).unwrap();
    assert!(!results.is_empty());
    assert_eq!(
        results[0].node.id, rare.id,
        "the document with the rare term must rank first"
    );
}

// ---------------------------------------------------------------
// Back-compat: TF-IDF flag
// ---------------------------------------------------------------

#[test]
fn tfidf_flag_still_ranks_and_finds_matches() {
    let db = db();
    db.create_node(note("note", "rust programming language", "systems"))
        .unwrap();
    db.create_node(note("note", "python scripting", ""))
        .unwrap();

    let bm25 = db.search_fts("rust", 10).unwrap();
    let tfidf = db.search_fts_ranked("rust", 10, FtsRanking::TfIdf).unwrap();

    assert_eq!(bm25.len(), 1);
    assert_eq!(tfidf.len(), 1);
    assert_eq!(bm25[0].node.id, tfidf[0].node.id);
    assert!(tfidf[0].score > 0.0);
}

#[test]
fn custom_bm25_parameters_are_honored() {
    let db = db();
    db.create_node(note("note", "rust north", "")).unwrap();
    db.create_node(note(
        "note",
        "rust south",
        "a much longer body padding out the document length considerably here",
    ))
    .unwrap();

    // With b=0 (no length normalization) two single-hit docs of equal tf
    // should score identically despite differing lengths.
    let no_norm = db
        .search_fts_ranked("rust", 10, FtsRanking::Bm25 { k1: 1.2, b: 0.0 })
        .unwrap();
    assert_eq!(no_norm.len(), 2);
    assert!(
        (no_norm[0].score - no_norm[1].score).abs() < 1e-6,
        "b=0 should disable length normalization"
    );

    // With b=0.75 the shorter doc should win.
    let with_norm = db
        .search_fts_ranked("rust", 10, FtsRanking::Bm25 { k1: 1.2, b: 0.75 })
        .unwrap();
    assert!(with_norm[0].score > with_norm[1].score);
}

// ---------------------------------------------------------------
// Index maintenance (stats stay correct across mutations)
// ---------------------------------------------------------------

#[test]
fn ranking_correct_after_update_changes_doc_length() {
    let db = db();
    let a = db
        .create_node(note("note", "kernel design north", ""))
        .unwrap();
    let b = db
        .create_node(note("note", "kernel design south", ""))
        .unwrap();

    // Initially equal — tie broken by ascending id.
    let before = db.search_fts("kernel", 10).unwrap();
    assert_eq!(before[0].node.id, a.id);

    // Bloat `a` with unrelated text (no query term, so tf stays 1 — only
    // the document length grows); `b` (now shorter) should overtake it.
    db.update_node(
        a.id,
        drevo::model::NodePatch {
            body: Some(
                "plus a long tail of unrelated commentary about scheduling \
                 memory paging filesystems and assorted networking layers"
                    .to_string(),
            ),
            ..Default::default()
        },
    )
    .unwrap();

    let after = db.search_fts("kernel", 10).unwrap();
    assert_eq!(
        after[0].node.id, b.id,
        "after `a` grew, the shorter `b` should rank first"
    );
}

#[test]
fn deleted_documents_drop_out_of_ranking() {
    let db = db();
    let a = db
        .create_node(note("bug", "race condition in scheduler", ""))
        .unwrap();
    db.create_node(note("bug", "scheduler starvation", ""))
        .unwrap();

    assert_eq!(db.search_fts("scheduler", 10).unwrap().len(), 2);
    db.delete_node(a.id).unwrap();
    let results = db.search_fts("scheduler", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results.iter().all(|r| r.node.id != a.id));
}

// ---------------------------------------------------------------
// Golden ranking across the five domains
// ---------------------------------------------------------------

#[test]
fn cbt_journal_recurring_theme_ranks_first() {
    let db = db();
    // The entry where "catastrophizing" recurs should top the ranking.
    let recurring = db
        .create_node(note(
            "thought",
            "Catastrophizing spiral",
            "catastrophizing again about work, catastrophizing about health, \
             catastrophizing about money",
        ))
        .unwrap();
    db.create_node(note(
        "thought",
        "A single catastrophizing moment",
        "noticed one catastrophizing thought and let it pass",
    ))
    .unwrap();
    db.create_node(note("thought", "Calm reflection", "today felt balanced"))
        .unwrap();

    let results = db.search_fts("catastrophizing", 10).unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].node.id, recurring.id);
}

#[test]
fn story_editor_motif_ranks_first() {
    let db = db();
    let motif = db
        .create_node(note(
            "chapter",
            "The Forest Deepens",
            "forest upon forest, the endless forest swallowed the path",
        ))
        .unwrap();
    db.create_node(note(
        "chapter",
        "Edge of the Forest",
        "they glimpsed the forest from afar",
    ))
    .unwrap();
    db.create_node(note(
        "character",
        "The Cartographer",
        "draws maps of cities",
    ))
    .unwrap();

    let results = db.search_fts("forest", 10).unwrap();
    assert!(results.len() >= 2);
    assert_eq!(results[0].node.id, motif.id);
}

#[test]
fn task_manager_keyword_ranks_first() {
    let db = db();
    let hot = db
        .create_node(note(
            "task",
            "Migration migration migration",
            "database migration rollout migration plan",
        ))
        .unwrap();
    db.create_node(note("task", "Plan the migration", "one migration step"))
        .unwrap();
    db.create_node(note("task", "Write release notes", "changelog"))
        .unwrap();

    let results = db.search_fts("migration", 10).unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].node.id, hot.id);
}

#[test]
fn erp_document_faceting_rare_term_ranks_first() {
    let db = db();
    // Many invoices, one specifically about a "refund".
    for i in 0..10 {
        db.create_node(note(
            "invoice",
            &format!("Invoice {i}"),
            "standard invoice line items",
        ))
        .unwrap();
    }
    let refund = db
        .create_node(note(
            "invoice",
            "Refund invoice",
            "customer refund processed",
        ))
        .unwrap();

    let results = db.search_fts("refund", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].node.id, refund.id);
}

#[test]
fn bug_tracker_cluster_term_ranks_first() {
    let db = db();
    let cluster = db
        .create_node(note(
            "bug",
            "Deadlock deadlock under load",
            "deadlock observed when two writers deadlock on the same key",
        ))
        .unwrap();
    db.create_node(note(
        "bug",
        "Occasional deadlock",
        "a deadlock once at startup",
    ))
    .unwrap();
    db.create_node(note("bug", "Typo in label", "cosmetic"))
        .unwrap();

    let results = db.search_fts("deadlock", 10).unwrap();
    assert!(results.len() >= 2);
    assert_eq!(results[0].node.id, cluster.id);
}
