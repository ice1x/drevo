//! `drevo.extract.*`: entities and facts read from a text by a language
//! model, with every quote located in the text before it is believed.
//!
//! - `CALL drevo.extract.entities(text[, config]) YIELD name, type,
//!   description, offset, length, surface, confidence` and
//!   `CALL drevo.extract.facts(text[, config]) YIELD subject, subjectType,
//!   relation, object, objectType, quote, offset, length, confidence` read;
//! - `CALL drevo.extract.ingest(text, config) YIELD source, chunks, entities,
//!   facts, skipped` writes the source, its chunks, the entities they mention
//!   and the facts they state, with provenance, in one transaction;
//! - `CALL drevo.extract.chunks(text[, config])` shows the passages and
//!   `CALL drevo.extract.status()` the chat endpoint.
//!
//! The chat endpoint is a local HTTP server answering like an
//! OpenAI-compatible `chat/completions` upstream: for each passage it is
//! sent it reports the mentions of a fixed cast and the facts whose quote
//! the passage holds, with offsets into the passage — plus, when asked, a
//! hallucinated mention the passage does not hold. The scenario is a book
//! editor feeding manuscripts.

use std::collections::HashMap;

use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
}

fn run_with(
    svc: &NativeService,
    q: &str,
    params: &[(&str, Value)],
) -> Result<ExecResult, ExecError> {
    let params = params
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect();
    svc.execute(&parse(q).expect("parse"), params)
}

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    run(svc, q).unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

fn count(svc: &NativeService, q: &str) -> i64 {
    match run_ok(svc, q).rows[0][0] {
        Value::Integer(n) => n,
        ref other => panic!("expected a count, got {other:?}"),
    }
}

/// The `InvalidProcedureCall` message `q` fails with.
fn invalid_call(svc: &NativeService, q: &str) -> String {
    match run(svc, q) {
        Err(ExecError::InvalidProcedureCall { message, .. }) => message,
        other => panic!("`{q}`: expected InvalidProcedureCall, got {other:?}"),
    }
}

// ----- without a chat upstream ------------------------------------------------

#[test]
fn without_an_upstream_each_procedure_names_the_status_procedure() {
    drevo::extract::uninstall();
    let svc = NativeService::in_memory();
    for q in [
        "CALL drevo.extract.entities('Dana joined Acme.') YIELD name RETURN name",
        "CALL drevo.extract.facts('Dana joined Acme.', {}) YIELD quote RETURN quote",
        "CALL drevo.extract.ingest('Dana joined Acme.', {source: 'book-1'}) YIELD facts RETURN facts",
    ] {
        match run(&svc, q) {
            Err(ExecError::EngineCapability { feature }) => {
                assert!(
                    feature.contains("CALL drevo.extract.status()"),
                    "{q}: {feature}"
                );
                assert!(feature.contains("DREVO_EXTRACT_UPSTREAM"), "{q}: {feature}");
            }
            other => panic!("`{q}`: expected the capability error, got {other:?}"),
        }
    }
    assert_eq!(count(&svc, "MATCH (n) RETURN count(n)"), 0);
}

#[test]
fn status_reports_no_upstream() {
    drevo::extract::uninstall();
    let svc = NativeService::in_memory();
    let res = run_ok(
        &svc,
        "CALL drevo.extract.status() YIELD configured, model, upstream, lastError, lastSkipped \
         RETURN configured, model, upstream, lastError, lastSkipped",
    );
    assert_eq!(
        res.rows,
        vec![vec![
            Value::Bool(false),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null
        ]]
    );
}

#[test]
fn a_bad_config_is_refused_before_any_call() {
    drevo::extract::uninstall();
    let svc = NativeService::in_memory();
    assert_eq!(
        invalid_call(
            &svc,
            "CALL drevo.extract.entities('x', {chunkSize: 10}) YIELD name RETURN name"
        ),
        "unknown config key `chunkSize`; expected types, relations, schemaHint, language, \
         model, chunkChars, minConfidence, source, session, resolve, dryRun"
    );
    assert_eq!(
        invalid_call(
            &svc,
            "CALL drevo.extract.facts('x', {dryRun: true}) YIELD quote RETURN quote"
        ),
        "`dryRun` applies to drevo.extract.ingest only"
    );
    assert_eq!(
        invalid_call(
            &svc,
            "CALL drevo.extract.ingest('x', {}) YIELD facts RETURN facts"
        ),
        "drevo.extract.ingest needs a `source`: an id or a map {id, uri, title, kind}"
    );
    assert_eq!(
        invalid_call(
            &svc,
            "CALL drevo.extract.entities(42) YIELD name RETURN name"
        ),
        "the text must be a string, got Integer"
    );
}

#[test]
fn ingest_takes_two_arguments() {
    let svc = NativeService::in_memory();
    assert_eq!(
        invalid_call(
            &svc,
            "CALL drevo.extract.ingest('x') YIELD facts RETURN facts"
        ),
        "expected 2 arguments, got 1"
    );
}

#[test]
fn chunks_shows_the_passages_with_exact_character_offsets() {
    drevo::extract::uninstall();
    let svc = NativeService::in_memory();
    let text = "Дана открыла рукопись на рассвете. 🚀 Дана прочла её до полудня. ".repeat(4);
    let res = run_with(
        &svc,
        "CALL drevo.extract.chunks($text, {chunkChars: 80}) YIELD position, offset, length, text \
         RETURN position, offset, length, text, substring($text, offset, length) = text AS exact",
        &[("text", Value::String(text.clone()))],
    )
    .expect("chunks");
    assert!(res.rows.len() > 2, "{:?}", res.rows);
    for (i, row) in res.rows.iter().enumerate() {
        assert_eq!(row[0], Value::Integer(i as i64));
        assert_eq!(row[4], Value::Bool(true), "{row:?}");
        match &row[2] {
            Value::Integer(n) => assert!(*n <= 80),
            other => panic!("{other:?}"),
        }
    }
    let null_config = run_with(
        &svc,
        "CALL drevo.extract.chunks($text) YIELD position RETURN count(position)",
        &[("text", Value::String(text))],
    )
    .expect("chunks");
    assert_eq!(null_config.rows[0][0], Value::Integer(1));
}

// ----- with a fake chat upstream ------------------------------------------------

#[cfg(feature = "embeddings-proxy")]
mod upstream {
    use super::*;

    use std::collections::BTreeMap;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use drevo::bolt::packstream::Value as Bolt;
    use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
    use drevo::extract::settings::{install_config, ExtractConfig};
    use serde_json::json;

    /// How the fake model behaves.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        /// Answer with what the passage holds.
        Honest,
        /// Also report "Globex", which no passage holds.
        Hallucinate,
        /// Answer with prose, never JSON.
        Garbage,
        /// Prose the first time, then JSON.
        GarbageOnce,
        /// Fail with HTTP 500.
        Fail,
    }

    #[derive(Clone)]
    struct Fake {
        mode: Mode,
        /// Every request body received.
        seen: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    /// The cast the fake model recognises.
    const CAST: [(&str, &str); 5] = [
        ("Пушкину", "PERSON"),
        ("Dana", "PERSON"),
        ("Acme", "ORGANIZATION"),
        ("Oslo", "LOCATION"),
        ("Nostromo", "SPACESHIP"),
    ];

    /// The facts the fake model recognises, by quote.
    const FACTS: [(&str, &str, &str, &str); 2] = [
        ("Dana", "works at", "Acme", "Dana joined Acme"),
        ("Acme", "locatedIn", "Oslo", "Acme is based in Oslo"),
    ];

    fn type_of(name: &str) -> &'static str {
        CAST.iter()
            .find(|(n, _)| *n == name)
            .map_or("OBJECT", |(_, t)| t)
    }

    /// Character offsets of every occurrence of `needle` in `hay`.
    fn char_offsets(hay: &str, needle: &str) -> Vec<usize> {
        hay.match_indices(needle)
            .map(|(byte, _)| hay[..byte].chars().count())
            .collect()
    }

    fn answer(passage: &str, hallucinate: bool) -> serde_json::Value {
        let mut entities = Vec::new();
        for (name, kind) in CAST {
            for offset in char_offsets(passage, name) {
                entities.push(json!({
                    "name": name, "type": kind, "surface": name, "offset": offset,
                    "description": format!("{name} in the manuscript"), "confidence": 0.9,
                    "salience": 3
                }));
            }
        }
        if hallucinate {
            entities.push(json!({
                "name": "Globex", "type": "ORGANIZATION", "surface": "Globex", "offset": 0
            }));
        }
        let mut facts = Vec::new();
        for (subject, relation, object, quote) in FACTS {
            for offset in char_offsets(passage, quote) {
                facts.push(json!({
                    "subject": subject, "subjectType": type_of(subject),
                    "relation": relation, "object": object, "objectType": type_of(object),
                    // The model is one character off; the server repairs it.
                    "quote": quote, "offset": offset + 1, "confidence": 0.8
                }));
            }
        }
        json!({ "entities": entities, "facts": facts })
    }

    async fn chat(
        State(fake): State<Fake>,
        Json(body): Json<serde_json::Value>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let calls = {
            let mut seen = fake.seen.lock().unwrap();
            seen.push(body.clone());
            seen.len()
        };
        if fake.mode == Mode::Fail {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "model overloaded",
            )
                .into_response();
        }
        let user = body["messages"]
            .as_array()
            .and_then(|m| m.iter().rev().find(|m| m["role"] == "user"))
            .and_then(|m| m["content"].as_str())
            .unwrap_or_default()
            .to_string();
        let first_user = body["messages"]
            .as_array()
            .and_then(|m| m.iter().find(|m| m["role"] == "user"))
            .and_then(|m| m["content"].as_str())
            .unwrap_or_default()
            .to_string();
        let passage = first_user
            .split_once("Text:\n")
            .map(|(_, p)| p.to_string())
            .unwrap_or_default();
        let content = match fake.mode {
            Mode::Garbage => "I found Dana and Acme in the text.".to_string(),
            Mode::GarbageOnce if !user.contains("Return ONLY") => "Dana, Acme.".to_string(),
            _ => answer(&passage, fake.mode == Mode::Hallucinate).to_string(),
        };
        let _ = calls;
        Json(json!({
            "choices": [{"message": {"role": "assistant", "content": content}}]
        }))
        .into_response()
    }

    /// A fake chat upstream installed for the `drevo.extract.*` procedures.
    struct Upstream {
        seen: Arc<Mutex<Vec<serde_json::Value>>>,
        _runtime: tokio::runtime::Runtime,
    }

    impl Upstream {
        fn start(mode: Mode) -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let fake = Fake {
                mode,
                seen: Arc::new(Mutex::new(Vec::new())),
            };
            let seen = Arc::clone(&fake.seen);
            let addr: SocketAddr = runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let app = Router::new()
                    .route("/v1/chat/completions", post(chat))
                    .with_state(fake);
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                addr
            });
            install_config(&ExtractConfig {
                upstream: format!("http://{addr}/v1/chat/completions"),
                api_key: Some("sk-test".into()),
                model: "fake-model".into(),
                timeout_ms: 10_000,
            })
            .unwrap();
            Self {
                seen,
                _runtime: runtime,
            }
        }

        fn calls(&self) -> Vec<serde_json::Value> {
            self.seen.lock().unwrap().clone()
        }
    }

    /// Serialises the tests of this binary: the endpoint is process-wide.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    const BOOK: &str = "Dana joined Acme in May. Acme is based in Oslo, far from the Nostromo.";

    fn strings(res: &ExecResult, col: usize) -> Vec<String> {
        res.rows
            .iter()
            .map(|r| match &r[col] {
                Value::String(s) => s.clone(),
                other => panic!("expected a string, got {other:?}"),
            })
            .collect()
    }

    fn book() -> Value {
        Value::String(BOOK.into())
    }

    const ALL_TYPES: &str = "['PERSON', 'ORGANIZATION', 'LOCATION', 'spaceship']";

    #[test]
    fn entities_yield_verified_rows_and_drop_a_hallucinated_one() {
        let _s = serial();
        let llm = Upstream::start(Mode::Hallucinate);
        let svc = NativeService::in_memory();
        let res = run_with(
            &svc,
            &format!(
                "CALL drevo.extract.entities($text, {{types: {ALL_TYPES}}}) \
                 YIELD name, type, description, offset, length, surface, confidence \
                 RETURN name, type, description, offset, length, surface, confidence, \
                        substring($text, offset, length) = surface AS exact \
                 ORDER BY offset"
            ),
            &[("text", book())],
        )
        .expect("entities");
        assert_eq!(
            strings(&res, 0),
            ["Dana", "Acme", "Acme", "Oslo", "Nostromo"]
        );
        assert_eq!(
            strings(&res, 1),
            [
                "PERSON",
                "ORGANIZATION",
                "ORGANIZATION",
                "LOCATION",
                "SPACESHIP"
            ]
        );
        assert_eq!(
            res.rows[0][2],
            Value::String("Dana in the manuscript".into())
        );
        assert_eq!(res.rows[0][3], Value::Integer(0));
        assert_eq!(res.rows[0][4], Value::Integer(4));
        assert_eq!(res.rows[0][6], Value::Float(0.9));
        assert!(res.rows.iter().all(|r| r[7] == Value::Bool(true)));
        // Globex was dropped, and counted.
        let st = run_ok(
            &svc,
            "CALL drevo.extract.status() YIELD configured, model, upstream, lastError, lastSkipped \
             RETURN configured, model, lastError, lastSkipped",
        );
        assert_eq!(
            st.rows[0],
            vec![
                Value::Bool(true),
                Value::String("fake-model".into()),
                Value::Null,
                Value::Integer(1)
            ]
        );
        assert_eq!(llm.calls().len(), 1);
        // Read-only: nothing was written.
        assert_eq!(count(&svc, "MATCH (n) RETURN count(n)"), 0);
    }

    #[test]
    fn the_default_types_leave_out_a_custom_one() {
        let _s = serial();
        let _llm = Upstream::start(Mode::Honest);
        let svc = NativeService::in_memory();
        let res = run_with(
            &svc,
            "CALL drevo.extract.entities($text) YIELD name RETURN name ORDER BY name",
            &[("text", book())],
        )
        .expect("entities");
        assert_eq!(strings(&res, 0), ["Acme", "Acme", "Dana", "Oslo"]);
    }

    #[test]
    fn facts_yield_repaired_quotes_and_upper_snake_relations() {
        let _s = serial();
        let _llm = Upstream::start(Mode::Honest);
        let svc = NativeService::in_memory();
        let res = run_with(
            &svc,
            "CALL drevo.extract.facts($text, {minConfidence: 0.7}) \
             YIELD subject, subjectType, relation, object, objectType, quote, offset, length, confidence \
             RETURN subject, subjectType, relation, object, objectType, quote, offset, length, confidence \
             ORDER BY offset",
            &[("text", book())],
        )
        .expect("facts");
        assert_eq!(res.rows.len(), 2, "{:?}", res.rows);
        assert_eq!(
            res.rows[0],
            vec![
                Value::String("Dana".into()),
                Value::String("PERSON".into()),
                Value::String("WORKS_AT".into()),
                Value::String("Acme".into()),
                Value::String("ORGANIZATION".into()),
                Value::String("Dana joined Acme".into()),
                Value::Integer(0),
                Value::Integer(16),
                Value::Float(0.8),
            ]
        );
        assert_eq!(res.rows[1][2], Value::String("LOCATED_IN".into()));
        assert_eq!(res.rows[1][6], Value::Integer(25));
        // A higher bar drops both.
        let none = run_with(
            &svc,
            "CALL drevo.extract.facts($text, {minConfidence: 0.85}) YIELD quote RETURN quote",
            &[("text", book())],
        )
        .expect("facts");
        assert!(none.rows.is_empty());
    }

    #[test]
    fn the_config_reaches_the_upstream() {
        let _s = serial();
        let llm = Upstream::start(Mode::Honest);
        let svc = NativeService::in_memory();
        run_with(
            &svc,
            "CALL drevo.extract.facts($text, {model: 'llama3.1', relations: ['works at'], \
               schemaHint: 'A novel about publishing.', language: 'en'}) \
             YIELD relation RETURN relation",
            &[("text", book())],
        )
        .expect("facts");
        let calls = llm.calls();
        assert_eq!(calls[0]["model"], "llama3.1");
        let system = calls[0]["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("A novel about publishing."), "{system}");
        assert!(system.contains("WORKS_AT"), "{system}");
        assert!(system.contains("The text is in en"), "{system}");
    }

    #[test]
    fn ingest_writes_the_provenance_graph() {
        let _s = serial();
        let _llm = Upstream::start(Mode::Hallucinate);
        let svc = NativeService::in_memory();
        let res = run_with(
            &svc,
            &format!(
                "CALL drevo.extract.ingest($text, {{chunkChars: 40, types: {ALL_TYPES}, \
                   source: {{id: 'book-1', title: 'Book One', kind: 'document'}}}}) \
                 YIELD source, chunks, entities, facts, skipped \
                 RETURN source, chunks, entities, facts, skipped"
            ),
            &[("text", book())],
        )
        .expect("ingest");
        let row = &res.rows[0];
        assert_eq!(row[0], Value::String("book-1".into()));
        let chunks = match row[1] {
            Value::Integer(n) => n,
            ref other => panic!("{other:?}"),
        };
        assert!(chunks >= 2, "{row:?}");
        // One hallucinated Globex per passage was dropped.
        assert_eq!(row[4], Value::Integer(chunks));
        assert_eq!(
            count(&svc, "MATCH (c:Chunk)-[:FROM]->(:Source {id: 'book-1', title: 'Book One'}) RETURN count(c)"),
            chunks
        );
        assert_eq!(
            count(&svc, "MATCH (:Chunk)-[r:NEXT]->(:Chunk) RETURN count(r)"),
            chunks - 1
        );
        assert_eq!(
            strings(
                &run_ok(&svc, "MATCH (c:Chunk) RETURN c.id ORDER BY c.order"),
                0
            ),
            (0..chunks)
                .map(|i| format!("book-1#{i}"))
                .collect::<Vec<_>>()
        );
        // Entities, keyed on name and type, with their custom label.
        assert_eq!(
            strings(
                &run_ok(&svc, "MATCH (e:Entity) RETURN e.name ORDER BY e.name"),
                0
            ),
            ["Acme", "Dana", "Nostromo", "Oslo"]
        );
        assert_eq!(
            count(
                &svc,
                "MATCH (e:Entity:Spaceship {type: 'SPACESHIP'}) RETURN count(e)"
            ),
            1
        );
        assert_eq!(
            count(&svc, "MATCH (e:Entity {name: 'Globex'}) RETURN count(e)"),
            0
        );
        // Every mention points at the words in its chunk.
        let exact = run_ok(
            &svc,
            "MATCH (e:Entity)-[m:MENTIONED_IN]->(c:Chunk) \
             RETURN substring(c.text, m.offset, m.length) = m.surface AND m.surface = e.name",
        );
        assert!(!exact.rows.is_empty());
        assert!(exact.rows.iter().all(|r| r[0] == Value::Bool(true)));
        // The facts cite their chunk and quote.
        let facts = run_ok(
            &svc,
            "CALL drevo.memory.factsAt('Dana', null) YIELD relation, object, source, quote \
             RETURN relation, object.name, source.id, quote",
        );
        assert_eq!(
            facts.rows,
            vec![vec![
                Value::String("WORKS_AT".into()),
                Value::String("Acme".into()),
                Value::String("book-1".into()),
                Value::String("Dana joined Acme".into()),
            ]]
        );
        let cited = run_ok(
            &svc,
            "MATCH (:Entity {name: 'Dana'})-[f:RELATED_TO]->(:Entity {name: 'Acme'}), \
                   (c:Chunk {id: f.chunk_id}) \
             RETURN f.source_id, substring(c.text, f.offset, f.length) = f.quote",
        );
        assert_eq!(
            cited.rows,
            vec![vec![Value::String("book-1".into()), Value::Bool(true)]]
        );
        let evidence = run_ok(
            &svc,
            "CALL drevo.memory.evidence('Dana', 10) YIELD chunk, quote, fact \
             RETURN chunk.id, quote, fact IS NOT NULL",
        );
        assert!(
            evidence
                .rows
                .iter()
                .any(|r| r[1] == Value::String("Dana joined Acme".into())
                    && r[2] == Value::Bool(true)),
            "{:?}",
            evidence.rows
        );
    }

    #[test]
    fn ingest_counts_match_its_dry_run_which_writes_nothing() {
        let _s = serial();
        let _llm = Upstream::start(Mode::Hallucinate);
        let svc = NativeService::in_memory();
        let q = |dry: bool| {
            format!(
                "CALL drevo.extract.ingest($text, {{chunkChars: 40, dryRun: {dry}, \
                   source: {{id: 'book-1'}}}}) \
                 YIELD source, chunks, entities, facts, skipped \
                 RETURN source, chunks, entities, facts, skipped"
            )
        };
        let dry = run_with(&svc, &q(true), &[("text", book())]).expect("dry run");
        assert_eq!(count(&svc, "MATCH (n) RETURN count(n)"), 0);
        let wet = run_with(&svc, &q(false), &[("text", book())]).expect("ingest");
        assert_eq!(dry.rows, wet.rows);
        assert!(count(&svc, "MATCH (n) RETURN count(n)") > 0);
    }

    #[test]
    fn re_ingesting_a_source_changes_nothing() {
        let _s = serial();
        let _llm = Upstream::start(Mode::Honest);
        let svc = NativeService::in_memory();
        let q = "CALL drevo.extract.ingest($text, {chunkChars: 40, source: 'book-1'}) \
                 YIELD chunks RETURN chunks";
        run_ok(
            &svc,
            "CALL drevo.memory.addSource('book-1', null, 'Book One', 'document') YIELD node RETURN node",
        );
        run_with(&svc, q, &[("text", book())]).expect("first");
        let shape = |svc: &NativeService| {
            (
                count(svc, "MATCH (n) RETURN count(n)"),
                count(svc, "MATCH ()-[r]->() RETURN count(r)"),
            )
        };
        let before = shape(&svc);
        run_with(&svc, q, &[("text", book())]).expect("second");
        assert_eq!(shape(&svc), before);
    }

    #[test]
    fn ingest_into_an_unknown_source_id_is_refused() {
        let _s = serial();
        let _llm = Upstream::start(Mode::Honest);
        let svc = NativeService::in_memory();
        let message = match run_with(
            &svc,
            "CALL drevo.extract.ingest($text, {source: 'missing'}) YIELD facts RETURN facts",
            &[("text", book())],
        ) {
            Err(ExecError::InvalidProcedureCall { message, .. }) => message,
            other => panic!("{other:?}"),
        };
        assert!(
            message.starts_with("no source `missing` — add it first with drevo.memory.addSource"),
            "{message}"
        );
        assert_eq!(count(&svc, "MATCH (n) RETURN count(n)"), 0);
    }

    #[test]
    fn ingest_mentions_come_from_the_session_newest_message() {
        let _s = serial();
        let _llm = Upstream::start(Mode::Honest);
        let svc = NativeService::in_memory();
        run_ok(
            &svc,
            "CALL drevo.memory.addMessage('edit-1', 'user', 'Please read chapter one') \
             YIELD node RETURN node",
        );
        run_with(
            &svc,
            "CALL drevo.extract.ingest($text, {source: {id: 'ch-1'}, session: 'edit-1'}) \
             YIELD entities RETURN entities",
            &[("text", book())],
        )
        .expect("ingest");
        assert_eq!(
            strings(
                &run_ok(
                    &svc,
                    "MATCH (:Message)-[:MENTIONS]->(e:Entity) RETURN e.name ORDER BY e.name"
                ),
                0
            ),
            ["Acme", "Dana", "Oslo"]
        );
    }

    /// «Пушкин» is known; a letter mentions «Пушкину» (dative).
    fn known_poet_and_letter(svc: &NativeService, resolve: &str) -> ExecResult {
        run_ok(
            svc,
            "CALL drevo.memory.rememberEntity(null, 'Пушкин', 'PERSON', 'poet') YIELD node RETURN node",
        );
        run_with(
            svc,
            &format!(
                "CALL drevo.extract.ingest($text, {{source: {{id: 'letter-1'}}, resolve: '{resolve}'}}) \
                 YIELD entities RETURN entities"
            ),
            &[("text", Value::String("Пушкину писали письма.".into()))],
        )
        .expect("ingest")
    }

    #[test]
    fn ingest_with_fuzzy_resolve_reuses_a_near_duplicate_entity() {
        let _s = serial();
        let _llm = Upstream::start(Mode::Honest);
        let svc = NativeService::in_memory();
        let res = known_poet_and_letter(&svc, "fuzzy");
        assert_eq!(res.rows[0][0], Value::Integer(1));
        assert_eq!(
            strings(&run_ok(&svc, "MATCH (e:Entity) RETURN e.name"), 0),
            ["Пушкин"]
        );
        assert_eq!(
            strings(
                &run_ok(
                    &svc,
                    "MATCH (e:Entity)-[m:MENTIONED_IN]->(:Chunk) RETURN e.name + '/' + m.surface"
                ),
                0
            ),
            ["Пушкин/Пушкину"]
        );
    }

    #[test]
    fn ingest_with_exact_resolve_keeps_a_near_duplicate_apart() {
        let _s = serial();
        let _llm = Upstream::start(Mode::Honest);
        let svc = NativeService::in_memory();
        known_poet_and_letter(&svc, "exact");
        assert_eq!(
            strings(
                &run_ok(&svc, "MATCH (e:Entity) RETURN e.name ORDER BY e.name"),
                0
            ),
            ["Пушкин", "Пушкину"]
        );
    }

    #[test]
    fn a_malformed_answer_is_retried_once() {
        let _s = serial();
        let llm = Upstream::start(Mode::GarbageOnce);
        let svc = NativeService::in_memory();
        let res = run_with(
            &svc,
            "CALL drevo.extract.entities($text) YIELD name RETURN count(name)",
            &[("text", book())],
        )
        .expect("entities");
        assert_eq!(res.rows[0][0], Value::Integer(4));
        let calls = llm.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1]["messages"][2]["role"], "assistant");
        assert!(calls[1]["messages"][3]["content"]
            .as_str()
            .unwrap()
            .contains("Return ONLY the JSON object"));
    }

    #[test]
    fn a_second_malformed_answer_fails_and_writes_nothing() {
        let _s = serial();
        let llm = Upstream::start(Mode::Garbage);
        let svc = NativeService::in_memory();
        run_ok(&svc, "CREATE (:Note {title: 'keep me'})");
        let message = match run_with(
            &svc,
            "CALL drevo.extract.ingest($text, {source: {id: 'book-1'}}) YIELD facts RETURN facts",
            &[("text", book())],
        ) {
            Err(ExecError::InvalidProcedureCall { name, message, .. }) => {
                assert_eq!(name, "drevo.extract.ingest");
                message
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(
            message,
            "the model did not return the extraction JSON after a retry: \
             no JSON object in the answer ('I found Dana and Acme in the text.')"
        );
        assert_eq!(llm.calls().len(), 2);
        assert_eq!(count(&svc, "MATCH (n) RETURN count(n)"), 1);
        let st = run_ok(
            &svc,
            "CALL drevo.extract.status() YIELD lastError RETURN lastError",
        );
        assert_eq!(st.rows[0][0], Value::String(message));
    }

    #[test]
    fn an_upstream_failure_is_reported() {
        let _s = serial();
        let _llm = Upstream::start(Mode::Fail);
        let svc = NativeService::in_memory();
        let message = match run_with(
            &svc,
            "CALL drevo.extract.facts($text) YIELD quote RETURN quote",
            &[("text", book())],
        ) {
            Err(ExecError::InvalidProcedureCall { message, .. }) => message,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            message,
            "extraction upstream error: upstream returned 500 Internal Server Error: model overloaded"
        );
    }

    #[test]
    fn a_failed_write_leaves_no_part_of_the_ingest() {
        let _s = serial();
        let _llm = Upstream::start(Mode::Honest);
        let svc = NativeService::in_memory();
        // Acme exists as a place, so the organisation Acme cannot be created.
        run_ok(
            &svc,
            "CREATE CONSTRAINT entity_name FOR (e:Entity) REQUIRE e.name IS UNIQUE",
        );
        run_ok(
            &svc,
            "CALL drevo.memory.rememberEntity(null, 'Acme', 'LOCATION', null) YIELD node RETURN node",
        );
        let before = count(&svc, "MATCH (n) RETURN count(n)");
        let failed = run_with(
            &svc,
            "CALL drevo.extract.ingest($text, {source: {id: 'book-1'}}) YIELD facts RETURN facts",
            &[("text", book())],
        );
        assert!(failed.is_err(), "{failed:?}");
        assert_eq!(count(&svc, "MATCH (n) RETURN count(n)"), before);
        assert_eq!(count(&svc, "MATCH (s:Source) RETURN count(s)"), 0);
    }

    #[test]
    fn the_trigger_recipe_extracts_new_documents_after_commit() {
        let _s = serial();
        let _llm = Upstream::start(Mode::Honest);
        let svc = Arc::new(NativeService::in_memory());
        svc.start_trigger_worker();
        run_ok(
            &svc,
            "CALL apoc.trigger.install('drevo', 'extract_documents', \
               'UNWIND $createdNodes AS d WITH d WHERE d:Document \
                CALL drevo.extract.ingest(d.body, {source: {id: d.id, title: d.title, kind: \"document\"}}) \
                YIELD facts RETURN count(*)', \
               {phase: 'afterAsync'})",
        );
        run_with(
            &svc,
            "CREATE (:Document {id: 'doc-1', title: 'Chapter One', body: $text})",
            &[("text", book())],
        )
        .expect("create");
        let deadline = Instant::now() + Duration::from_secs(20);
        while count(&svc, "MATCH (e:Entity) RETURN count(e)") < 3 {
            assert!(
                Instant::now() < deadline,
                "the afterAsync trigger never extracted the document"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            count(
                &svc,
                "MATCH (:Chunk)-[:FROM]->(s:Source {id: 'doc-1', title: 'Chapter One', kind: 'document'}) \
                 RETURN count(s)"
            ),
            1
        );
        assert_eq!(
            count(
                &svc,
                "MATCH (:Entity {name: 'Dana'})-[f:RELATED_TO {source_id: 'doc-1'}]->(:Entity {name: 'Acme'}) \
                 RETURN count(f)"
            ),
            1
        );
    }

    #[test]
    fn bolt_carries_typed_rows() {
        let _s = serial();
        let _llm = Upstream::start(Mode::Honest);
        let svc = Arc::new(NativeService::in_memory());
        let mut s = Session::new_durable(Arc::clone(&svc));
        s.handle(ClientMessage::Hello {
            extra: BTreeMap::new(),
        });
        let mut records = Vec::new();
        for query in [
            "CALL drevo.extract.entities($text) YIELD name, offset, length, confidence \
             RETURN name, offset, length, confidence ORDER BY offset LIMIT 1",
            "CALL drevo.extract.ingest($text, {source: {id: 'book-1'}}) \
             YIELD source, chunks, entities, facts, skipped \
             RETURN source, chunks, entities, facts, skipped",
        ] {
            let mut replies = s.handle(ClientMessage::Run {
                query: query.to_string(),
                parameters: BTreeMap::from([("text".to_string(), Bolt::String(BOOK.into()))]),
                extra: BTreeMap::new(),
            });
            replies.extend(s.handle(ClientMessage::Pull {
                extra: BTreeMap::new(),
            }));
            let fields: Vec<Vec<Bolt>> = replies
                .into_iter()
                .filter_map(|m| match m {
                    ServerMessage::Record { fields } => Some(fields),
                    _ => None,
                })
                .collect();
            assert_eq!(fields.len(), 1, "{query}");
            records.push(fields.into_iter().next().unwrap());
        }
        assert_eq!(
            records[0],
            vec![
                Bolt::String("Dana".into()),
                Bolt::Integer(0),
                Bolt::Integer(4),
                Bolt::Float(0.9)
            ]
        );
        assert_eq!(
            records[1],
            vec![
                Bolt::String("book-1".into()),
                Bolt::Integer(1),
                Bolt::Integer(4),
                Bolt::Integer(2),
                // Nostromo is a SPACESHIP, not one of the default types.
                Bolt::Integer(1)
            ]
        );
    }
}
