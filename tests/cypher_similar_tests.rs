//! End-to-end Cypher `similar(...)` predicate tests — Phase 12 task `00077`.
//!
//! `similar(vector, query, threshold)` is drevo's joint graph+vector
//! extension: it lets a single Cypher query both traverse the graph and
//! filter by embedding similarity, e.g.
//!
//! ```cypher
//! MATCH (n:Doc) WHERE similar(n.embedding, $q, 0.85) RETURN n.title
//! ```
//!
//! The predicate computes cosine similarity (`src/vector/distance.rs`)
//! between the first two arguments and returns `true` when the score is
//! at least `threshold`. Embeddings live inside node `properties` as JSON
//! arrays, so they surface in the executor as `Value::List` of numbers.
//!
//! These tests exercise the predicate across the five drevo target
//! scenario domains plus the cross-cutting graph-RAG retrieval idiom and
//! the error / NULL-propagation edge cases.

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

fn run_with(source: &str, drevo: &Drevo, params: HashMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, params).expect("execute").rows
}

fn err(source: &str, drevo: &Drevo, params: HashMap<String, Value>) -> ExecError {
    let q = parse(source).expect("parse");
    execute(&q, drevo, params).expect_err("expected error")
}

/// Build a `Value::List` embedding from `f64` components — the runtime
/// shape a JSON-array node property takes on inside the executor.
fn vec_value(components: &[f64]) -> Value {
    Value::List(components.iter().copied().map(Value::Float).collect())
}

fn query_param(components: &[f64]) -> HashMap<String, Value> {
    let mut params = HashMap::new();
    params.insert("q".to_string(), vec_value(components));
    params
}

fn names(rows: &[Vec<Value>]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            _ => String::new(),
        })
        .collect()
}

/// Insert a `:Doc` node whose `embedding` property is a vector literal.
fn create_doc(drevo: &Drevo, title: &str, embedding: &[f64]) {
    let list = embedding
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    run(
        &format!("CREATE (:Doc {{title: '{title}', embedding: [{list}]}})"),
        drevo,
    );
}

// ===== Core semantics =======================================================

#[test]
fn identical_vectors_pass_any_threshold_below_one() {
    let db = db();
    create_doc(&db, "exact", &[1.0, 0.0, 0.0]);
    let rows = run_with(
        "MATCH (d:Doc) WHERE similar(d.embedding, $q, 0.99) RETURN d.title AS t",
        &db,
        query_param(&[1.0, 0.0, 0.0]),
    );
    assert_eq!(names(&rows), vec!["exact".to_string()]);
}

#[test]
fn orthogonal_vectors_fail_high_threshold() {
    let db = db();
    create_doc(&db, "orthogonal", &[0.0, 1.0]);
    let rows = run_with(
        "MATCH (d:Doc) WHERE similar(d.embedding, $q, 0.5) RETURN d.title AS t",
        &db,
        query_param(&[1.0, 0.0]),
    );
    // cosine = 0.0, below the 0.5 threshold → filtered out.
    assert!(rows.is_empty());
}

#[test]
fn ranks_and_filters_by_cosine_threshold() {
    let db = db();
    // Query direction is [1, 0].
    create_doc(&db, "aligned", &[1.0, 0.0]); //   cos = 1.0
    create_doc(&db, "near", &[1.0, 0.2]); //       cos ≈ 0.981
    create_doc(&db, "diagonal", &[1.0, 1.0]); //   cos ≈ 0.707
    create_doc(&db, "away", &[0.0, 1.0]); //       cos = 0.0
    let rows = run_with(
        "MATCH (d:Doc) WHERE similar(d.embedding, $q, 0.85) RETURN d.title AS t ORDER BY d.title",
        &db,
        query_param(&[1.0, 0.0]),
    );
    assert_eq!(
        names(&rows),
        vec!["aligned".to_string(), "near".to_string()]
    );
}

#[test]
fn threshold_boundary_is_inclusive() {
    let db = db();
    create_doc(&db, "exact", &[3.0, 4.0]);
    // cosine of a vector with itself is exactly 1.0; threshold 1.0 must pass.
    let rows = run_with(
        "MATCH (d:Doc) WHERE similar(d.embedding, $q, 1.0) RETURN d.title AS t",
        &db,
        query_param(&[3.0, 4.0]),
    );
    assert_eq!(names(&rows), vec!["exact".to_string()]);
}

#[test]
fn cosine_is_magnitude_invariant() {
    let db = db();
    // Same direction, 10× magnitude → cosine still 1.0.
    create_doc(&db, "scaled", &[10.0, 0.0]);
    let rows = run_with(
        "MATCH (d:Doc) WHERE similar(d.embedding, $q, 0.999) RETURN d.title AS t",
        &db,
        query_param(&[1.0, 0.0]),
    );
    assert_eq!(names(&rows), vec!["scaled".to_string()]);
}

#[test]
fn negative_threshold_admits_opposite_vectors() {
    let db = db();
    create_doc(&db, "opposite", &[-1.0, 0.0]);
    // cosine = -1.0; only a threshold ≤ -1 lets it through.
    let rows = run_with(
        "MATCH (d:Doc) WHERE similar(d.embedding, $q, -1.0) RETURN d.title AS t",
        &db,
        query_param(&[1.0, 0.0]),
    );
    assert_eq!(names(&rows), vec!["opposite".to_string()]);
}

#[test]
fn query_vector_as_inline_list_literal() {
    let db = db();
    create_doc(&db, "doc", &[1.0, 0.0]);
    let rows = run(
        "MATCH (d:Doc) WHERE similar(d.embedding, [1.0, 0.0], 0.9) RETURN d.title AS t",
        &db,
    );
    assert_eq!(names(&rows), vec!["doc".to_string()]);
}

#[test]
fn similar_in_return_projects_boolean() {
    let db = db();
    create_doc(&db, "doc", &[1.0, 0.0]);
    let rows = run_with(
        "MATCH (d:Doc) RETURN similar(d.embedding, $q, 0.9) AS hit",
        &db,
        query_param(&[1.0, 0.0]),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Bool(true));
}

// ===== NULL propagation =====================================================

#[test]
fn missing_embedding_yields_null_and_filters_row() {
    let db = db();
    create_doc(&db, "with_vec", &[1.0, 0.0]);
    run("CREATE (:Doc {title: 'no_vec'})", &db); // no embedding property
    let rows = run_with(
        "MATCH (d:Doc) WHERE similar(d.embedding, $q, 0.5) RETURN d.title AS t ORDER BY d.title",
        &db,
        query_param(&[1.0, 0.0]),
    );
    // The node lacking an embedding evaluates similar(...) to NULL, which
    // WHERE treats as falsy — only the embedded node survives.
    assert_eq!(names(&rows), vec!["with_vec".to_string()]);
}

#[test]
fn null_in_return_is_null_not_error() {
    let db = db();
    run("CREATE (:Doc {title: 'no_vec'})", &db);
    let rows = run_with(
        "MATCH (d:Doc) RETURN similar(d.embedding, $q, 0.5) AS hit",
        &db,
        query_param(&[1.0, 0.0]),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Null);
}

// ===== Error handling =======================================================

#[test]
fn wrong_arity_is_an_error() {
    let db = db();
    create_doc(&db, "doc", &[1.0, 0.0]);
    let e = err(
        "MATCH (d:Doc) WHERE similar(d.embedding, $q) RETURN d.title",
        &db,
        query_param(&[1.0, 0.0]),
    );
    assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
}

#[test]
fn dimension_mismatch_is_an_error() {
    let db = db();
    create_doc(&db, "doc", &[1.0, 0.0, 0.0]);
    let e = err(
        "MATCH (d:Doc) WHERE similar(d.embedding, $q, 0.5) RETURN d.title",
        &db,
        query_param(&[1.0, 0.0]), // 2 dims vs the node's 3 dims
    );
    assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
}

#[test]
fn non_numeric_threshold_is_an_error() {
    let db = db();
    create_doc(&db, "doc", &[1.0, 0.0]);
    let e = err(
        "MATCH (d:Doc) WHERE similar(d.embedding, $q, 'high') RETURN d.title",
        &db,
        query_param(&[1.0, 0.0]),
    );
    assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
}

#[test]
fn non_list_vector_argument_is_an_error() {
    let db = db();
    run(
        "CREATE (:Doc {title: 'doc', embedding: 'not a vector'})",
        &db,
    );
    let e = err(
        "MATCH (d:Doc) WHERE similar(d.embedding, $q, 0.5) RETURN d.title",
        &db,
        query_param(&[1.0, 0.0]),
    );
    assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
}

#[test]
fn unknown_function_remains_unsupported() {
    let db = db();
    create_doc(&db, "doc", &[1.0, 0.0]);
    let e = err(
        "MATCH (d:Doc) WHERE totally_made_up(d.embedding) RETURN d.title",
        &db,
        HashMap::new(),
    );
    assert!(matches!(e, ExecError::Unsupported { .. }), "{e:?}");
}

// ===== Scenario coverage ====================================================

#[test]
fn cbt_journal_finds_thoughts_with_similar_emotional_signature() {
    // CBT scenario: each thought carries an emotion-embedding; the
    // clinician queries for thoughts close to an "anxious spiral" vector.
    let db = db();
    for (title, emb) in [
        ("racing_heart", &[0.9, 0.1, 0.0] as &[f64]),
        ("worst_case", &[0.85, 0.15, 0.0]),
        ("calm_walk", &[0.0, 0.1, 0.9]),
    ] {
        run(
            &format!(
                "CREATE (:Thought {{title: '{title}', emotion: [{}]}})",
                emb.iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            &db,
        );
    }
    let rows = run_with(
        "MATCH (t:Thought) WHERE similar(t.emotion, $q, 0.9) RETURN t.title AS t ORDER BY t.title",
        &db,
        query_param(&[0.9, 0.1, 0.0]),
    );
    assert_eq!(
        names(&rows),
        vec!["racing_heart".to_string(), "worst_case".to_string()]
    );
}

#[test]
fn graph_rag_expands_similarity_hit_into_neighbourhood() {
    // The canonical graph-RAG idiom: find documents similar to a query,
    // then traverse a `cites` edge to pull in connected context — one
    // query mixing vector search and graph traversal.
    let db = db();
    create_doc(&db, "seed", &[1.0, 0.0]);
    create_doc(&db, "context", &[0.0, 1.0]); // dissimilar on its own…
    run(
        "MATCH (a:Doc {title: 'seed'}), (b:Doc {title: 'context'}) CREATE (a)-[:cites]->(b)",
        &db,
    );
    let rows = run_with(
        "MATCH (a:Doc)-[:cites]->(b:Doc) WHERE similar(a.embedding, $q, 0.9) RETURN b.title AS t",
        &db,
        query_param(&[1.0, 0.0]),
    );
    // …but it is reached because its citing document matched the query.
    assert_eq!(names(&rows), vec!["context".to_string()]);
}

#[test]
fn similar_composes_with_other_where_predicates() {
    let db = db();
    run(
        "CREATE (:Doc {title: 'published', status: 'public', embedding: [1.0, 0.0]})",
        &db,
    );
    run(
        "CREATE (:Doc {title: 'draft', status: 'private', embedding: [1.0, 0.0]})",
        &db,
    );
    let rows = run_with(
        "MATCH (d:Doc) WHERE similar(d.embedding, $q, 0.9) AND d.status = 'public' RETURN d.title AS t",
        &db,
        query_param(&[1.0, 0.0]),
    );
    assert_eq!(names(&rows), vec!["published".to_string()]);
}
