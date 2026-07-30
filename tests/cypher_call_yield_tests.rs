//! End-to-end Cypher tests — Phase 10 follow-up task `00145`.
//!
//! `CALL proc.name(args) [YIELD col [AS alias] … [WHERE pred]]` invokes a
//! built-in procedure. drevo ships the read-only schema-introspection
//! procedures Neo4j drivers and tools expect:
//!
//! * `db.labels()` — YIELD `label`: every distinct node label (the primary
//!   kind plus any secondary `:Extra` labels), sorted.
//! * `db.relationshipTypes()` — YIELD `relationshipType`: every distinct
//!   edge kind, sorted.
//! * `db.propertyKeys()` — YIELD `propertyKey`: every distinct property key
//!   across nodes and edges (the reserved `_labels` key is never exposed),
//!   sorted.
//!
//! Before `00145` the `CALL` / `YIELD` keywords tokenised (`00061`) but
//! neither the grammar nor the executor handled them, so any `CALL` query
//! failed to parse. This task adds the `Clause::Call` AST node, the parser
//! production (dotted name + argument list + optional `YIELD … WHERE`), the
//! executor's procedure registry, and `ExecError::InvalidProcedureCall`
//! (unknown procedure / wrong arity / unknown yield column). A standalone
//! `CALL` with no `YIELD` projects every output column directly; `YIELD`
//! binds the named columns into the row stream for downstream clauses.
//!
//! These cases drive the real parser → executor pipeline across the five
//! drevo target scenario domains (CBT journal, story / book editor, IT
//! task manager, ERP, bug tracker) plus the cross-cutting semantics.

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

fn exec(source: &str, drevo: &Drevo) {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute");
}

fn exec_err(source: &str, drevo: &Drevo) -> ExecError {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect_err("expected execution error")
}

/// Flatten a single-column string result into a `Vec<String>`.
fn strings(rows: &[Vec<Value>]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected String, got {other:?}"),
        })
        .collect()
}

// ---- CBT journal -----------------------------------------------------------

#[test]
fn cbt_journal_discovers_its_label_vocabulary() {
    let db = db();
    exec(
        "CREATE (:Thought {body: 'I always fail'}), \
         (:Distortion {name: 'catastrophising'}), \
         (:Reframe {body: 'sometimes I succeed'})",
        &db,
    );
    let labels = strings(&run("CALL db.labels()", &db));
    assert_eq!(labels, vec!["Distortion", "Reframe", "Thought"]);
}

#[test]
fn cbt_journal_lists_relationship_vocabulary() {
    let db = db();
    exec(
        "CREATE (t:Thought)-[:HAS_DISTORTION]->(d:Distortion), \
         (t)-[:REFRAMED_AS]->(r:Reframe)",
        &db,
    );
    let kinds = strings(&run("CALL db.relationshipTypes()", &db));
    assert_eq!(kinds, vec!["HAS_DISTORTION", "REFRAMED_AS"]);
}

// ---- Story / book editor ---------------------------------------------------

#[test]
fn story_editor_counts_labels_via_yield_aggregation() {
    let db = db();
    exec(
        "CREATE (:Character {title: 'Ahab'}), (:Character {title: 'Ishmael'}), \
         (:Chapter {title: 'Loomings'}), (:Scene {title: 'The Pequod'})",
        &db,
    );
    // CALL feeding an aggregation: how many distinct labels does the
    // manuscript use?
    let rows = run(
        "CALL db.labels() YIELD label RETURN count(label) AS kinds",
        &db,
    );
    assert_eq!(rows[0][0], Value::Integer(3));
}

#[test]
fn story_editor_property_keys_union_nodes_and_edges() {
    let db = db();
    exec(
        "CREATE (a:Character {title: 'Ahab', mood: 'vengeful'}) \
         -[:APPEARS_IN {page: 12}]->(c:Chapter {title: 'The Quarter-Deck'})",
        &db,
    );
    let keys = strings(&run("CALL db.propertyKeys()", &db));
    // `title` from nodes, `mood` from a node, `page` from the edge.
    assert_eq!(keys, vec!["mood", "page", "title"]);
    assert!(!keys.contains(&"_labels".to_string()));
}

// ---- IT task manager -------------------------------------------------------

#[test]
fn task_manager_yield_where_filters_to_one_label() {
    let db = db();
    exec(
        "CREATE (:Task {title: 'ship'}), (:Sprint {title: 'S1'}), (:User {title: 'dev'})",
        &db,
    );
    let rows = run(
        "CALL db.labels() YIELD label WHERE label = 'Task' RETURN label",
        &db,
    );
    assert_eq!(strings(&rows), vec!["Task"]);
}

#[test]
fn task_manager_yield_alias_renames_column() {
    let db = db();
    exec("CREATE (:Task {title: 'ship'})", &db);
    let rows = run("CALL db.labels() YIELD label AS kind RETURN kind", &db);
    assert_eq!(strings(&rows), vec!["Task"]);
}

// ---- ERP -------------------------------------------------------------------

#[test]
fn erp_secondary_labels_appear_in_label_listing() {
    let db = db();
    // Multi-label node: an Employee that is also a Manager.
    exec("CREATE (:Person:Employee:Manager {title: 'Dana'})", &db);
    let labels = strings(&run("CALL db.labels()", &db));
    assert_eq!(labels, vec!["Employee", "Manager", "Person"]);
}

#[test]
fn erp_relationship_types_are_distinct_and_sorted() {
    let db = db();
    // `CONTAINS` is a reserved Cypher keyword (string predicate); used as a
    // relationship type it must round-trip with its written casing — see the
    // `consume_name` keyword-casing fix and `keyword_rel_type_*` regressions.
    exec(
        "CREATE (o:Order)-[:CONTAINS]->(:LineItem), \
         (o)-[:PLACED_BY]->(:Customer), \
         (o)-[:CONTAINS]->(:LineItem)",
        &db,
    );
    let kinds = strings(&run("CALL db.relationshipTypes()", &db));
    assert_eq!(kinds, vec!["CONTAINS", "PLACED_BY"]);
}

#[test]
fn keyword_relationship_type_round_trips_through_call() {
    // Regression for the keyword-casing fix, via the exact path that
    // surfaced it: a `:CONTAINS` edge must appear as `CONTAINS` (not the
    // lowercased `contains`) in `CALL db.relationshipTypes()`.
    let db = db();
    exec("CREATE (a:N)-[:CONTAINS]->(b:N)", &db);
    let kinds = strings(&run("CALL db.relationshipTypes()", &db));
    assert_eq!(kinds, vec!["CONTAINS"]);
}

// ---- Bug tracker -----------------------------------------------------------

#[test]
fn bug_tracker_empty_graph_yields_no_labels() {
    let db = db();
    let rows = run("CALL db.labels()", &db);
    assert!(rows.is_empty());
}

#[test]
fn bug_tracker_standalone_call_projects_output_column() {
    let db = db();
    exec(
        "CREATE (:Bug {title: 'crash'}), (:Component {title: 'parser'})",
        &db,
    );
    let q = parse("CALL db.labels()").expect("parse");
    let res = execute(&q, &db, HashMap::new()).expect("execute");
    assert_eq!(res.columns, vec!["label"]);
    assert_eq!(strings(&res.rows), vec!["Bug", "Component"]);
}

// ---- Cross-cutting error semantics -----------------------------------------

#[test]
fn unknown_procedure_is_rejected() {
    let db = db();
    let e = exec_err("CALL db.nope()", &db);
    assert!(
        matches!(e, ExecError::InvalidProcedureCall { ref name, .. } if name == "db.nope"),
        "got {e:?}"
    );
}

#[test]
fn arguments_to_zero_arity_procedure_are_rejected() {
    let db = db();
    let e = exec_err("CALL db.labels(42)", &db);
    assert!(
        matches!(e, ExecError::InvalidProcedureCall { ref message, .. } if message.contains("0 arguments")),
        "got {e:?}"
    );
}

#[test]
fn yield_of_unknown_column_is_rejected() {
    let db = db();
    let e = exec_err("CALL db.labels() YIELD bogus RETURN bogus", &db);
    assert!(
        matches!(e, ExecError::InvalidProcedureCall { ref message, .. } if message.contains("does not yield")),
        "got {e:?}"
    );
}

#[test]
fn error_is_deterministic_on_empty_graph() {
    // The upfront validation sweep surfaces the error before any rows are
    // produced, so an empty graph still rejects a bad CALL.
    let db = db();
    let e = exec_err("CALL db.nope() YIELD x RETURN x", &db);
    assert!(
        matches!(e, ExecError::InvalidProcedureCall { .. }),
        "got {e:?}"
    );
}

// ---- Vector search (issue #202 Part 2) -------------------------------------
// `CALL drevo.vector.query(label, property, query, k) YIELD node, score` —
// top-k nodes ranked by cosine similarity to a query vector. Brute-force,
// works with externally-computed embeddings stored as a list node property.

/// Seed three chunks: `a` is identical to the query direction, `b` close,
/// `c` orthogonal; `a`/`b` in book 1, `c` in book 2.
fn seed_chunks(db: &Drevo) {
    exec(
        "CREATE (:Chunk {title: 'a', book_id: 1, embedding: [1.0, 0.0]}), \
                (:Chunk {title: 'b', book_id: 1, embedding: [0.8, 0.6]}), \
                (:Chunk {title: 'c', book_id: 2, embedding: [0.0, 1.0]})",
        db,
    );
}

#[test]
fn vector_query_ranks_top_k_by_cosine() {
    let db = db();
    seed_chunks(&db);
    let rows = run(
        "CALL drevo.vector.query('Chunk', 'embedding', [1.0, 0.0], 3) YIELD node, score \
         RETURN node.title AS t ORDER BY score DESC",
        &db,
    );
    assert_eq!(strings(&rows), vec!["a", "b", "c"]);
}

#[test]
fn vector_query_honours_k_limit() {
    let db = db();
    seed_chunks(&db);
    let rows = run(
        "CALL drevo.vector.query('Chunk', 'embedding', [1.0, 0.0], 2) YIELD node, score \
         RETURN node.title AS t ORDER BY score DESC",
        &db,
    );
    assert_eq!(strings(&rows), vec!["a", "b"]);
}

#[test]
fn vector_query_post_yield_where_filters_by_property() {
    let db = db();
    seed_chunks(&db);
    // The issue's headline query shape: score, then filter by book_id.
    let rows = run(
        "CALL drevo.vector.query('Chunk', 'embedding', [1.0, 0.0], 3) YIELD node, score \
         WHERE node.book_id = 1 \
         RETURN node.title AS t ORDER BY score DESC",
        &db,
    );
    assert_eq!(strings(&rows), vec!["a", "b"]);
}

#[test]
fn vector_query_exposes_a_numeric_score() {
    let db = db();
    seed_chunks(&db);
    // The identical-direction chunk scores 1.0.
    let rows = run(
        "CALL drevo.vector.query('Chunk', 'embedding', [1.0, 0.0], 1) YIELD node, score \
         RETURN score AS s",
        &db,
    );
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::Float(f) => assert!((f - 1.0).abs() < 1e-6, "expected ~1.0, got {f}"),
        other => panic!("score must be a Float, got {other:?}"),
    }
}

#[test]
fn vector_query_skips_nodes_without_the_embedding() {
    let db = db();
    seed_chunks(&db);
    exec("CREATE (:Chunk {title: 'no_emb', book_id: 1})", &db);
    let rows = run(
        "CALL drevo.vector.query('Chunk', 'embedding', [1.0, 0.0], 10) YIELD node, score \
         RETURN node.title AS t",
        &db,
    );
    let titles = strings(&rows);
    assert!(!titles.contains(&"no_emb".to_string()), "got {titles:?}");
    assert_eq!(titles.len(), 3);
}

#[test]
fn vector_query_standalone_projects_node_and_score() {
    let db = db();
    seed_chunks(&db);
    let rows = run(
        "CALL drevo.vector.query('Chunk', 'embedding', [1.0, 0.0], 1)",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 2, "standalone CALL projects node + score");
    assert!(matches!(rows[0][0], Value::Node(_)));
    assert!(matches!(rows[0][1], Value::Float(_)));
}

#[test]
fn vector_query_unknown_label_is_empty_not_error() {
    let db = db();
    seed_chunks(&db);
    let rows = run(
        "CALL drevo.vector.query('Nonexistent', 'embedding', [1.0, 0.0], 3) YIELD node, score \
         RETURN node",
        &db,
    );
    assert!(rows.is_empty());
}

#[test]
fn vector_query_wrong_arity_is_an_error() {
    let db = db();
    seed_chunks(&db);
    let err = exec_err(
        "CALL drevo.vector.query('Chunk', 'embedding', [1.0, 0.0]) YIELD node, score RETURN node",
        &db,
    );
    assert!(
        matches!(err, ExecError::InvalidProcedureCall { .. }),
        "{err:?}"
    );
}

// ---- Full-text search (issue #208) -----------------------------------------
// `CALL fts.search(query, k) YIELD node, score` — BM25-ranked matching nodes
// from the full-text index (task 00131), the FTS analogue of
// `drevo.vector.query`.

/// Two entities carry the distinctive term `zorptastic` (in different groups);
/// a third is unrelated (no shared trigrams with the query term).
fn seed_fts_entities(db: &Drevo) {
    exec(
        "CREATE (:Entity {title: 'zorptastic anxiety spiral', group_id: 1}), \
                (:Entity {title: 'zorptastic calm morning', group_id: 2}), \
                (:Entity {title: 'unrelated content here', group_id: 1})",
        db,
    );
}

#[test]
fn fts_search_returns_only_matching_nodes() {
    let db = db();
    seed_fts_entities(&db);
    let rows = run(
        "CALL fts.search('zorptastic', 10) YIELD node, score RETURN node.title AS t",
        &db,
    );
    let titles = strings(&rows);
    assert_eq!(titles.len(), 2, "got {titles:?}");
    assert!(
        titles.iter().all(|t| t.contains("zorptastic")),
        "{titles:?}"
    );
    assert!(
        !titles.iter().any(|t| t.contains("unrelated")),
        "{titles:?}"
    );
}

#[test]
fn fts_search_post_yield_where_filters_by_group() {
    let db = db();
    seed_fts_entities(&db);
    let rows = run(
        "CALL fts.search('zorptastic', 10) YIELD node, score \
         WHERE node.group_id = 1 \
         RETURN node.title AS t",
        &db,
    );
    assert_eq!(strings(&rows), vec!["zorptastic anxiety spiral"]);
}

#[test]
fn fts_search_honours_k_limit() {
    let db = db();
    seed_fts_entities(&db);
    let rows = run(
        "CALL fts.search('zorptastic', 1) YIELD node, score RETURN node.title AS t",
        &db,
    );
    assert_eq!(strings(&rows).len(), 1);
}

#[test]
fn fts_search_exposes_a_positive_score() {
    let db = db();
    seed_fts_entities(&db);
    let rows = run(
        "CALL fts.search('zorptastic', 1) YIELD node, score RETURN score AS s",
        &db,
    );
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::Float(f) => assert!(*f > 0.0, "BM25 score should be positive, got {f}"),
        other => panic!("score must be a Float, got {other:?}"),
    }
}

#[test]
fn fts_search_standalone_projects_node_and_score() {
    let db = db();
    seed_fts_entities(&db);
    let rows = run("CALL fts.search('zorptastic', 1)", &db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 2);
    assert!(matches!(rows[0][0], Value::Node(_)));
    assert!(matches!(rows[0][1], Value::Float(_)));
}

#[test]
fn fts_search_no_match_is_empty() {
    let db = db();
    seed_fts_entities(&db);
    let rows = run(
        "CALL fts.search('nonexistentqwxyz', 10) YIELD node, score RETURN node",
        &db,
    );
    assert!(rows.is_empty());
}

#[test]
fn fts_search_wrong_arity_is_an_error() {
    let db = db();
    seed_fts_entities(&db);
    let err = exec_err(
        "CALL fts.search('zorptastic') YIELD node, score RETURN node",
        &db,
    );
    assert!(
        matches!(err, ExecError::InvalidProcedureCall { .. }),
        "{err:?}"
    );
}
