//! Procedures and schema commands inside an explicit transaction (#610).
//!
//! A statement in an explicit transaction — Bolt `BEGIN` … `COMMIT`, which is
//! how the Neo4j drivers run `execute_read` / `execute_write`, or a drevo-py
//! `Transaction` — runs on the transaction's snapshot with the server's
//! services attached:
//! - full-text, vector and fuzzy search, `keywords()`, `SHOW INDEXES` and
//!   `SHOW CONSTRAINTS` work, resolving hits against the snapshot;
//! - the full-text index covers committed data only, so after the
//!   transaction has written a full-text procedure fails with
//!   `ExecError::IndexBehindWrites` (as it does after a write earlier in
//!   the same statement);
//! - nodes created in the transaction are auto-embedded;
//! - schema commands, which take effect outside the transaction, fail with
//!   `ExecError::ForbiddenInTransaction`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native::NativeTxId;
use drevo::native_service::NativeService;

fn ok(svc: &NativeService, q: &str) -> ExecResult {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
        .unwrap_or_else(|e| panic!("`{q}`: {e:?}"))
}

fn in_tx(svc: &NativeService, tx: NativeTxId, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute_in_tx(tx, &parse(q).expect("parse"), HashMap::new())
}

fn ok_in_tx(svc: &NativeService, tx: NativeTxId, q: &str) -> ExecResult {
    in_tx(svc, tx, q).unwrap_or_else(|e| panic!("`{q}` in tx: {e:?}"))
}

fn strings(result: &ExecResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        })
        .collect()
}

/// Every string anywhere in the result — for `SHOW …` rows, where only the
/// presence of a name matters.
fn mentions(result: &ExecResult, needle: &str) -> bool {
    result
        .rows
        .iter()
        .flatten()
        .any(|v| matches!(v, Value::String(s) if s == needle))
}

struct MockEmbedder;

impl drevo::embeddings::TextEmbedder for MockEmbedder {
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, drevo::embeddings::EmbeddingsError> {
        if text.contains("rust") {
            Ok(vec![1.0, 0.0])
        } else {
            Ok(vec![0.0, 1.0])
        }
    }
}

// ── reads ────────────────────────────────────────────────────────────────

#[test]
fn full_text_search_runs_in_a_transaction_that_has_not_written() {
    let svc = NativeService::in_memory();
    ok(
        &svc,
        "CREATE (a:Doc {title: 'sourdough bread'}), (b:Doc {title: 'rust book'}) \
         CREATE (a)-[:SEE {note: 'zebra crossing'}]->(b) \
         CREATE (:Message {session: 's', body: 'zqxalpha said hello'})",
    );
    let tx = svc.begin_tx();
    let hits = ok_in_tx(
        &svc,
        tx,
        "CALL fts.search('bread', 5) YIELD node RETURN node.title",
    );
    assert_eq!(strings(&hits), ["sourdough bread"]);
    let hits = ok_in_tx(
        &svc,
        tx,
        "CALL db.index.fulltext.queryNodes('any', 'rust') YIELD node RETURN node.title",
    );
    assert_eq!(strings(&hits), ["rust book"]);
    let hits = ok_in_tx(
        &svc,
        tx,
        "CALL fts.searchRelationships('zebra', 5) YIELD rel RETURN rel.note",
    );
    assert_eq!(strings(&hits), ["zebra crossing"]);
    let hits = ok_in_tx(
        &svc,
        tx,
        "CALL drevo.memory.recall('s', 'zqxalpha', 5) YIELD node RETURN node.body",
    );
    assert_eq!(strings(&hits), ["zqxalpha said hello"]);
    let keywords = ok_in_tx(&svc, tx, "RETURN keywords('sourdough bread', 1) AS k");
    assert_eq!(keywords.rows.len(), 1);
    svc.commit_tx(tx).expect("read-only commit");
}

#[test]
fn full_text_hits_resolve_against_the_transaction_snapshot() {
    let svc = NativeService::in_memory();
    ok(&svc, "CREATE (:Doc {title: 'sourdough bread'})");
    let tx = svc.begin_tx();
    // Committed after the transaction began: indexed, but not in its snapshot.
    ok(&svc, "CREATE (:Doc {title: 'rye bread'})");
    let hits = ok_in_tx(
        &svc,
        tx,
        "CALL fts.search('bread', 5) YIELD node RETURN node.title",
    );
    assert_eq!(strings(&hits), ["sourdough bread"]);
    svc.rollback_tx(tx);
}

#[test]
fn full_text_search_after_the_transaction_wrote_names_the_cause() {
    let svc = NativeService::in_memory();
    ok(&svc, "CREATE (:Doc {title: 'sourdough bread'})");
    let tx = svc.begin_tx();
    ok_in_tx(&svc, tx, "CREATE (:Doc {title: 'rye bread'})");
    for (q, feature) in [
        (
            "CALL fts.search('bread', 5) YIELD node RETURN node",
            "fts.search",
        ),
        (
            "CALL db.index.fulltext.queryNodes('any', 'bread') YIELD node RETURN node",
            "db.index.fulltext.queryNodes",
        ),
        (
            "CALL fts.searchRelationships('bread', 5) YIELD rel RETURN rel",
            "fts.searchRelationships",
        ),
        (
            "CALL drevo.memory.recall('s', 'bread', 5) YIELD node RETURN node",
            "drevo.memory.recall",
        ),
    ] {
        let err = in_tx(&svc, tx, q).expect_err(q);
        assert!(
            matches!(&err, ExecError::IndexBehindWrites { feature: f } if f == feature),
            "{q}: {err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("committed"), "{q}: {message}");
        assert!(!message.contains("KV"), "{q}: {message}");
    }
    svc.rollback_tx(tx);
}

#[test]
fn full_text_search_after_a_write_in_the_same_statement_names_the_cause() {
    let svc = NativeService::in_memory();
    let err = svc
        .execute(
            &parse(
                "CREATE (:Doc {title: 'rye bread'}) WITH 1 AS one \
                 CALL fts.search('bread', 5) YIELD node RETURN node",
            )
            .expect("parse"),
            HashMap::new(),
        )
        .expect_err("search after a write");
    assert!(
        matches!(&err, ExecError::IndexBehindWrites { feature } if feature == "fts.search"),
        "{err:?}"
    );
}

#[test]
fn show_indexes_and_constraints_list_the_schema_in_a_transaction() {
    let svc = NativeService::in_memory();
    ok(&svc, "CREATE TEXT INDEX doc_title FOR (n:Doc) ON (n.title)");
    ok(&svc, "CREATE VECTOR INDEX doc_vec FOR (n:Doc) ON (n.vec)");
    ok(
        &svc,
        "CREATE CONSTRAINT user_email FOR (n:User) REQUIRE n.email IS UNIQUE",
    );
    let tx = svc.begin_tx();
    let indexes = ok_in_tx(&svc, tx, "SHOW INDEXES");
    assert!(mentions(&indexes, "doc_title"), "{:?}", indexes.rows);
    assert!(mentions(&indexes, "doc_vec"), "{:?}", indexes.rows);
    let constraints = ok_in_tx(&svc, tx, "SHOW CONSTRAINTS");
    assert!(
        mentions(&constraints, "user_email"),
        "{:?}",
        constraints.rows
    );
    svc.rollback_tx(tx);
}

#[test]
fn vector_and_fuzzy_search_read_the_transaction_s_own_writes() {
    let svc = NativeService::in_memory();
    ok(&svc, "CREATE VECTOR INDEX doc_vec FOR (n:Doc) ON (n.vec)");
    ok(&svc, "CREATE TEXT INDEX doc_title FOR (n:Doc) ON (n.title)");
    ok(&svc, "CREATE (:Doc {title: 'committed', vec: [0.0, 1.0]})");
    let tx = svc.begin_tx();
    ok_in_tx(&svc, tx, "CREATE (:Doc {title: 'mine', vec: [1.0, 0.0]})");
    let hits = ok_in_tx(
        &svc,
        tx,
        "CALL db.index.vector.queryNodes('doc_vec', 1, [1.0, 0.0]) YIELD node RETURN node.title",
    );
    assert_eq!(strings(&hits), ["mine"]);
    let hits = ok_in_tx(
        &svc,
        tx,
        "CALL drevo.text.fuzzy('doc_title', 'mina', 1) YIELD node RETURN node.title",
    );
    assert_eq!(strings(&hits), ["mine"]);
    svc.rollback_tx(tx);
}

#[test]
fn semantic_query_and_status_run_in_a_transaction() {
    let svc = NativeService::in_memory();
    assert!(svc.set_embedder(Arc::new(MockEmbedder)));
    ok(
        &svc,
        "CALL drevo.semantic.register('Doc', 'text', 'emb', 'auto')",
    );
    ok(&svc, "CREATE (:Doc {title: 'a', text: 'rust ownership'})");
    let tx = svc.begin_tx();
    let status = ok_in_tx(
        &svc,
        tx,
        "CALL drevo.semantic.status() YIELD label RETURN label",
    );
    assert_eq!(strings(&status), ["Doc"]);
    let hits = ok_in_tx(
        &svc,
        tx,
        "CALL drevo.semantic.query('Doc', 'emb', 'rust', 1) YIELD node RETURN node.title",
    );
    assert_eq!(strings(&hits), ["a"]);
    svc.rollback_tx(tx);
}

// ── writes ───────────────────────────────────────────────────────────────

#[test]
fn nodes_created_in_a_transaction_are_auto_embedded() {
    let svc = NativeService::in_memory();
    assert!(svc.set_embedder(Arc::new(MockEmbedder)));
    ok(
        &svc,
        "CALL drevo.semantic.register('Doc', 'text', 'emb', 'auto')",
    );
    let tx = svc.begin_tx();
    ok_in_tx(
        &svc,
        tx,
        "CREATE (:Doc {title: 'a', text: 'rust ownership'})",
    );
    svc.commit_tx(tx).expect("commit");
    let emb = ok(&svc, "MATCH (n:Doc {title: 'a'}) RETURN n.emb");
    assert_eq!(
        emb.rows,
        vec![vec![Value::List(vec![
            Value::Float(1.0),
            Value::Float(0.0)
        ])]]
    );
}

#[test]
fn schema_commands_are_refused_in_a_transaction_and_change_nothing() {
    let svc = NativeService::in_memory();
    assert!(svc.set_embedder(Arc::new(MockEmbedder)));
    ok(&svc, "CREATE TEXT INDEX keep_idx FOR (n:Doc) ON (n.title)");
    ok(
        &svc,
        "CREATE CONSTRAINT keep_c FOR (n:User) REQUIRE n.email IS UNIQUE",
    );
    for (q, procedure) in [
        (
            "CREATE INDEX new_idx FOR (n:Doc) ON (n.meta.author)",
            "CREATE INDEX",
        ),
        (
            "CREATE TEXT INDEX new_txt FOR (n:Doc) ON (n.body)",
            "CREATE INDEX",
        ),
        (
            "CREATE VECTOR INDEX new_vec FOR (n:Doc) ON (n.vec)",
            "CREATE VECTOR INDEX",
        ),
        ("DROP INDEX keep_idx", "DROP INDEX"),
        (
            "CREATE CONSTRAINT new_c FOR (n:User) REQUIRE n.name IS UNIQUE",
            "CREATE CONSTRAINT",
        ),
        ("DROP CONSTRAINT keep_c", "DROP CONSTRAINT"),
        (
            "CALL drevo.semantic.register('Doc', 'text', 'emb', 'auto')",
            "drevo.semantic.register",
        ),
        (
            "CALL drevo.semantic.registerRel('SEE', 'note', 'emb', 'auto')",
            "drevo.semantic.registerRel",
        ),
        (
            "CALL drevo.semantic.reindex('Doc', 'emb', 10)",
            "drevo.semantic.reindex",
        ),
        (
            "CALL drevo.semantic.reindexRel('SEE', 'emb', 10)",
            "drevo.semantic.reindexRel",
        ),
    ] {
        let tx = svc.begin_tx();
        let err = in_tx(&svc, tx, q).expect_err(q);
        assert!(
            matches!(&err, ExecError::ForbiddenInTransaction { procedure: p, .. } if p == procedure),
            "{q}: {err:?}"
        );
        assert!(err.to_string().contains("auto-commit"), "{q}: {err}");
        svc.rollback_tx(tx);
    }
    let indexes = ok(&svc, "SHOW INDEXES");
    assert!(mentions(&indexes, "keep_idx"));
    for name in ["new_idx", "new_txt", "new_vec"] {
        assert!(!mentions(&indexes, name), "{name}: {:?}", indexes.rows);
    }
    let constraints = ok(&svc, "SHOW CONSTRAINTS");
    assert!(mentions(&constraints, "keep_c"));
    assert!(!mentions(&constraints, "new_c"));
    let status = ok(
        &svc,
        "CALL drevo.semantic.status() YIELD label RETURN label",
    );
    assert!(status.rows.is_empty(), "{:?}", status.rows);
}

// ── capability error ─────────────────────────────────────────────────────

#[test]
fn the_capability_error_does_not_blame_a_storage_engine() {
    let g = drevo::native::NativeGraph::new();
    let err = drevo::cypher::executor::execute_on_engine(
        &parse("CALL fts.search('x', 5)").expect("parse"),
        &g,
        HashMap::new(),
    )
    .expect_err("no index stack");
    assert!(matches!(err, ExecError::EngineCapability { .. }), "{err:?}");
    let message = err.to_string();
    assert!(!message.contains("KV"), "{message}");
    assert!(!message.contains("storage engine"), "{message}");
}

// ── Bolt ─────────────────────────────────────────────────────────────────

const FORBIDDEN: &str = "Neo.ClientError.Transaction.ForbiddenDueToTransactionType";

fn hello(svc: &Arc<NativeService>) -> Session<'static> {
    let mut s = Session::new_durable(Arc::clone(svc));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    s
}

fn begin(s: &mut Session<'_>) {
    assert!(matches!(
        s.handle(ClientMessage::Begin {
            extra: BTreeMap::new()
        })[0],
        ServerMessage::Success { .. }
    ));
}

fn run(s: &mut Session<'_>, q: &str) -> Vec<ServerMessage> {
    let mut out = s.handle(ClientMessage::Run {
        query: q.to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    if matches!(out.first(), Some(ServerMessage::Success { .. })) {
        let mut n = BTreeMap::new();
        n.insert("n".to_string(), Bolt::Integer(-1));
        out.extend(s.handle(ClientMessage::Pull { extra: n }));
    }
    out
}

fn failure_code(replies: &[ServerMessage]) -> Option<String> {
    replies.iter().find_map(|m| match m {
        ServerMessage::Failure { metadata } => match metadata.get("code") {
            Some(Bolt::String(c)) => Some(c.clone()),
            _ => Some(String::new()),
        },
        _ => None,
    })
}

fn record_count(replies: &[ServerMessage]) -> usize {
    replies
        .iter()
        .filter(|m| matches!(m, ServerMessage::Record { .. }))
        .count()
}

#[test]
fn bolt_full_text_search_in_an_explicit_transaction() {
    let svc = Arc::new(NativeService::in_memory());
    ok(&svc, "CREATE (:Doc {title: 'sourdough bread'})");
    let mut s = hello(&svc);
    begin(&mut s);
    let hits = run(
        &mut s,
        "CALL db.index.fulltext.queryNodes('any', 'bread') YIELD node RETURN node.title",
    );
    assert_eq!(failure_code(&hits), None, "{hits:?}");
    assert_eq!(record_count(&hits), 1);
    let write = run(&mut s, "CREATE (:Doc {title: 'rye bread'})");
    assert_eq!(failure_code(&write), None, "{write:?}");
    let after = run(&mut s, "CALL fts.search('bread', 5) YIELD node RETURN node");
    assert_eq!(
        failure_code(&after).as_deref(),
        Some(FORBIDDEN),
        "{after:?}"
    );
}

#[test]
fn bolt_schema_command_in_an_explicit_transaction_is_a_client_error() {
    let svc = Arc::new(NativeService::in_memory());
    let mut s = hello(&svc);
    begin(&mut s);
    let replies = run(&mut s, "CREATE TEXT INDEX t FOR (n:Doc) ON (n.title)");
    assert_eq!(
        failure_code(&replies).as_deref(),
        Some(FORBIDDEN),
        "{replies:?}"
    );
}
