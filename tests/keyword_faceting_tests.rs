//! Integration tests for keyword faceting — Phase 17 task `00133`.
//!
//! [`Drevo::facets`] groups every node of a `kind` by the keywords
//! extracted from one of its text fields ([`keywords()`](../src/fts/keywords.rs),
//! task `00132`), optionally collapsing near-duplicate keywords along one
//! of two axes:
//!
//! * **lexical** — shared Porter stem or close trigram overlap (typos);
//!   form-based, dependency-free, the default;
//! * **semantic** — cosine similarity of caller-supplied embeddings;
//!   meaning-based, opt-in.
//!
//! These tests run against a real (in-memory) `Drevo` graph across the five
//! drevo target scenario domains — CBT journal, story editor, IT task
//! manager, ERP, bug tracker — plus the property-source and edge-case
//! behaviours.

use std::collections::HashMap;

use drevo::db::Drevo;
use drevo::fts::facet::{
    Facet, FacetCollapse, DEFAULT_COSINE_THRESHOLD, DEFAULT_TRIGRAM_THRESHOLD,
};
use drevo::model::{NewNode, Properties};
use drevo::vector::Vector;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

fn note(kind: &str, title: &str, body: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn note_props(kind: &str, title: &str, props: &[(&str, serde_json::Value)]) -> NewNode {
    let mut map = HashMap::new();
    for (k, v) in props {
        map.insert((*k).to_string(), v.clone());
    }
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(map),
    }
}

/// Find the facet whose representative label is `label`.
fn find<'a>(facets: &'a [Facet], label: &str) -> &'a Facet {
    facets
        .iter()
        .find(|f| f.facet == label)
        .unwrap_or_else(|| panic!("no facet labelled '{label}' in {facets:?}"))
}

fn lexical() -> FacetCollapse<'static> {
    FacetCollapse::Lexical {
        trigram_threshold: DEFAULT_TRIGRAM_THRESHOLD,
    }
}

// ---------------------------------------------------------------
// Core behaviour
// ---------------------------------------------------------------

#[test]
fn facets_on_empty_graph_is_empty() {
    let db = db();
    let facets = db.facets("note", "body", 5, &FacetCollapse::None).unwrap();
    assert!(facets.is_empty());
}

#[test]
fn unknown_kind_yields_no_facets() {
    let db = db();
    db.create_node(note("note", "Title", "graph database engine"))
        .unwrap();
    let facets = db
        .facets("missing", "body", 5, &FacetCollapse::None)
        .unwrap();
    assert!(facets.is_empty());
}

#[test]
fn none_counts_distinct_documents_per_keyword() {
    let db = db();
    db.create_node(note("note", "A", "graph traversal"))
        .unwrap();
    db.create_node(note("note", "B", "graph storage")).unwrap();
    db.create_node(note("note", "C", "vector search")).unwrap();

    let facets = db.facets("note", "body", 5, &FacetCollapse::None).unwrap();
    // "graph" appears in two documents → highest count, ranked first.
    assert_eq!(facets[0].facet, "graph");
    assert_eq!(facets[0].count, 2);
    // Distinct keywords stay separate under `None`.
    assert!(facets.iter().any(|f| f.facet == "vector" && f.count == 1));
}

#[test]
fn body_is_the_default_source_but_title_is_selectable() {
    let db = db();
    db.create_node(note("note", "photosynthesis", "irrelevant filler"))
        .unwrap();
    let from_title = db.facets("note", "title", 3, &FacetCollapse::None).unwrap();
    assert!(from_title.iter().any(|f| f.facet == "photosynthesis"));
    let from_body = db.facets("note", "body", 3, &FacetCollapse::None).unwrap();
    assert!(from_body.iter().all(|f| f.facet != "photosynthesis"));
}

#[test]
fn facets_can_read_an_arbitrary_property() {
    let db = db();
    db.create_node(note_props(
        "ticket",
        "T-1",
        &[(
            "summary",
            serde_json::json!("kubernetes deployment rollout"),
        )],
    ))
    .unwrap();
    db.create_node(note_props(
        "ticket",
        "T-2",
        &[(
            "summary",
            serde_json::json!("kubernetes ingress controller"),
        )],
    ))
    .unwrap();

    let facets = db
        .facets("ticket", "summary", 5, &FacetCollapse::None)
        .unwrap();
    assert_eq!(find(&facets, "kubernetes").count, 2);
}

#[test]
fn node_missing_the_property_is_skipped_not_errored() {
    let db = db();
    // One node has the property, one doesn't — the scan must not abort.
    db.create_node(note_props(
        "ticket",
        "has-summary",
        &[(
            "summary",
            serde_json::json!("latency regression investigation"),
        )],
    ))
    .unwrap();
    db.create_node(note("ticket", "no-summary", "")).unwrap();

    let facets = db
        .facets("ticket", "summary", 5, &FacetCollapse::None)
        .unwrap();
    assert!(!facets.is_empty());
    assert!(facets.iter().all(|f| f.count == 1));
}

// ---------------------------------------------------------------
// Lexical collapse
// ---------------------------------------------------------------

#[test]
fn lexical_collapse_folds_morphological_variants() {
    // CBT journal: recurring "anxiety" theme spelled in two morphological
    // forms across entries — must collapse into one facet under lexical.
    let db = db();
    db.create_node(note("entry", "Mon", "anxiety before the meeting"))
        .unwrap();
    db.create_node(note("entry", "Tue", "lingering anxieties about work"))
        .unwrap();
    db.create_node(note("entry", "Wed", "anxiety returned at night"))
        .unwrap();

    let none = db.facets("entry", "body", 5, &FacetCollapse::None).unwrap();
    // Without collapsing, anxiety / anxieties are two separate facets.
    assert!(none.iter().any(|f| f.facet == "anxiety"));
    assert!(none.iter().any(|f| f.facet == "anxieties"));

    let collapsed = db.facets("entry", "body", 5, &lexical()).unwrap();
    let theme = find(&collapsed, "anxiety");
    // anxiety (docs Mon+Wed) ∪ anxieties (doc Tue) = 3 distinct documents.
    assert_eq!(theme.count, 3);
    assert!(theme.members.contains(&"anxieties".to_string()));
}

#[test]
fn lexical_representative_is_the_most_frequent_surface_form() {
    let db = db();
    for i in 0..3 {
        db.create_node(note("task", &format!("plan-{i}"), "deployment automation"))
            .unwrap();
    }
    db.create_node(note("task", "deploy-once", "manual deployments"))
        .unwrap();

    let facets = db.facets("task", "body", 5, &lexical()).unwrap();
    // "deployment" (3 docs) and "deployments" (1 doc) share a stem; the
    // 3-doc form is the representative.
    let f = find(&facets, "deployment");
    assert_eq!(f.facet, "deployment");
    assert_eq!(f.count, 4);
    assert!(f.members.contains(&"deployments".to_string()));
}

// ---------------------------------------------------------------
// Semantic collapse (opt-in, caller-supplied embeddings)
// ---------------------------------------------------------------

#[test]
fn semantic_collapse_merges_synonyms_across_documents() {
    // Story editor: motif tracking — "fear" and "dread" are synonyms with
    // no shared characters, so only the semantic axis collapses them.
    let db = db();
    db.create_node(note("scene", "Opening", "a creeping fear settled in"))
        .unwrap();
    db.create_node(note("scene", "Climax", "pure dread filled the hall"))
        .unwrap();

    let mut emb = HashMap::new();
    emb.insert("fear".to_string(), Vector(vec![1.0, 0.0, 0.0]));
    emb.insert("dread".to_string(), Vector(vec![0.99, 0.1, 0.0]));
    // Unrelated words get an orthogonal vector so they never merge.
    for kw in ["creeping", "settled", "pure", "filled", "hall"] {
        emb.insert(kw.to_string(), Vector(vec![0.0, 0.0, 1.0]));
    }

    let collapsed = db
        .facets(
            "scene",
            "body",
            5,
            &FacetCollapse::Semantic {
                embeddings: &emb,
                cosine_threshold: DEFAULT_COSINE_THRESHOLD,
            },
        )
        .unwrap();

    // fear (doc Opening) ∪ dread (doc Climax) collapse into one 2-doc facet.
    let motif = collapsed
        .iter()
        .find(|f| f.members.contains(&"fear".to_string()))
        .expect("a facet containing 'fear'");
    assert_eq!(motif.count, 2);
    assert!(motif.members.contains(&"dread".to_string()));
}

// ---------------------------------------------------------------
// Cross-domain smoke: ERP + bug tracker faceting end to end
// ---------------------------------------------------------------

#[test]
fn erp_documents_facet_by_extracted_keyword() {
    let db = db();
    db.create_node(note("invoice", "INV-1", "quarterly procurement supplies"))
        .unwrap();
    db.create_node(note("invoice", "INV-2", "quarterly procurement hardware"))
        .unwrap();
    db.create_node(note("invoice", "INV-3", "annual maintenance contract"))
        .unwrap();

    let facets = db.facets("invoice", "body", 5, &lexical()).unwrap();
    // "procurement" recurs in two invoices.
    assert_eq!(find(&facets, "procurement").count, 2);
    // Facets are sorted by descending document count.
    let counts: Vec<u64> = facets.iter().map(|f| f.count).collect();
    let mut sorted = counts.clone();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        counts, sorted,
        "facets must be count-descending: {facets:?}"
    );
}

#[test]
fn bug_tracker_clusters_by_description_terms() {
    let db = db();
    db.create_node(note("bug", "#1", "crash on startup with null pointer"))
        .unwrap();
    db.create_node(note("bug", "#2", "crash when saving large files"))
        .unwrap();
    db.create_node(note("bug", "#3", "memory leak in background worker"))
        .unwrap();

    let facets = db.facets("bug", "body", 5, &FacetCollapse::None).unwrap();
    // "crash" clusters two of the three bug reports.
    assert_eq!(find(&facets, "crash").count, 2);
}

#[test]
fn faceting_is_deterministic_across_runs() {
    let db = db();
    for (i, body) in [
        "graph database engine",
        "graph traversal algorithm",
        "vector similarity search",
    ]
    .iter()
    .enumerate()
    {
        db.create_node(note("note", &format!("n{i}"), body))
            .unwrap();
    }
    let a = db.facets("note", "body", 5, &lexical()).unwrap();
    let b = db.facets("note", "body", 5, &lexical()).unwrap();
    assert_eq!(a, b);
}
