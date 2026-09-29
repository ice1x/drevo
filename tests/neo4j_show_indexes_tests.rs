//! The rest of the Neo4j-compatible schema surface (issue #532): `SHOW INDEXES`
//! / `SHOW CONSTRAINTS` and vector indexes over **relationships**
//! (`CREATE VECTOR INDEX … FOR ()-[r:TYPE]-() ON (r.prop)` +
//! `CALL db.index.vector.queryRelationships(name, k, vec)`).
//!
//! The query shapes are copied from the Neo4j GenAI clients that issue them:
//! `neo4j-agent-memory` checks `SHOW INDEXES YIELD name WHERE name = $name
//! RETURN count(*)` before every `CREATE … INDEX` of its schema bootstrap, and
//! `neo4j-graphrag` looks indexes up by type / label / property with
//! `SHOW INDEXES YIELD …, options WHERE type = 'VECTOR' AND …` and queries
//! relationship embeddings with `db.index.vector.queryRelationships`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run_with(
    svc: &NativeService,
    q: &str,
    params: &[(&str, Value)],
) -> Result<ExecResult, ExecError> {
    let params = params
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect::<HashMap<_, _>>();
    svc.execute(&parse(q).expect("parse"), params)
}

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    run_with(svc, q, &[]).unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn list(items: &[&str]) -> Value {
    Value::List(items.iter().map(|i| s(i)).collect())
}

/// One node vector index (`chunks` on `Chunk.embedding`) and one relationship
/// vector index (`similar_emb` on `SIMILAR.embedding`).
fn with_indexes() -> NativeService {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE VECTOR INDEX chunks FOR (c:Chunk) ON (c.embedding)",
    );
    run_ok(
        &svc,
        "CREATE VECTOR INDEX similar_emb IF NOT EXISTS FOR ()-[r:SIMILAR]-() ON (r.embedding) \
         OPTIONS {indexConfig: {`vector.dimensions`: 2}}",
    );
    svc
}

// ----- SHOW INDEXES -----------------------------------------------------------

#[test]
fn show_indexes_lists_node_and_relationship_vector_indexes() {
    let svc = with_indexes();
    let res = run_ok(
        &svc,
        "SHOW INDEXES YIELD name, type, entityType, labelsOrTypes, properties, state \
         RETURN name, type, entityType, labelsOrTypes, properties, state ORDER BY name",
    );
    assert_eq!(
        res.rows,
        vec![
            vec![
                s("chunks"),
                s("VECTOR"),
                s("NODE"),
                list(&["Chunk"]),
                list(&["embedding"]),
                s("ONLINE")
            ],
            vec![
                s("similar_emb"),
                s("VECTOR"),
                s("RELATIONSHIP"),
                list(&["SIMILAR"]),
                list(&["embedding"]),
                s("ONLINE"),
            ],
        ]
    );
}

#[test]
fn bare_show_indexes_projects_every_column() {
    let svc = with_indexes();
    let res = run_ok(&svc, "SHOW INDEXES");
    for col in [
        "name",
        "type",
        "entityType",
        "labelsOrTypes",
        "properties",
        "options",
    ] {
        assert!(
            res.columns.iter().any(|c| c == col),
            "missing column {col}: {:?}",
            res.columns
        );
    }
    assert_eq!(res.rows.len(), 2);
}

#[test]
fn agent_memory_existence_check() {
    // neo4j-agent-memory graph/client.py: checked before each CREATE … INDEX.
    let svc = with_indexes();
    let q = "SHOW INDEXES YIELD name WHERE name = $name RETURN count(*) AS count";
    let hit = run_with(&svc, q, &[("name", s("chunks"))]).unwrap();
    assert_eq!(hit.rows, vec![vec![Value::Integer(1)]]);
    let miss = run_with(&svc, q, &[("name", s("entity_embedding_idx"))]).unwrap();
    assert_eq!(miss.rows, vec![vec![Value::Integer(0)]]);
    // Same shape for constraints; drevo enforces none, so none are listed.
    let constraints = run_with(
        &svc,
        "SHOW CONSTRAINTS YIELD name WHERE name = $name RETURN count(*) AS count",
        &[("name", s("entity_id"))],
    )
    .unwrap();
    assert_eq!(constraints.rows, vec![vec![Value::Integer(0)]]);
}

#[test]
fn graphrag_vector_index_lookup() {
    // neo4j-graphrag indexes.py retrieve_vector_index_info, verbatim.
    let svc = with_indexes();
    let q = "SHOW INDEXES YIELD name, type, entityType, labelsOrTypes, \
             properties, options WHERE type = 'VECTOR' AND (name = $index_name \
             OR (labelsOrTypes[0] = $label_or_type AND \
             properties[0] = $embedding_property)) \
             RETURN name, type, entityType, labelsOrTypes, properties, options";
    // By label + property, under a name the client does not know.
    let res = run_with(
        &svc,
        q,
        &[
            ("index_name", s("vector_index")),
            ("label_or_type", s("Chunk")),
            ("embedding_property", s("embedding")),
        ],
    )
    .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0][0], s("chunks"));
    assert!(matches!(res.rows[0][5], Value::Map(_)), "options is a map");
}

#[test]
fn show_where_without_yield_and_type_filters() {
    let svc = with_indexes();
    let res = run_ok(&svc, "SHOW INDEXES WHERE name = 'chunks'");
    assert_eq!(res.rows.len(), 1);

    assert_eq!(run_ok(&svc, "SHOW VECTOR INDEXES").rows.len(), 2);
    assert_eq!(run_ok(&svc, "SHOW ALL INDEXES").rows.len(), 2);
    assert!(run_ok(&svc, "SHOW RANGE INDEXES").rows.is_empty());
    assert!(run_ok(&svc, "SHOW CONSTRAINTS").rows.is_empty());
}

// ----- relationship vector indexes --------------------------------------------

fn seed_similar(svc: &NativeService) {
    run_ok(
        svc,
        "CREATE (a:Doc {title: 'a'}), (b:Doc {title: 'b'}), (c:Doc {title: 'c'}), \
         (a)-[:SIMILAR {name: 'ab', embedding: [1.0, 0.0]}]->(b), \
         (b)-[:SIMILAR {name: 'bc', embedding: [0.6, 0.8]}]->(c), \
         (a)-[:SIMILAR {name: 'ac', embedding: [0.0, 1.0]}]->(c), \
         (a)-[:OTHER {name: 'other', embedding: [1.0, 0.0]}]->(c)",
    );
}

#[test]
fn query_relationships_ranks_edges_of_the_indexed_type() {
    let svc = with_indexes();
    seed_similar(&svc);
    let res = run_ok(
        &svc,
        "CALL db.index.vector.queryRelationships('similar_emb', 2, [1.0, 0.0]) \
         YIELD relationship, score RETURN relationship.name AS name, score",
    );
    let names: Vec<Value> = res.rows.iter().map(|r| r[0].clone()).collect();
    // Best-first, only :SIMILAR edges (the identical :OTHER edge is not indexed).
    assert_eq!(names, vec![s("ab"), s("bc")]);
    match res.rows[0][1] {
        Value::Float(score) => assert!((score - 1.0).abs() < 1e-6),
        ref other => panic!("expected a float score, got {other:?}"),
    }
}

#[test]
fn node_and_relationship_indexes_are_not_interchangeable() {
    let svc = with_indexes();
    let err = run_with(
        &svc,
        "CALL db.index.vector.queryNodes('similar_emb', 2, [1.0, 0.0]) YIELD node RETURN node",
        &[],
    )
    .expect_err("a relationship index cannot answer queryNodes");
    assert!(format!("{err}").contains("queryRelationships"), "{err}");

    let err = run_with(
        &svc,
        "CALL db.index.vector.queryRelationships('chunks', 2, [1.0, 0.0]) \
         YIELD relationship RETURN relationship",
        &[],
    )
    .expect_err("a node index cannot answer queryRelationships");
    assert!(format!("{err}").contains("queryNodes"), "{err}");
}

static NEXT: AtomicU64 = AtomicU64::new(0);

#[test]
fn relationship_index_survives_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "drevo_show_idx_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("native.wal");
    {
        let svc = NativeService::open(&wal).unwrap();
        run_ok(
            &svc,
            "CREATE VECTOR INDEX similar_emb FOR ()-[r:SIMILAR]->() ON (r.embedding)",
        );
        seed_similar(&svc);
    }
    let svc = NativeService::open(&wal).unwrap();
    let shown = run_ok(
        &svc,
        "SHOW INDEXES YIELD name, entityType RETURN name, entityType",
    );
    assert_eq!(shown.rows, vec![vec![s("similar_emb"), s("RELATIONSHIP")]]);
    let res = run_ok(
        &svc,
        "CALL db.index.vector.queryRelationships('similar_emb', 1, [1.0, 0.0]) \
         YIELD relationship RETURN relationship.name AS name",
    );
    assert_eq!(res.rows, vec![vec![s("ab")]]);
    let _ = std::fs::remove_dir_all(&dir);
}

// ----- CALL … YIELD as the last clause ----------------------------------------

#[test]
fn a_trailing_call_yield_returns_the_yielded_columns() {
    // Neo4j returns the yielded columns when a query ends in `CALL … YIELD`;
    // `SHOW … WHERE` relies on it.
    let svc = NativeService::in_memory();
    run_ok(&svc, "CREATE (:Alpha), (:Beta)");
    let res = run_ok(&svc, "CALL db.labels() YIELD label AS l");
    assert_eq!(res.columns, vec!["l".to_string()]);
    assert_eq!(res.rows, vec![vec![s("Alpha")], vec![s("Beta")]]);
}
