//! Neo4j-compatible vector-index surface over the native engine (issue #532):
//! `CREATE VECTOR INDEX … FOR (n:Label) ON (n.prop)` + `CALL
//! db.index.vector.queryNodes(name, k, vec)`, the no-op schema DDL (`CREATE
//! INDEX` / `CREATE CONSTRAINT`), and the `db.index.fulltext.queryNodes` alias.
//!
//! These run through [`NativeService::execute`], which wires the durable
//! semantic control plane (the vector-index registry lives there) and the
//! full-text index — so this exercises the real name→(label, property)
//! resolution and, for the durable path, that a registration survives a reopen.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
}

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    run(svc, q).unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

/// Three orthogonal unit embeddings, so the nearest neighbour of a query is
/// unambiguous.
fn seed(svc: &NativeService) {
    run_ok(
        svc,
        "CREATE (:Chunk {title: 'a', embedding: [1.0, 0.0, 0.0]})",
    );
    run_ok(
        svc,
        "CREATE (:Chunk {title: 'b', embedding: [0.0, 1.0, 0.0]})",
    );
    run_ok(
        svc,
        "CREATE (:Chunk {title: 'c', embedding: [0.0, 0.0, 1.0]})",
    );
}

/// The `title` column of a `RETURN node.title AS title, score` result, in row
/// order.
fn titles(res: &ExecResult) -> Vec<String> {
    res.rows
        .iter()
        .map(|r| match &r[0] {
            Value::String(t) => t.clone(),
            other => panic!("expected a title string, got {other:?}"),
        })
        .collect()
}

const QUERY: &str = "CALL db.index.vector.queryNodes('chunks', 3, [0.9, 0.1, 0.0]) \
     YIELD node, score RETURN node.title AS title, score ORDER BY score DESC";

#[test]
fn create_vector_index_then_query_by_name_ranks_nearest_first() {
    let svc = NativeService::in_memory();
    seed(&svc);
    let created = run_ok(
        &svc,
        "CREATE VECTOR INDEX chunks FOR (c:Chunk) ON (c.embedding)",
    );
    assert!(created.rows.is_empty(), "schema DDL returns no rows");

    let res = run_ok(&svc, QUERY);
    // The query vector points mostly along 'a' ([1,0,0]); it must rank first.
    assert_eq!(titles(&res), vec!["a", "b", "c"]);
}

#[test]
fn query_nodes_matches_drevo_vector_query_exactly() {
    let svc = NativeService::in_memory();
    seed(&svc);
    run_ok(
        &svc,
        "CREATE VECTOR INDEX chunks FOR (c:Chunk) ON (c.embedding)",
    );

    // The Neo4j-named surface resolves to the same (label, property) and runs
    // the same cosine scan as the drevo-native procedure — identical rows.
    let via_named = run_ok(&svc, QUERY);
    let via_native = run_ok(
        &svc,
        "CALL drevo.vector.query('Chunk', 'embedding', [0.9, 0.1, 0.0], 3) \
         YIELD node, score RETURN node.title AS title, score ORDER BY score DESC",
    );
    assert_eq!(titles(&via_named), titles(&via_native));
}

#[test]
fn if_not_exists_is_idempotent_but_a_bare_duplicate_errors() {
    let svc = NativeService::in_memory();
    seed(&svc);
    run_ok(
        &svc,
        "CREATE VECTOR INDEX dup FOR (c:Chunk) ON (c.embedding)",
    );
    // IF NOT EXISTS on the same name is a no-op.
    run_ok(
        &svc,
        "CREATE VECTOR INDEX dup IF NOT EXISTS FOR (c:Chunk) ON (c.embedding)",
    );
    // A bare re-create of an existing name is an error.
    let err = run(
        &svc,
        "CREATE VECTOR INDEX dup FOR (c:Chunk) ON (c.embedding)",
    )
    .unwrap_err();
    assert!(
        matches!(err, ExecError::InvalidMutation(ref m) if m.contains("already exists")),
        "got {err:?}"
    );
}

#[test]
fn querying_an_unknown_index_name_errors() {
    let svc = NativeService::in_memory();
    seed(&svc);
    let err = run(
        &svc,
        "CALL db.index.vector.queryNodes('nope', 3, [1.0, 0.0, 0.0]) YIELD node RETURN node",
    )
    .unwrap_err();
    assert!(
        matches!(err, ExecError::InvalidProcedureCall { ref message, .. } if message.contains("no vector index named")),
        "got {err:?}"
    );
}

#[test]
fn non_vector_schema_ddl_is_accepted_as_a_noop() {
    let svc = NativeService::in_memory();
    seed(&svc);
    // A driver's schema bootstrap (drevo auto-indexes, so these do nothing) must
    // not fail — each returns an empty result rather than a parse/exec error.
    for stmt in [
        "CREATE INDEX chunk_title IF NOT EXISTS FOR (c:Chunk) ON (c.title)",
        "CREATE RANGE INDEX chunk_title2 FOR (c:Chunk) ON (c.title)",
        "CREATE FULLTEXT INDEX chunk_ft FOR (c:Chunk) ON EACH [c.title]",
        "CREATE CONSTRAINT chunk_uniq IF NOT EXISTS FOR (c:Chunk) REQUIRE c.title IS UNIQUE",
    ] {
        let res = run_ok(&svc, stmt);
        assert!(res.rows.is_empty(), "`{stmt}` should be a no-op");
    }
    // The graph is untouched: still exactly the three seeded chunks.
    let count = run_ok(&svc, "MATCH (c:Chunk) RETURN count(c) AS n");
    assert_eq!(count.rows[0][0], Value::Integer(3));
}

#[test]
fn db_index_fulltext_query_nodes_finds_by_text() {
    let svc = NativeService::in_memory();
    // A distinctive token so BM25 matches exactly this node.
    run_ok(
        &svc,
        "CREATE (:Doc {title: 'note', body: 'the zqxmarker appears here'})",
    );
    let res = run_ok(
        &svc,
        "CALL db.index.fulltext.queryNodes('anyName', 'zqxmarker') \
         YIELD node, score RETURN node.title AS title, score",
    );
    assert_eq!(titles(&res), vec!["note"]);
}

// ---- durability: a registration survives a reopen ------------------------

static NEXT: AtomicU64 = AtomicU64::new(0);

fn tmp_wal() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "drevo_vidx_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("native.wal")
}

#[test]
fn vector_index_registration_survives_reopen() {
    let wal = tmp_wal();
    {
        let svc = NativeService::open(&wal).unwrap();
        seed(&svc);
        run_ok(
            &svc,
            "CREATE VECTOR INDEX chunks FOR (c:Chunk) ON (c.embedding)",
        );
    }
    // A fresh service over the same WAL dir reloads the registry from the
    // sidecar; the named index still resolves to (Chunk, embedding).
    let svc = NativeService::open(&wal).unwrap();
    let res = run_ok(&svc, QUERY);
    assert_eq!(titles(&res), vec!["a", "b", "c"]);
    let _ = std::fs::remove_dir_all(wal.parent().unwrap());
}
