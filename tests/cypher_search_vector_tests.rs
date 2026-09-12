//! Declarative vector search — the `SEARCH … IN (VECTOR INDEX …) SCORE AS …`
//! Cypher clause (issue #430), lowered to the same cosine scan as
//! `CALL drevo.vector.query`.
//!
//! First slice: the index is addressed as a dotted `Label.property`; a named
//! `CREATE VECTOR INDEX` registry is a follow-up.

use std::collections::HashMap;

use drevo::cypher::executor::{execute, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

/// A graph of 2-D movie-plot embeddings:
/// A=[1,0] (identical to the query), B=[0.9,0.1] (close), C=[0,1] (orthogonal).
fn seeded() -> Drevo {
    let d = Drevo::open_in_memory().expect("open");
    let seed = parse(
        "CREATE (:Movie {title: 'A', plotEmbedding: [1.0, 0.0]}), \
                (:Movie {title: 'B', plotEmbedding: [0.9, 0.1]}), \
                (:Movie {title: 'C', plotEmbedding: [0.0, 1.0]})",
    )
    .expect("parse seed");
    execute(&seed, &d, HashMap::new()).expect("seed");
    d
}

/// The query vector [1, 0] as a `$q` parameter.
fn query_param() -> HashMap<String, Value> {
    let mut p = HashMap::new();
    p.insert(
        "q".to_string(),
        Value::List(vec![Value::Float(1.0), Value::Float(0.0)]),
    );
    p
}

fn titles(rows: &[Vec<Value>], col: usize) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[col] {
            Value::String(s) => s.clone(),
            other => panic!("expected string title, got {other:?}"),
        })
        .collect()
}

#[test]
fn search_returns_top_k_ordered_by_similarity_with_score() {
    let d = seeded();
    let q = parse(
        "MATCH (n:Movie) \
         SEARCH n IN (VECTOR INDEX Movie.plotEmbedding FOR $q LIMIT 3) SCORE AS score \
         RETURN n.title AS title, score",
    )
    .expect("parse");
    let r = execute(&q, &d, query_param()).expect("execute");

    assert_eq!(r.columns, vec!["title".to_string(), "score".to_string()]);
    // Ordered by descending cosine similarity: A (1.0) > B (~0.994) > C (0.0).
    assert_eq!(titles(&r.rows, 0), ["A", "B", "C"]);

    // The score is a float in [0, 1], monotonically non-increasing, ~1.0 first.
    let score = |i: usize| match &r.rows[i][1] {
        Value::Float(f) => *f,
        other => panic!("expected float score, got {other:?}"),
    };
    assert!(
        (score(0) - 1.0).abs() < 1e-6,
        "best score is ~1.0: {}",
        score(0)
    );
    assert!(
        score(0) >= score(1) && score(1) >= score(2),
        "scores descend"
    );
    assert!(
        score(2).abs() < 1e-6,
        "orthogonal vector scores ~0: {}",
        score(2)
    );
}

#[test]
fn search_limit_bounds_cardinality_not_multiplied_by_match() {
    // MATCH (n:Movie) binds 3 rows; SEARCH … LIMIT 2 must yield 2 rows total,
    // not 2 × 3 — the search variable is overwritten, not cross-joined.
    let d = seeded();
    let q = parse(
        "MATCH (n:Movie) \
         SEARCH n IN (VECTOR INDEX Movie.plotEmbedding FOR $q LIMIT 2) SCORE AS score \
         RETURN n.title AS title",
    )
    .expect("parse");
    let r = execute(&q, &d, query_param()).expect("execute");
    assert_eq!(r.rows.len(), 2, "top-2 → exactly 2 rows");
    assert_eq!(titles(&r.rows, 0), ["A", "B"]);
}

#[test]
fn search_inner_where_filters_results() {
    let d = seeded();
    let q = parse(
        "MATCH (n:Movie) \
         SEARCH n IN (VECTOR INDEX Movie.plotEmbedding FOR $q WHERE n.title <> 'A' LIMIT 3) \
           SCORE AS score \
         RETURN n.title AS title",
    )
    .expect("parse");
    let r = execute(&q, &d, query_param()).expect("execute");
    assert_eq!(
        titles(&r.rows, 0),
        ["B", "C"],
        "A excluded by the inner WHERE"
    );
}

#[test]
fn search_without_score_as_binds_only_the_node() {
    let d = seeded();
    let q = parse(
        "MATCH (n:Movie) \
         SEARCH n IN (VECTOR INDEX Movie.plotEmbedding FOR $q LIMIT 1) \
         RETURN n.title AS title",
    )
    .expect("parse");
    let r = execute(&q, &d, query_param()).expect("execute");
    assert_eq!(titles(&r.rows, 0), ["A"]);
}

#[test]
fn search_matches_the_vector_query_procedure() {
    // The declarative clause and the procedure must agree exactly.
    let d = seeded();
    let via_clause = {
        let q = parse(
            "MATCH (n:Movie) \
             SEARCH n IN (VECTOR INDEX Movie.plotEmbedding FOR $q LIMIT 3) SCORE AS score \
             RETURN n.title AS title, score",
        )
        .expect("parse");
        execute(&q, &d, query_param()).expect("execute").rows
    };
    let via_proc = {
        let q = parse(
            "CALL drevo.vector.query('Movie', 'plotEmbedding', $q, 3) YIELD node, score \
             RETURN node.title AS title, score",
        )
        .expect("parse");
        execute(&q, &d, query_param()).expect("execute").rows
    };
    assert_eq!(
        titles(&via_clause, 0),
        titles(&via_proc, 0),
        "SEARCH and drevo.vector.query return the same nodes in the same order"
    );
}

#[test]
fn search_is_a_soft_keyword_property_named_search_still_works() {
    // `search` must remain usable as a property (not reserved).
    let d = Drevo::open_in_memory().expect("open");
    let seed = parse("CREATE (:Doc {title: 'd', search: 'hits'})").expect("parse seed");
    execute(&seed, &d, HashMap::new()).expect("seed");
    let q = parse("MATCH (n:Doc) RETURN n.search AS search").expect("parse");
    let r = execute(&q, &d, HashMap::new()).expect("execute");
    assert_eq!(
        match &r.rows[0][0] {
            Value::String(s) => s.as_str(),
            other => panic!("expected string, got {other:?}"),
        },
        "hits"
    );
}
