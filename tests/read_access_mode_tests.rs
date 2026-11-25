//! READ access mode (issue #825): a session or transaction that asks for READ
//! access may read, search and run read procedures, and nothing else. A write,
//! a schema change or a procedure with effects outside the transaction (a chat
//! model, the embedding service, events, triggers, the server's state) is
//! refused with `Neo.ClientError.Statement.AccessMode` before it runs, and the
//! graph is left as it was.
//!
//! The service API takes the mode per statement; Bolt reads it from the
//! `mode: "r"` the drivers send on `BEGIN` / `RUN`, and `POST /cypher` from
//! `access: "read"`.

#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{Access, ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::embeddings::{EmbeddingsError, TextEmbedder};
use drevo::llm::{ChatCompleter, ChatRequest, LlmError};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Serialises the tests that install the process-wide chat model.
static CHAT_MODEL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const GRAPH: &str = "CREATE (a:Person {name: 'Ada', bio: 'wrote the first program'}), \
                            (b:Person {name: 'Bob', bio: 'builds bridges'}), \
                            (a)-[:KNOWS {since: 2020}]->(b)";

fn graph() -> NativeService {
    let svc = NativeService::in_memory();
    svc.execute(&parse(GRAPH).unwrap(), HashMap::new())
        .expect("seed");
    svc
}

fn read(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    let query = parse(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    svc.execute_with_access(&query, HashMap::new(), None, Access::Read)
}

fn write(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    let query = parse(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    svc.execute_with_access(&query, HashMap::new(), None, Access::Write)
}

/// `(nodes, relationships, the properties of every node and relationship)`.
fn snapshot(svc: &NativeService) -> String {
    let q = "MATCH (n) OPTIONAL MATCH (n)-[r]->() \
             RETURN count(DISTINCT n) AS nodes, count(r) AS rels, \
                    collect(DISTINCT properties(n)) AS np, collect(properties(r)) AS rp";
    format!("{:?}", write(svc, q).expect("snapshot").rows)
}

fn assert_access_mode(result: Result<ExecResult, ExecError>, what: &str) {
    match result {
        Err(ExecError::AccessMode { message }) => {
            assert!(
                message.contains("READ"),
                "{what}: the refusal must say the session is READ: {message}"
            );
        }
        other => panic!("{what}: expected an AccessMode refusal, got {other:?}"),
    }
}

#[test]
fn reads_work_in_read_mode() {
    let svc = graph();
    let rows = |q: &str| read(&svc, q).unwrap_or_else(|e| panic!("`{q}`: {e}")).rows;
    assert_eq!(
        rows("MATCH (p:Person) RETURN p.name ORDER BY p.name"),
        vec![
            vec![Value::String("Ada".into())],
            vec![Value::String("Bob".into())]
        ]
    );
    assert_eq!(
        rows("MATCH (:Person)-[k:KNOWS]->(b) RETURN b.name, k.since"),
        vec![vec![Value::String("Bob".into()), Value::Integer(2020)]]
    );
    assert_eq!(
        rows("CALL fts.search('bridges', 5) YIELD node RETURN node.name").len(),
        1
    );
    assert!(!rows("CALL db.labels() YIELD label RETURN label").is_empty());
    assert_eq!(
        rows("CALL drevo.info() YIELD version RETURN version").len(),
        1
    );
    assert_eq!(
        rows("CALL drevo.pagerank() YIELD node, score RETURN node, score").len(),
        2
    );
    assert_eq!(
        rows("MATCH (p:Person) WITH p.name AS n RETURN n UNION MATCH (p:Person) RETURN p.bio AS n")
            .len(),
        4
    );
    assert_eq!(
        rows("CALL drevo.search.hybrid('bridges', 'Person', 'vec', 5) YIELD node RETURN node.name")
            .len(),
        1,
        "hybrid search without an embedder is a full-text read"
    );
}

#[test]
fn every_write_clause_is_refused_and_nothing_changes() {
    let svc = graph();
    let before = snapshot(&svc);
    for q in [
        "CREATE (:Person {name: 'Eve'})",
        "MATCH (a:Person {name: 'Ada'}) CREATE (a)-[:KNOWS]->(:Person {name: 'Eve'})",
        "MERGE (:Person {name: 'Eve'})",
        "MERGE (p:Person {name: 'Ada'}) ON MATCH SET p.seen = true",
        "MATCH (p:Person {name: 'Ada'}) SET p.age = 36",
        "MATCH (p:Person {name: 'Ada'}) SET p:Admin",
        "MATCH (p:Person {name: 'Ada'}) REMOVE p.bio",
        "MATCH (p:Person {name: 'Bob'}) DETACH DELETE p",
        "MATCH ()-[k:KNOWS]->() DELETE k",
        "UNWIND [1, 2] AS i FOREACH (x IN [i] | CREATE (:N {x: x}))",
        "MATCH (p:Person) RETURN p.name AS n UNION CREATE (q:Person {name: 'Eve'}) RETURN q.name AS n",
        "MATCH (p:Person {name: 'Ada'}) WITH p CREATE (:Note {about: p.name}) RETURN count(*)",
    ] {
        assert_access_mode(read(&svc, q), q);
    }
    assert_eq!(
        snapshot(&svc),
        before,
        "a refused statement changes nothing"
    );
}

#[test]
fn schema_changes_are_refused() {
    let svc = graph();
    for q in [
        "CREATE INDEX person_name FOR (p:Person) ON (p.name)",
        "CREATE CONSTRAINT person_name FOR (p:Person) REQUIRE p.name IS UNIQUE",
        "CREATE VECTOR INDEX person_vec FOR (p:Person) ON (p.vec)",
        "DROP INDEX person_name IF EXISTS",
    ] {
        assert_access_mode(read(&svc, q), q);
    }
    let indexes = write(&svc, "SHOW INDEXES").expect("show").rows.len();
    assert_eq!(
        read(&svc, "SHOW INDEXES")
            .expect("SHOW INDEXES is a read")
            .rows
            .len(),
        indexes
    );
}

#[test]
fn procedures_with_effects_are_refused() {
    let svc = graph();
    let before = snapshot(&svc);
    for (q, name) in [
        (
            "CALL drevo.memory.addMessage('s1', 'user', 'hi') YIELD id RETURN id",
            "drevo.memory.addMessage",
        ),
        (
            "CALL drevo.memory.rememberEntity('Ada', 'Person', {}) YIELD id RETURN id",
            "drevo.memory.rememberEntity",
        ),
        (
            "CALL drevo.events.post('topic', {x: 1}) YIELD id RETURN id",
            "drevo.events.post",
        ),
        (
            "CALL apoc.trigger.install('drevo', 't', 'RETURN 1', {phase: 'after'})",
            "apoc.trigger.install",
        ),
        ("CALL drevo.sequence.create('s')", "drevo.sequence.create"),
        (
            "CALL drevo.rag.context('who wrote the first program?') YIELD text RETURN text",
            "drevo.rag.context",
        ),
        (
            "CALL drevo.rag.pruneTraces(0) YIELD deleted RETURN deleted",
            "drevo.rag.pruneTraces",
        ),
        (
            "CALL drevo.cypher.fromText('who is Ada?') YIELD cypher RETURN cypher",
            "drevo.cypher.fromText",
        ),
        (
            "CALL drevo.extract.entities('Ada met Bob.') YIELD name RETURN name",
            "drevo.extract.entities",
        ),
        (
            "CALL drevo.extract.ingest('Ada met Bob.') YIELD node RETURN node",
            "drevo.extract.ingest",
        ),
        (
            "CALL drevo.semantic.embed('Ada') YIELD vector RETURN vector",
            "drevo.semantic.embed",
        ),
        (
            "CALL drevo.community.summarize() YIELD community RETURN community",
            "drevo.community.summarize",
        ),
        (
            "CALL dbms.killConnections(['bolt-1']) YIELD connectionId RETURN connectionId",
            "dbms.killConnections",
        ),
        (
            "CALL drevo.tx.commitPrepared('g') YIELD gid RETURN gid",
            "drevo.tx.commitPrepared",
        ),
    ] {
        match read(&svc, q) {
            Err(ExecError::AccessMode { message }) => assert!(
                message.contains(name),
                "the refusal of `{q}` must name {name}: {message}"
            ),
            other => panic!("`{q}`: expected an AccessMode refusal, got {other:?}"),
        }
    }
    assert_eq!(snapshot(&svc), before);
}

/// A chat model that counts its calls and answers with no entities.
struct CountingModel(Arc<AtomicUsize>);

impl ChatCompleter for CountingModel {
    fn complete(&self, _: &ChatRequest) -> Result<String, LlmError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(r#"{"entities": [], "facts": []}"#.into())
    }
}

#[test]
fn an_extraction_in_read_mode_never_reaches_the_model() {
    let _model = CHAT_MODEL.lock().unwrap_or_else(|e| e.into_inner());
    let calls = Arc::new(AtomicUsize::new(0));
    drevo::extract::install(Arc::new(CountingModel(Arc::clone(&calls))));
    let svc = graph();
    for q in [
        "CALL drevo.extract.entities('Ada met Bob in London.') YIELD name RETURN name",
        "CALL drevo.extract.facts('Ada met Bob in London.') YIELD subject RETURN subject",
        "CALL drevo.extract.ingest('Ada met Bob in London.') YIELD node RETURN node",
        "CALL drevo.cypher.fromText('who is Ada?') YIELD cypher RETURN cypher",
        "CALL drevo.community.summarize({mode: 'llm'}) YIELD community RETURN community",
    ] {
        assert_access_mode(read(&svc, q), q);
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "READ mode must not call the model"
    );
    // The same call with write access does reach it: the fixture counts.
    let written = write(
        &svc,
        "CALL drevo.extract.entities('Ada met Bob in London.') YIELD name RETURN name",
    );
    assert!(written.is_ok(), "{written:?}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "write access calls the model"
    );
    drevo::extract::uninstall();
}

/// An embedder that counts its calls.
struct CountingEmbedder(Arc<AtomicUsize>);

impl TextEmbedder for CountingEmbedder {
    fn embed_query(&self, _text: &str) -> Result<Vec<f32>, EmbeddingsError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![1.0, 0.0, 0.0])
    }
}

#[test]
fn a_read_that_would_embed_text_is_refused_before_the_embedder_runs() {
    let svc = graph();
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(svc.set_embedder(Arc::new(CountingEmbedder(Arc::clone(&calls)))));
    let q = "CALL drevo.semantic.query('Person', 'vec', 'who builds bridges?', 3) \
             YIELD node RETURN node";
    assert_access_mode(read(&svc, q), q);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the embedder was called");
    write(&svc, q).expect("with write access the query embeds and runs");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // A read that needs no embedding still works with the embedder installed.
    assert_eq!(
        read(&svc, "MATCH (p:Person) RETURN count(p)").unwrap().rows,
        vec![vec![Value::Integer(2)]]
    );
}

#[test]
fn a_read_transaction_refuses_writes_and_keeps_reading() {
    let svc = graph();
    let before = snapshot(&svc);
    let tx = svc.begin_tx();
    let in_tx = |q: &str, access: Access| {
        svc.execute_in_tx_with_access(tx, &parse(q).unwrap(), HashMap::new(), None, access)
    };
    assert_eq!(
        in_tx("MATCH (p:Person) RETURN count(p)", Access::Read)
            .unwrap()
            .rows,
        vec![vec![Value::Integer(2)]]
    );
    assert_access_mode(
        in_tx("CREATE (:Person {name: 'Eve'})", Access::Read),
        "CREATE in a READ transaction",
    );
    assert_access_mode(
        in_tx(
            "CALL drevo.memory.addMessage('s', 'user', 'x') YIELD id RETURN id",
            Access::Read,
        ),
        "a writing procedure in a READ transaction",
    );
    assert_eq!(
        in_tx("MATCH (p:Person) RETURN count(p)", Access::Read)
            .unwrap()
            .rows,
        vec![vec![Value::Integer(2)]]
    );
    svc.rollback_tx(tx);
    assert_eq!(snapshot(&svc), before);
}

#[test]
fn write_access_is_unchanged() {
    let svc = graph();
    write(&svc, "CREATE (:Person {name: 'Eve'})").expect("write access writes");
    let q = parse("CREATE (:Person {name: 'Fay'})").unwrap();
    svc.execute(&q, HashMap::new())
        .expect("execute writes as before");
    assert_eq!(
        read(&svc, "MATCH (p:Person) RETURN count(p)").unwrap().rows,
        vec![vec![Value::Integer(4)]]
    );
}

// ── Bolt: the `mode` the drivers send ───────────────────────────────────────

fn session(svc: &Arc<NativeService>) -> Session<'static> {
    let mut s = Session::new_durable(Arc::clone(svc));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    s
}

fn mode(m: &str) -> BTreeMap<String, Bolt> {
    BTreeMap::from([("mode".to_string(), Bolt::String(m.to_string()))])
}

fn run(s: &mut Session, query: &str, extra: BTreeMap<String, Bolt>) -> Vec<ServerMessage> {
    let reply = s.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: BTreeMap::new(),
        extra,
    });
    if reply
        .iter()
        .any(|m| matches!(m, ServerMessage::Success { .. }))
    {
        let mut pull = BTreeMap::new();
        pull.insert("n".to_string(), Bolt::Integer(-1));
        let mut all = reply;
        all.extend(s.handle(ClientMessage::Pull { extra: pull }));
        return all;
    }
    reply
}

fn failure_code(reply: &[ServerMessage]) -> Option<String> {
    reply.iter().find_map(|m| match m {
        ServerMessage::Failure { metadata } => match metadata.get("code") {
            Some(Bolt::String(code)) => Some(code.clone()),
            _ => None,
        },
        _ => None,
    })
}

fn records(reply: &[ServerMessage]) -> usize {
    reply
        .iter()
        .filter(|m| matches!(m, ServerMessage::Record { .. }))
        .count()
}

const ACCESS_MODE: &str = "Neo.ClientError.Statement.AccessMode";

#[test]
fn bolt_an_autocommit_run_in_read_mode_refuses_a_write() {
    let svc = Arc::new(graph());
    let mut s = session(&svc);
    let reply = run(&mut s, "CREATE (:Person {name: 'Eve'})", mode("r"));
    assert_eq!(
        failure_code(&reply).as_deref(),
        Some(ACCESS_MODE),
        "{reply:?}"
    );
    s.handle(ClientMessage::Reset);
    let reply = run(&mut s, "MATCH (p:Person) RETURN p.name", mode("r"));
    assert_eq!(failure_code(&reply), None, "{reply:?}");
    assert_eq!(records(&reply), 2);
    for extra in [mode("w"), BTreeMap::new()] {
        let reply = run(&mut s, "CREATE (:Person {name: 'Eve'})", extra);
        assert_eq!(failure_code(&reply), None, "write mode writes: {reply:?}");
    }
}

#[test]
fn bolt_a_read_transaction_refuses_a_write_and_a_writing_procedure() {
    let svc = Arc::new(graph());
    let mut s = session(&svc);
    let begun = s.handle(ClientMessage::Begin { extra: mode("r") });
    assert_eq!(failure_code(&begun), None, "{begun:?}");
    let reply = run(&mut s, "MATCH (p:Person) RETURN p.name", BTreeMap::new());
    assert_eq!(records(&reply), 2, "{reply:?}");
    let reply = run(&mut s, "MERGE (:Person {name: 'Eve'})", BTreeMap::new());
    assert_eq!(
        failure_code(&reply).as_deref(),
        Some(ACCESS_MODE),
        "{reply:?}"
    );
    s.handle(ClientMessage::Reset);
    let begun = s.handle(ClientMessage::Begin { extra: mode("r") });
    assert_eq!(failure_code(&begun), None, "{begun:?}");
    let reply = run(
        &mut s,
        "CALL drevo.memory.addMessage('s', 'user', 'hi') YIELD id RETURN id",
        BTreeMap::new(),
    );
    assert_eq!(
        failure_code(&reply).as_deref(),
        Some(ACCESS_MODE),
        "{reply:?}"
    );
    s.handle(ClientMessage::Reset);
    // A transaction begun for writing writes.
    s.handle(ClientMessage::Begin { extra: mode("w") });
    let reply = run(&mut s, "CREATE (:Person {name: 'Eve'})", BTreeMap::new());
    assert_eq!(failure_code(&reply), None, "{reply:?}");
    s.handle(ClientMessage::Commit);
    assert_eq!(
        svc.execute(
            &parse("MATCH (p:Person) RETURN count(p)").unwrap(),
            HashMap::new()
        )
        .unwrap()
        .rows,
        vec![vec![Value::Integer(3)]]
    );
}

#[test]
fn bolt_user_management_is_refused_in_read_mode() {
    let svc = Arc::new(graph());
    let mut s = session(&svc);
    let reply = run(
        &mut s,
        "CREATE USER eve SET PASSWORD 'secret123!'",
        mode("r"),
    );
    assert_eq!(
        failure_code(&reply).as_deref(),
        Some(ACCESS_MODE),
        "{reply:?}"
    );
}

#[test]
fn bolt_an_unknown_mode_is_refused() {
    let svc = Arc::new(graph());
    let mut s = session(&svc);
    let reply = run(&mut s, "RETURN 1", mode("x"));
    assert!(failure_code(&reply).is_some(), "{reply:?}");
}

// ── HTTP: `access` on POST /cypher ──────────────────────────────────────────

async fn post_cypher(
    app: &axum::Router,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/cypher")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn http_access_read_refuses_a_write_and_runs_a_read() {
    let app = build_native_router(NativeApiState::new(Arc::new(graph())));
    let (status, body) = post_cypher(
        &app,
        serde_json::json!({"query": "CREATE (:Person {name: 'Eve'})", "access": "read"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], ACCESS_MODE, "{body}");
    let (status, body) = post_cypher(
        &app,
        serde_json::json!({"query": "MATCH (p:Person) RETURN count(p) AS n", "access": "read"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"][0][0], 2, "{body}");
    let (status, body) = post_cypher(
        &app,
        serde_json::json!({"query": "CREATE (:Person {name: 'Eve'})", "access": "write"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = post_cypher(
        &app,
        serde_json::json!({"query": "CREATE (:Person {name: 'Fay'})"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "no access field writes as before");
}

#[tokio::test]
async fn http_access_read_refuses_administration_and_a_bad_value() {
    let app = build_native_router(NativeApiState::new(Arc::new(graph())));
    for query in [
        "CREATE USER eve SET PASSWORD 'secret123!'",
        "CREATE DATABASE other",
    ] {
        let (status, body) =
            post_cypher(&app, serde_json::json!({"query": query, "access": "read"})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "`{query}`: {body}");
        assert_eq!(body["code"], ACCESS_MODE, "`{query}`: {body}");
    }
    let (status, body) = post_cypher(
        &app,
        serde_json::json!({"query": "RETURN 1", "access": "readonly"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
