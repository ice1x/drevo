//! End-to-end Cypher `keywords(...)` tests — Phase 17 task `00132`.
//!
//! `keywords(text, k [, stem])` is drevo's keyword-extraction scalar
//! function: it returns the top-`k` salient terms of `text`, ranked by
//! term-frequency × BM25 IDF (task `00131`) over the indexed corpus, after
//! word tokenization and English stopword removal. An optional third
//! boolean argument enables Porter stemming. It composes in `RETURN`,
//! `WHERE`, and per-row over a `MATCH`. The faceted group-by
//!
//! ```cypher
//! MATCH (n) UNWIND keywords(n.body, 5) AS kw RETURN kw, count(*) ORDER BY count(*) DESC
//! ```
//!
//! is the intended downstream use once the executor's `UNWIND` clause is
//! implemented (a separate roadmap item; `UNWIND` is not yet executable).
//!
//! These tests exercise the function across the five drevo target scenario
//! domains (CBT journal, story editor, task manager, ERP, bug tracker) plus
//! the NULL-propagation and error edge cases.

use std::collections::HashMap;

use drevo::cypher::executor::{execute, ExecError, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

fn run(source: &str, drevo: &Drevo) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").rows
}

fn err(source: &str, drevo: &Drevo) -> ExecError {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect_err("expected error")
}

/// Extract the single `Value::List` of strings returned in the first column
/// of the first row.
fn keyword_list(rows: &[Vec<Value>]) -> Vec<String> {
    match rows.first().and_then(|r| r.first()) {
        Some(Value::List(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                _ => panic!("keyword list element is not a string: {v:?}"),
            })
            .collect(),
        other => panic!("expected a List in the first column, got {other:?}"),
    }
}

/// Create a node with a unique title and the given body text (which feeds the
/// FTS / BM25 corpus that keyword IDF is computed against).
fn create(drevo: &Drevo, label: &str, title: &str, body: &str) {
    run(
        &format!("CREATE (:{label} {{title: '{title}', body: '{body}'}})"),
        drevo,
    );
}

// ===== Core semantics =======================================================

#[test]
fn keywords_returns_a_list_of_strings() {
    let db = db();
    let rows = run("RETURN keywords('graph database systems', 3) AS kw", &db);
    let kws = keyword_list(&rows);
    assert!(!kws.is_empty());
    assert!(kws.iter().all(|w| !w.is_empty()));
    assert!(kws.len() <= 3);
}

#[test]
fn keywords_drops_stopwords() {
    let db = db();
    let kws = keyword_list(&run(
        "RETURN keywords('the anxiety and the worry of the day', 5) AS kw",
        &db,
    ));
    assert!(kws.contains(&"anxiety".to_string()));
    assert!(kws.contains(&"worry".to_string()));
    assert!(!kws.iter().any(|w| w == "the" || w == "and" || w == "of"));
}

#[test]
fn keywords_respects_k_limit() {
    let db = db();
    let kws = keyword_list(&run(
        "RETURN keywords('alpha beta gamma delta epsilon zeta', 2) AS kw",
        &db,
    ));
    assert_eq!(kws.len(), 2);
}

#[test]
fn keywords_zero_k_is_empty() {
    let db = db();
    assert!(keyword_list(&run("RETURN keywords('graph node edge', 0) AS kw", &db)).is_empty());
}

// ===== BM25 IDF salience ====================================================

#[test]
fn rare_term_outranks_common_term_cbt() {
    // CBT journal: many entries mention "meeting" (common, low IDF); a single
    // entry mentions "mitochondria" (rare, high IDF). Both occur once in the
    // probe text, so corpus rarity decides the ranking.
    let db = db();
    for i in 0..6 {
        create(
            &db,
            "Entry",
            &format!("entry-{i}"),
            "felt anxious before the meeting today",
        );
    }
    create(
        &db,
        "Entry",
        "biology-note",
        "reflected on mitochondria during the meeting",
    );

    let kws = keyword_list(&run(
        "RETURN keywords('meeting mitochondria', 2) AS kw",
        &db,
    ));
    assert_eq!(
        kws.first().map(String::as_str),
        Some("mitochondria"),
        "the rarer corpus term should rank first, got {kws:?}"
    );
}

#[test]
fn deterministic_across_runs() {
    let db = db();
    let a = keyword_list(&run("RETURN keywords('zebra apple mango', 3) AS kw", &db));
    let b = keyword_list(&run("RETURN keywords('mango zebra apple', 3) AS kw", &db));
    assert_eq!(a, b);
}

// ===== Per-node extraction over a MATCH =====================================
//
// NB: the headline faceted idiom `MATCH (n) UNWIND keywords(n.body, k) AS kw
// RETURN kw, count(*)` needs the `UNWIND` clause, which the executor does not
// implement yet (it is a separate roadmap item, distinct from this task). The
// `keywords(...)` function itself composes per-row in a `MATCH ... RETURN`
// projection today, which these tests exercise; once `UNWIND` lands the
// faceted group-by works with no change to `keywords`.

#[test]
fn keywords_extracted_per_node_task_manager() {
    // Task manager: extract each task's salient keywords as a per-row list.
    let db = db();
    create(
        &db,
        "Task",
        "t1",
        "deploy the payment service to production",
    );
    create(&db, "Task", "t2", "payment service latency investigation");

    let rows = run(
        "MATCH (t:Task) RETURN t.title AS title, keywords(t.body, 5) AS kw ORDER BY title",
        &db,
    );
    assert_eq!(rows.len(), 2);

    // Collect the keyword list of each task by title.
    let by_title: HashMap<String, Vec<String>> = rows
        .iter()
        .map(|r| {
            let title = match &r[0] {
                Value::String(s) => s.clone(),
                other => panic!("title not a string: {other:?}"),
            };
            let kws = match &r[1] {
                Value::List(items) => items
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        _ => panic!("kw not a string"),
                    })
                    .collect(),
                other => panic!("kw not a list: {other:?}"),
            };
            (title, kws)
        })
        .collect();

    assert!(by_title["t1"].contains(&"payment".to_string()));
    assert!(by_title["t1"].contains(&"service".to_string()));
    assert!(by_title["t2"].contains(&"latency".to_string()));
    // Stopwords never appear in any task's keywords.
    assert!(by_title
        .values()
        .all(|kws| !kws.iter().any(|w| w == "the" || w == "to")));
}

// ===== NULL propagation =====================================================

#[test]
fn missing_property_yields_no_keywords_not_an_error() {
    // Bug tracker: a bug node with no `summary` property. keywords(b.summary)
    // must yield an empty list rather than aborting the scan, so a query that
    // scans a heterogeneous label degrades gracefully on rows that lack the
    // text property.
    let db = db();
    create(&db, "Bug", "bug-1", "null pointer dereference in parser");

    let rows = run("MATCH (b:Bug) RETURN keywords(b.summary, 5) AS kw", &db);
    assert!(keyword_list(&rows).is_empty());

    // A NULL literal text argument behaves identically.
    let lit = run("RETURN keywords(null, 5) AS kw", &db);
    assert!(keyword_list(&lit).is_empty());
}

// ===== Stemming =============================================================

#[test]
fn stemming_collapses_morphological_variants_story() {
    // Story editor: a passage repeating variants of "run".
    let db = db();

    let unstemmed = keyword_list(&run(
        "RETURN keywords('connect connecting connections connected', 5, false) AS kw",
        &db,
    ));
    // Without stemming the surface forms stay distinct.
    assert!(unstemmed.len() >= 2, "got {unstemmed:?}");

    let stemmed = keyword_list(&run(
        "RETURN keywords('connect connecting connections connected', 5, true) AS kw",
        &db,
    ));
    // With stemming they collapse onto a single stem ("connect").
    assert_eq!(stemmed.len(), 1, "variants should merge, got {stemmed:?}");
}

// ===== ERP domain sanity ====================================================

#[test]
fn keywords_surface_domain_terms_erp() {
    let db = db();
    let kws = keyword_list(&run(
        "RETURN keywords('quarterly invoice reconciliation for the vendor ledger', 4) AS kw",
        &db,
    ));
    // The salient nouns survive; the stopword "the" / "for" do not.
    assert!(kws.iter().any(|w| w == "invoice"));
    assert!(kws
        .iter()
        .any(|w| w == "reconciliation" || w == "ledger" || w == "vendor"));
    assert!(!kws.iter().any(|w| w == "for" || w == "the"));
}

// ===== Errors ===============================================================

#[test]
fn wrong_arity_is_an_error() {
    let db = db();
    let e = err("RETURN keywords('only one arg')", &db);
    assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
}

#[test]
fn non_integer_k_is_an_error() {
    let db = db();
    let e = err("RETURN keywords('some text', 'three')", &db);
    assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
}

#[test]
fn non_boolean_stem_flag_is_an_error() {
    let db = db();
    let e = err("RETURN keywords('some text', 3, 'yes')", &db);
    assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
}
