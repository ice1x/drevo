//! Durable ingest jobs: `drevo.ingest.*` stores a job of passages in the
//! graph and a scheduler per database extracts them one by one in the
//! background, retrying a failed model call with a growing delay, and
//! picking up where it left off after a restart.
//!
//! - `CALL drevo.ingest.submit(items, config) YIELD jobId, total` stores an
//!   `:IngestJob` with one `:IngestItem` per passage;
//! - `status`, `list` and `errors` read jobs; `cancel`, `pause`, `resume`,
//!   `retryFailed` and `purge` change them;
//! - each item is a `drevo.extract.ingest` of its passage in one
//!   transaction, and ends `-[:PRODUCED]->` its `:Chunk`.
//!
//! The procedures that only read and store jobs need no model and are
//! tested first. The scheduler tests use a local HTTP server answering like
//! an OpenAI-compatible `chat/completions` upstream that can be scripted to
//! fail with a status, hang until released, or answer slowly, and that
//! counts how many requests it has in flight at once. The scenario is a
//! book editor loading manuscripts.

use std::collections::BTreeMap;

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
        .collect();
    svc.execute(&parse(q).expect("parse"), params)
}

fn ok(svc: &NativeService, q: &str, params: &[(&str, Value)]) -> ExecResult {
    run_with(svc, q, params).unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Integer(n) => *n,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    }
}

fn count(svc: &NativeService, q: &str, params: &[(&str, Value)]) -> i64 {
    int(&ok(svc, q, params).rows[0][0])
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

fn map(pairs: &[(&str, Value)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

/// The `InvalidProcedureCall` message `q` fails with.
fn invalid_call(svc: &NativeService, q: &str, params: &[(&str, Value)]) -> String {
    match run_with(svc, q, params) {
        Err(ExecError::InvalidProcedureCall { message, .. }) => message,
        other => panic!("`{q}`: expected InvalidProcedureCall, got {other:?}"),
    }
}

/// Submit `items` with `config`; the job id and its item count.
fn submit(svc: &NativeService, items: Value, config: Value) -> (String, i64) {
    let res = ok(
        svc,
        "CALL drevo.ingest.submit($items, $config) YIELD jobId, total RETURN jobId, total",
        &[("items", items), ("config", config)],
    );
    (string(&res.rows[0][0]), int(&res.rows[0][1]))
}

/// A job's state as `drevo.ingest.status` reports it.
#[derive(Debug, Clone, PartialEq)]
struct Status {
    state: String,
    total: i64,
    done: i64,
    failed: i64,
    /// Items by state.
    items: BTreeMap<String, i64>,
}

fn status(svc: &NativeService, job: &str) -> Status {
    let res = ok(
        svc,
        "CALL drevo.ingest.status($job) YIELD job, items \
         RETURN job.state, job.total, job.done, job.failed, items",
        &[("job", s(job))],
    );
    let row = &res.rows[0];
    let items = match &row[4] {
        Value::Map(m) => m.iter().map(|(k, v)| (k.clone(), int(v))).collect(),
        other => panic!("expected a map of counts, got {other:?}"),
    };
    Status {
        state: string(&row[0]),
        total: int(&row[1]),
        done: int(&row[2]),
        failed: int(&row[3]),
        items,
    }
}

fn counts(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
    let mut all: BTreeMap<String, i64> = ["queued", "running", "done", "failed", "dead"]
        .iter()
        .map(|k| ((*k).to_string(), 0))
        .collect();
    for (k, v) in pairs {
        all.insert((*k).to_string(), *v);
    }
    all
}

const MANUSCRIPT: &str = "Dana joined Acme in May. Acme is based in Oslo. \
     Dana wrote the first chapter there. Oslo was cold that year.";

// ----- storing and reading jobs, without a model ----------------------------

#[test]
fn submit_stores_the_job_and_its_passages_and_returns_at_once() {
    let svc = NativeService::in_memory();
    let (job, total) = submit(
        &svc,
        map(&[("text", s(MANUSCRIPT))]),
        map(&[
            (
                "source",
                map(&[("id", s("book-1")), ("title", s("Book One"))]),
            ),
            ("chunkChars", Value::Integer(40)),
            ("priority", Value::Integer(3)),
        ]),
    );
    let expected = drevo::extract::chunk::split(MANUSCRIPT, 40);
    assert_eq!(total, expected.len() as i64);
    let st = status(&svc, &job);
    assert_eq!(st.state, "queued");
    assert_eq!((st.total, st.done, st.failed), (total, 0, 0));
    assert_eq!(st.items, counts(&[("queued", total)]));
    let res = ok(
        &svc,
        "MATCH (j:IngestJob {id: $job})-[:HAS_ITEM]->(i:IngestItem) \
         RETURN i.seq, i.offset, i.length, i.text, i.attempts, i.state, j.pipeline, \
                j.source_id, j.priority, j.config ORDER BY i.seq",
        &[("job", s(&job))],
    );
    assert_eq!(res.rows.len(), expected.len());
    for (row, chunk) in res.rows.iter().zip(&expected) {
        assert_eq!(int(&row[0]), chunk.order as i64);
        assert_eq!(int(&row[1]), chunk.offset as i64);
        assert_eq!(int(&row[2]), chunk.length as i64);
        assert_eq!(string(&row[3]), chunk.text);
        assert_eq!(int(&row[4]), 0);
        assert_eq!(string(&row[5]), "queued");
        assert_eq!(string(&row[6]), "extract");
        assert_eq!(string(&row[7]), "book-1");
        assert_eq!(int(&row[8]), 3);
        let config: serde_json::Value = serde_json::from_str(&string(&row[9])).unwrap();
        assert_eq!(config["source"]["id"], "book-1");
        assert_eq!(config["chunkChars"], 40);
    }
    // Nothing is extracted yet: no source, no chunk.
    assert_eq!(count(&svc, "MATCH (c:Chunk) RETURN count(c)", &[]), 0);
}

#[test]
fn list_shows_jobs_newest_first_and_filters_by_state() {
    let svc = NativeService::in_memory();
    let (first, _) = submit(
        &svc,
        Value::List(vec![s("one")]),
        map(&[("source", map(&[("id", s("a"))]))]),
    );
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (second, _) = submit(
        &svc,
        Value::List(vec![s("two")]),
        map(&[("source", map(&[("id", s("b"))]))]),
    );
    ok(
        &svc,
        "CALL drevo.ingest.cancel($job) YIELD job RETURN job",
        &[("job", s(&first))],
    );
    let ids = |q: &str| -> Vec<String> {
        ok(&svc, q, &[])
            .rows
            .iter()
            .map(|r| string(&r[0]))
            .collect()
    };
    assert_eq!(
        ids("CALL drevo.ingest.list(null, 10) YIELD job RETURN job.id"),
        [second.clone(), first.clone()]
    );
    assert_eq!(
        ids("CALL drevo.ingest.list(null, 1) YIELD job RETURN job.id"),
        std::slice::from_ref(&second)
    );
    assert_eq!(
        ids("CALL drevo.ingest.list('cancelled', 10) YIELD job RETURN job.id"),
        [first]
    );
    assert_eq!(
        ids("CALL drevo.ingest.list('queued', 10) YIELD job RETURN job.id"),
        [second]
    );
    assert_eq!(
        invalid_call(&svc, "CALL drevo.ingest.list('sleeping', 10)", &[]),
        "state must be one of queued, running, paused, done, failed, cancelled, or null, \
         got 'sleeping'"
    );
    assert_eq!(
        invalid_call(&svc, "CALL drevo.ingest.list(null, -1)", &[]),
        "limit must be a non-negative integer, got -1"
    );
}

#[test]
fn pause_resume_and_cancel_move_a_queued_job_and_refuse_what_does_not_apply() {
    let svc = NativeService::in_memory();
    let (job, _) = submit(
        &svc,
        Value::List(vec![s("one"), s("two")]),
        map(&[("source", map(&[("id", s("a"))]))]),
    );
    let state = |q: &str| -> String { string(&ok(&svc, q, &[("job", s(&job))]).rows[0][0]) };
    assert_eq!(
        state("CALL drevo.ingest.pause($job) YIELD job RETURN job.state"),
        "paused"
    );
    assert_eq!(
        invalid_call(&svc, "CALL drevo.ingest.pause($job)", &[("job", s(&job))]),
        "the job is already paused"
    );
    // Never started: back to queued.
    assert_eq!(
        state("CALL drevo.ingest.resume($job) YIELD job RETURN job.state"),
        "queued"
    );
    assert_eq!(
        invalid_call(&svc, "CALL drevo.ingest.resume($job)", &[("job", s(&job))]),
        "cannot resume a job that is queued: it is not paused"
    );
    assert_eq!(
        state("CALL drevo.ingest.cancel($job) YIELD job RETURN job.state"),
        "cancelled"
    );
    assert_eq!(
        invalid_call(&svc, "CALL drevo.ingest.cancel($job)", &[("job", s(&job))]),
        "cannot cancel a job that is cancelled"
    );
    assert_eq!(
        invalid_call(
            &svc,
            "CALL drevo.ingest.retryFailed($job)",
            &[("job", s(&job))]
        ),
        "cannot retry a job that is cancelled"
    );
    // The items stay as they were.
    assert_eq!(status(&svc, &job).items, counts(&[("queued", 2)]));
    let finished = ok(
        &svc,
        "MATCH (j:IngestJob {id: $job}) RETURN j.finished_at IS NOT NULL",
        &[("job", s(&job))],
    );
    assert_eq!(finished.rows[0][0], Value::Bool(true));
}

#[test]
fn purge_drops_a_finished_job_s_items_and_keeps_its_counters() {
    let svc = NativeService::in_memory();
    let (job, _) = submit(
        &svc,
        Value::List(vec![s("one"), s("two"), s("three")]),
        map(&[("source", map(&[("id", s("a"))]))]),
    );
    assert_eq!(
        invalid_call(&svc, "CALL drevo.ingest.purge($job)", &[("job", s(&job))]),
        format!("job `{job}` is queued: cancel it or let it finish before purging it")
    );
    ok(&svc, "CALL drevo.ingest.cancel($job)", &[("job", s(&job))]);
    let res = ok(
        &svc,
        "CALL drevo.ingest.purge($job) YIELD deleted RETURN deleted",
        &[("job", s(&job))],
    );
    assert_eq!(int(&res.rows[0][0]), 3);
    assert_eq!(count(&svc, "MATCH (i:IngestItem) RETURN count(i)", &[]), 0);
    let st = status(&svc, &job);
    assert_eq!((st.state.as_str(), st.total), ("cancelled", 3));
    assert_eq!(st.items, counts(&[]));
    // Purging again deletes nothing.
    let res = ok(
        &svc,
        "CALL drevo.ingest.purge($job) YIELD deleted RETURN deleted",
        &[("job", s(&job))],
    );
    assert_eq!(int(&res.rows[0][0]), 0);
}

#[test]
fn errors_is_empty_for_a_job_without_failures() {
    let svc = NativeService::in_memory();
    let (job, _) = submit(
        &svc,
        Value::List(vec![s("one")]),
        map(&[("source", map(&[("id", s("a"))]))]),
    );
    let res = ok(
        &svc,
        "CALL drevo.ingest.errors($job, 10) YIELD seq, attempts, last_error \
         RETURN seq, attempts, last_error",
        &[("job", s(&job))],
    );
    assert!(res.rows.is_empty());
}

#[test]
fn an_unknown_job_is_named() {
    let svc = NativeService::in_memory();
    for q in [
        "CALL drevo.ingest.status('nope')",
        "CALL drevo.ingest.cancel('nope')",
        "CALL drevo.ingest.pause('nope')",
        "CALL drevo.ingest.resume('nope')",
        "CALL drevo.ingest.retryFailed('nope')",
        "CALL drevo.ingest.errors('nope', 5)",
        "CALL drevo.ingest.purge('nope')",
    ] {
        assert_eq!(invalid_call(&svc, q, &[]), "no ingest job `nope`", "{q}");
    }
    assert_eq!(
        invalid_call(&svc, "CALL drevo.ingest.status(3)", &[]),
        "the job id must be a string, got Integer"
    );
    // A job node written by hand with a state no job has.
    ok(
        &svc,
        "CREATE (:IngestJob {id: 'odd', state: 'sleeping'})",
        &[],
    );
    assert_eq!(
        invalid_call(&svc, "CALL drevo.ingest.pause('odd')", &[]),
        "job `odd` has an unknown state `sleeping`"
    );
    assert_eq!(
        invalid_call(&svc, "CALL drevo.ingest.errors('odd', -2)", &[]),
        "limit must be a non-negative integer, got -2"
    );
}

#[test]
fn bad_submissions_are_refused_and_store_nothing() {
    let svc = NativeService::in_memory();
    let cases: Vec<(Value, Value, &str)> = vec![
        (
            Value::List(vec![s("x")]),
            map(&[]),
            "drevo.ingest.submit needs a config with a `source`: an id or a map \
             {id, uri, title, kind}",
        ),
        (
            Value::List(vec![s("x")]),
            map(&[("source", s("b")), ("dryRun", Value::Bool(true))]),
            "`dryRun` does not apply to ingest jobs",
        ),
        (
            Value::List(vec![]),
            map(&[("source", map(&[("id", s("b"))]))]),
            "there is no text to ingest",
        ),
        (
            Value::List(vec![s("x"), Value::Integer(3)]),
            map(&[("source", map(&[("id", s("b"))]))]),
            "items[1] must be a string or a map {text, offset, meta}, got Integer",
        ),
        (
            Value::List(vec![s("x")]),
            map(&[
                ("source", s("missing-source")),
                ("maxAttempts", Value::Integer(0)),
            ]),
            "maxAttempts must be an integer from 1 to 100, got 0",
        ),
    ];
    for (items, config, want) in cases {
        assert_eq!(
            invalid_call(
                &svc,
                "CALL drevo.ingest.submit($items, $config)",
                &[("items", items), ("config", config)]
            ),
            want
        );
    }
    // Meta is data, not graph values.
    ok(&svc, "CREATE (:Draft {title: 'x'})", &[]);
    assert_eq!(
        invalid_call(
            &svc,
            "MATCH (d:Draft) CALL drevo.ingest.submit([{text: 'a', meta: {draft: d}}], \
             {source: {id: 'b'}}) YIELD jobId RETURN jobId",
            &[]
        ),
        "items[0].meta must hold plain values, not nodes or relationships"
    );
    // A source given by id must exist.
    assert_eq!(
        invalid_call(
            &svc,
            "CALL drevo.ingest.submit(['x'], {source: 'missing-source'})",
            &[]
        ),
        "no source `missing-source` — add it first with drevo.memory.addSource, or pass \
         {id, uri, title, kind}"
    );
    assert_eq!(
        count(
            &svc,
            "MATCH (n) WHERE n:IngestJob OR n:IngestItem RETURN count(n)",
            &[]
        ),
        0
    );
}

#[test]
fn every_procedure_checks_its_argument_count() {
    let svc = NativeService::in_memory();
    for (q, want) in [
        (
            "CALL drevo.ingest.submit(['x'])",
            "expected 2 arguments, got 1",
        ),
        ("CALL drevo.ingest.status()", "expected 1 arguments, got 0"),
        (
            "CALL drevo.ingest.list(null)",
            "expected 2 arguments, got 1",
        ),
        (
            "CALL drevo.ingest.errors('j')",
            "expected 2 arguments, got 1",
        ),
    ] {
        assert_eq!(invalid_call(&svc, q, &[]), want, "{q}");
    }
}

#[test]
fn a_store_without_jobs_lists_none() {
    let svc = NativeService::in_memory();
    let res = ok(
        &svc,
        "CALL drevo.ingest.list(null, 10) YIELD job RETURN job",
        &[],
    );
    assert!(res.rows.is_empty());
}

#[cfg(feature = "http")]
#[test]
fn ingest_settings_come_from_the_environment() {
    let config_from = |entries: &[(&'static str, &str)]| {
        let env: std::collections::HashMap<&'static str, String> = entries
            .iter()
            .map(|(k, v)| (*k, (*v).to_string()))
            .collect();
        drevo::server::Config::from_env(move |k| env.get(k).cloned()).map_err(|e| e.to_string())
    };
    let defaults = config_from(&[]).unwrap().ingest;
    assert_eq!(defaults, drevo::ingest::IngestSettings::default());
    assert_eq!(defaults.workers, 2);
    assert_eq!(defaults.max_attempts, 5);
    assert_eq!(defaults.retry_base, std::time::Duration::from_millis(2000));
    let set = config_from(&[
        ("DREVO_INGEST_WORKERS", " 4 "),
        ("DREVO_INGEST_MAX_ATTEMPTS", "7"),
        ("DREVO_INGEST_RETRY_BASE_MS", "250"),
    ])
    .unwrap()
    .ingest;
    assert_eq!(
        (set.workers, set.max_attempts, set.retry_base),
        (4, 7, std::time::Duration::from_millis(250))
    );
    for (key, bad, want) in [
        (
            "DREVO_INGEST_WORKERS",
            "0",
            "invalid DREVO_INGEST_WORKERS value `0`: expected a whole number of items in \
             flight, at least 1",
        ),
        (
            "DREVO_INGEST_MAX_ATTEMPTS",
            "many",
            "invalid DREVO_INGEST_MAX_ATTEMPTS value `many`: expected a whole number of \
             attempts from 1 to 100",
        ),
        (
            "DREVO_INGEST_MAX_ATTEMPTS",
            "101",
            "invalid DREVO_INGEST_MAX_ATTEMPTS value `101`: expected a whole number of \
             attempts from 1 to 100",
        ),
        (
            "DREVO_INGEST_RETRY_BASE_MS",
            "-5",
            "invalid DREVO_INGEST_RETRY_BASE_MS value `-5`: expected a whole number of \
             milliseconds, at least 1",
        ),
    ] {
        assert_eq!(config_from(&[(key, bad)]).unwrap_err(), want, "{key}={bad}");
    }
}

// ----- the scheduler, with a scripted chat upstream ----------------------------

#[cfg(feature = "embeddings-proxy")]
mod upstream {
    use super::*;

    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use drevo::extract::settings::{install_config, ExtractConfig};
    use serde_json::json;

    /// What the fake does with one request.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Step {
        /// Answer with the mentions the passage holds.
        Answer,
        /// Answer after a while.
        Slow(u64),
        /// Fail with this HTTP status.
        Status(u16),
        /// Wait until released, then answer.
        Hang,
    }

    #[derive(Default)]
    struct Script {
        /// The steps of the next requests, in order.
        steps: VecDeque<Step>,
        /// The step once `steps` is used up.
        then: Option<Step>,
        /// The passage of every request, in arrival order.
        passages: Vec<String>,
    }

    #[derive(Clone)]
    struct Fake {
        script: Arc<Mutex<Script>>,
        in_flight: Arc<AtomicUsize>,
        most_in_flight: Arc<AtomicUsize>,
        released: Arc<AtomicBool>,
    }

    /// The cast the fake model recognises.
    const CAST: [(&str, &str); 3] = [
        ("Dana", "PERSON"),
        ("Acme", "ORGANIZATION"),
        ("Oslo", "LOCATION"),
    ];

    fn answer(passage: &str) -> serde_json::Value {
        let mut entities = Vec::new();
        for (name, kind) in CAST {
            for (byte, _) in passage.match_indices(name) {
                let offset = passage[..byte].chars().count();
                entities.push(json!({
                    "name": name, "type": kind, "surface": name, "offset": offset,
                    "confidence": 0.9
                }));
            }
        }
        json!({ "entities": entities, "facts": [] })
    }

    struct Flight(Arc<AtomicUsize>);
    impl Drop for Flight {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    async fn chat(
        State(fake): State<Fake>,
        Json(body): Json<serde_json::Value>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let now = fake.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        let _flight = Flight(Arc::clone(&fake.in_flight));
        fake.most_in_flight.fetch_max(now, Ordering::SeqCst);
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
        let step = {
            let mut script = fake.script.lock().unwrap();
            script.passages.push(passage.clone());
            script
                .steps
                .pop_front()
                .unwrap_or(script.then.unwrap_or(Step::Answer))
        };
        match step {
            Step::Status(code) => {
                return (
                    axum::http::StatusCode::from_u16(code).unwrap(),
                    "scripted failure",
                )
                    .into_response()
            }
            Step::Slow(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
            Step::Hang => {
                while !fake.released.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            Step::Answer => {}
        }
        Json(json!({
            "choices": [{"message": {"role": "assistant", "content": answer(&passage).to_string()}}]
        }))
        .into_response()
    }

    /// A scripted chat upstream installed for extraction.
    struct Upstream {
        fake: Fake,
        _runtime: tokio::runtime::Runtime,
    }

    impl Upstream {
        fn start(steps: &[Step], then: Step) -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let fake = Fake {
                script: Arc::new(Mutex::new(Script {
                    steps: steps.iter().copied().collect(),
                    then: Some(then),
                    passages: Vec::new(),
                })),
                in_flight: Arc::new(AtomicUsize::new(0)),
                most_in_flight: Arc::new(AtomicUsize::new(0)),
                released: Arc::new(AtomicBool::new(false)),
            };
            let app_fake = fake.clone();
            let addr: SocketAddr = runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let app = Router::new()
                    .route("/v1/chat/completions", post(chat))
                    .with_state(app_fake);
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                addr
            });
            install_config(&ExtractConfig {
                upstream: format!("http://{addr}/v1/chat/completions"),
                api_key: None,
                model: "fake-model".into(),
                timeout_ms: 20_000,
            })
            .unwrap();
            Self {
                fake,
                _runtime: runtime,
            }
        }

        fn calls(&self) -> usize {
            self.fake.script.lock().unwrap().passages.len()
        }

        fn passages(&self) -> Vec<String> {
            self.fake.script.lock().unwrap().passages.clone()
        }

        fn in_flight(&self) -> usize {
            self.fake.in_flight.load(Ordering::SeqCst)
        }

        fn most_in_flight(&self) -> usize {
            self.fake.most_in_flight.load(Ordering::SeqCst)
        }

        fn release(&self) {
            self.fake.released.store(true, Ordering::SeqCst);
        }

        fn hold(&self) {
            self.fake.released.store(false, Ordering::SeqCst);
        }
    }

    impl Drop for Upstream {
        fn drop(&mut self) {
            // Let nothing hang past the test.
            self.release();
        }
    }

    /// Serialises the tests of this module: the upstream and the ingest
    /// settings are process-wide.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        drevo::ingest::configure(drevo::ingest::IngestSettings {
            workers: 2,
            max_attempts: 5,
            retry_base: Duration::from_millis(20),
            jitter_seed: Some(7),
        });
        guard
    }

    fn workers(n: usize) {
        drevo::ingest::configure(drevo::ingest::IngestSettings {
            workers: n,
            ..drevo::ingest::settings()
        });
    }

    fn started() -> Arc<NativeService> {
        let svc = Arc::new(NativeService::in_memory());
        svc.start_ingest();
        svc
    }

    /// Poll until `until` holds for the job's status; panic with the last
    /// status after `limit`.
    fn wait_for(
        svc: &NativeService,
        job: &str,
        limit: Duration,
        until: impl Fn(&Status) -> bool,
    ) -> Status {
        let deadline = Instant::now() + limit;
        loop {
            let st = status(svc, job);
            if until(&st) {
                return st;
            }
            assert!(
                Instant::now() < deadline,
                "gave up waiting; last status {st:?}"
            );
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn wait_until(limit: Duration, what: &str, until: impl Fn() -> bool) {
        let deadline = Instant::now() + limit;
        while !until() {
            assert!(Instant::now() < deadline, "gave up waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    const LONG: Duration = Duration::from_secs(20);

    fn item(svc: &NativeService, job: &str, seq: i64) -> (String, i64, Value) {
        let res = ok(
            svc,
            "MATCH (i:IngestItem {job_id: $job, seq: $seq}) RETURN i.state, i.attempts, i.last_error",
            &[("job", s(job)), ("seq", Value::Integer(seq))],
        );
        let row = &res.rows[0];
        (string(&row[0]), int(&row[1]), row[2].clone())
    }

    /// A text the chunker cuts into exactly three passages of at most 60
    /// characters.
    fn three_passages() -> String {
        let text = "Dana joined Acme in the spring of that year. \
                    Acme moved the whole team to Oslo in June. \
                    Dana finished the book in Oslo by winter.";
        assert_eq!(drevo::extract::chunk::split(text, 60).len(), 3);
        text.to_string()
    }

    #[test]
    fn a_three_passage_document_is_extracted_with_provenance() {
        let _s = serial();
        let llm = Upstream::start(&[], Step::Answer);
        let svc = started();
        let text = three_passages();
        let (job, total) = submit(
            &svc,
            map(&[("text", s(&text))]),
            map(&[
                (
                    "source",
                    map(&[("id", s("book-1")), ("title", s("Book One"))]),
                ),
                ("chunkChars", Value::Integer(60)),
            ]),
        );
        assert_eq!(total, 3);
        let st = wait_for(&svc, &job, LONG, |st| st.state == "done");
        assert_eq!((st.total, st.done, st.failed), (3, 3, 0));
        assert_eq!(st.items, counts(&[("done", 3)]));
        assert_eq!(llm.calls(), 3);
        // Each item points at its chunk, which is the passage at its offset
        // in the book.
        let res = ok(
            &svc,
            "MATCH (i:IngestItem {job_id: $job})-[:PRODUCED]->(c:Chunk)-[:FROM]->(src:Source) \
             RETURN i.seq, c.id, c.offset, c.length, c.text, src.id, src.title, i.attempts \
             ORDER BY i.seq",
            &[("job", s(&job))],
        );
        assert_eq!(res.rows.len(), 3);
        for (seq, row) in res.rows.iter().enumerate() {
            assert_eq!(int(&row[0]), seq as i64);
            assert_eq!(string(&row[1]), format!("book-1#{seq}"));
            let offset = int(&row[2]) as usize;
            let length = int(&row[3]) as usize;
            let at: String = text.chars().skip(offset).take(length).collect();
            assert_eq!(string(&row[4]), at);
            assert_eq!(string(&row[5]), "book-1");
            assert_eq!(string(&row[6]), "Book One");
            assert_eq!(int(&row[7]), 1);
        }
        // The chunks are chained in reading order and the entities point at
        // the passages that mention them.
        assert_eq!(
            count(
                &svc,
                "MATCH (:Chunk {id: 'book-1#0'})-[:NEXT]->(:Chunk {id: 'book-1#1'})\
                 -[:NEXT]->(:Chunk {id: 'book-1#2'}) RETURN count(*)",
                &[]
            ),
            1
        );
        assert_eq!(
            count(
                &svc,
                "MATCH (:Entity {name: 'Oslo'})-[:MENTIONED_IN]->(c:Chunk) RETURN count(DISTINCT c)",
                &[]
            ),
            2
        );
        let finished = ok(
            &svc,
            "MATCH (j:IngestJob {id: $job}) \
             RETURN j.started_at IS NOT NULL, j.finished_at IS NOT NULL",
            &[("job", s(&job))],
        );
        assert_eq!(finished.rows[0], vec![Value::Bool(true), Value::Bool(true)]);
        svc.stop_ingest();
    }

    #[test]
    fn a_failing_upstream_is_retried_until_it_answers() {
        let _s = serial();
        let llm = Upstream::start(&[Step::Status(500), Step::Status(503)], Step::Answer);
        let svc = started();
        let (job, _) = submit(
            &svc,
            Value::List(vec![s(MANUSCRIPT)]),
            map(&[("source", map(&[("id", s("ms-1"))]))]),
        );
        let st = wait_for(&svc, &job, LONG, |st| st.state == "done");
        assert_eq!(st.done, 1);
        let (state, attempts, last_error) = item(&svc, &job, 0);
        assert_eq!((state.as_str(), attempts), ("done", 3));
        assert_eq!(last_error, Value::Null);
        assert_eq!(llm.calls(), 3);
        svc.stop_ingest();
    }

    #[test]
    fn an_item_out_of_attempts_is_dead_until_retried() {
        let _s = serial();
        #[cfg(feature = "http")]
        install_problem_layer();
        let llm = Upstream::start(&[Step::Status(503), Step::Status(503)], Step::Answer);
        let svc = started();
        let (job, _) = submit(
            &svc,
            Value::List(vec![s(MANUSCRIPT)]),
            map(&[
                ("source", map(&[("id", s("ms-2"))])),
                ("maxAttempts", Value::Integer(2)),
            ]),
        );
        let st = wait_for(&svc, &job, LONG, |st| st.state == "failed");
        assert_eq!((st.done, st.failed), (0, 1));
        assert_eq!(st.items, counts(&[("dead", 1)]));
        assert_eq!(llm.calls(), 2);
        let errors = ok(
            &svc,
            "CALL drevo.ingest.errors($job, 10) YIELD seq, attempts, last_error \
             RETURN seq, attempts, last_error",
            &[("job", s(&job))],
        );
        assert_eq!(errors.rows.len(), 1);
        assert_eq!(int(&errors.rows[0][0]), 0);
        assert_eq!(int(&errors.rows[0][1]), 2);
        let message = string(&errors.rows[0][2]);
        assert!(
            message.starts_with("extraction upstream error: upstream returned 503"),
            "{message}"
        );
        assert!(message.contains("scripted failure"), "{message}");
        // The problem feed says so.
        #[cfg(feature = "http")]
        let (problems, _) = drevo::problems::ProblemLog::global().since(0);
        #[cfg(feature = "http")]
        assert!(
            problems.iter().any(|p| p.target == "drevo::ingest"
                && p.level == "ERROR"
                && p.fields.get("job").is_some_and(|j| j == &job)),
            "{problems:?}"
        );
        // Retrying requeues the dead item with fresh attempts.
        let res = ok(
            &svc,
            "CALL drevo.ingest.retryFailed($job) YIELD requeued RETURN requeued",
            &[("job", s(&job))],
        );
        assert_eq!(int(&res.rows[0][0]), 1);
        let st = wait_for(&svc, &job, LONG, |st| st.state == "done");
        assert_eq!((st.done, st.failed), (1, 0));
        let (state, attempts, _) = item(&svc, &job, 0);
        assert_eq!((state.as_str(), attempts), ("done", 1));
        assert_eq!(llm.calls(), 3);
        svc.stop_ingest();
    }

    #[cfg(feature = "http")]
    fn install_problem_layer() {
        use tracing_subscriber::layer::SubscriberExt;
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let subscriber = tracing_subscriber::registry()
                .with(drevo::problems::ProblemLayer::global())
                .with(CountUnusableConfig);
            let _ = tracing::subscriber::set_global_default(subscriber);
        });
    }

    /// Warnings "an ingest job has an unusable config" logged so far: one
    /// per scheduler pass while such a job is active.
    #[cfg(feature = "http")]
    static UNUSABLE_CONFIG_WARNINGS: AtomicUsize = AtomicUsize::new(0);

    /// Counts [`UNUSABLE_CONFIG_WARNINGS`].
    #[cfg(feature = "http")]
    struct CountUnusableConfig;

    #[cfg(feature = "http")]
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CountUnusableConfig {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Message(bool);
            impl tracing::field::Visit for Message {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message"
                        && format!("{value:?}").contains("an ingest job has an unusable config")
                    {
                        self.0 = true;
                    }
                }
            }
            if event.metadata().target() != "drevo::ingest" {
                return;
            }
            let mut message = Message(false);
            event.record(&mut message);
            if message.0 {
                UNUSABLE_CONFIG_WARNINGS.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[test]
    fn a_client_error_is_dead_at_once_and_a_rate_limit_is_retried() {
        let _s = serial();
        let llm = Upstream::start(&[Step::Status(400)], Step::Answer);
        let svc = started();
        let (job, _) = submit(
            &svc,
            Value::List(vec![s(MANUSCRIPT)]),
            map(&[("source", map(&[("id", s("ms-3"))]))]),
        );
        let st = wait_for(&svc, &job, LONG, |st| st.state == "failed");
        assert_eq!(st.items, counts(&[("dead", 1)]));
        let (_, attempts, error) = item(&svc, &job, 0);
        assert_eq!(attempts, 1);
        assert!(
            string(&error).contains("upstream returned 400"),
            "{error:?}"
        );
        assert_eq!(llm.calls(), 1);

        llm.fake.script.lock().unwrap().steps = [Step::Status(429)].into_iter().collect();
        let (job, _) = submit(
            &svc,
            Value::List(vec![s(MANUSCRIPT)]),
            map(&[("source", map(&[("id", s("ms-4"))]))]),
        );
        wait_for(&svc, &job, LONG, |st| st.state == "done");
        assert_eq!(item(&svc, &job, 0).1, 2);
        svc.stop_ingest();
    }

    #[test]
    fn a_job_runs_at_most_its_concurrency_at_once() {
        let _s = serial();
        workers(4);
        let llm = Upstream::start(&[], Step::Slow(150));
        let svc = started();
        let items: Vec<Value> = (0..6)
            .map(|i| s(&format!("Dana wrote chapter {i}.")))
            .collect();
        let (job, _) = submit(
            &svc,
            Value::List(items),
            map(&[
                ("source", map(&[("id", s("chapters"))])),
                ("concurrency", Value::Integer(2)),
            ]),
        );
        wait_for(&svc, &job, LONG, |st| st.state == "done");
        assert_eq!(llm.calls(), 6);
        assert_eq!(llm.most_in_flight(), 2);
        svc.stop_ingest();
    }

    #[test]
    fn the_server_cap_bounds_every_job() {
        let _s = serial();
        workers(1);
        let llm = Upstream::start(&[], Step::Slow(60));
        let svc = started();
        let mut jobs = Vec::new();
        for n in 0..2 {
            let (job, _) = submit(
                &svc,
                Value::List(vec![s("Dana in Oslo."), s("Acme in Oslo.")]),
                map(&[
                    ("source", map(&[("id", s(&format!("cap-{n}")))])),
                    ("concurrency", Value::Integer(8)),
                ]),
            );
            jobs.push(job);
        }
        for job in &jobs {
            wait_for(&svc, job, LONG, |st| st.state == "done");
        }
        assert_eq!(llm.most_in_flight(), 1);
        svc.stop_ingest();
    }

    #[test]
    fn pause_stops_picking_resume_goes_on_and_cancel_skips_the_rest() {
        let _s = serial();
        workers(1);
        let llm = Upstream::start(&[], Step::Hang);
        let svc = started();
        let items: Vec<Value> = (0..5)
            .map(|i| s(&format!("Page {i} of the draft.")))
            .collect();
        let (job, _) = submit(
            &svc,
            Value::List(items),
            map(&[("source", map(&[("id", s("draft"))]))]),
        );
        wait_until(LONG, "the first item to start", || llm.in_flight() == 1);
        ok(&svc, "CALL drevo.ingest.pause($job)", &[("job", s(&job))]);
        // The running item finishes; nothing new starts.
        llm.release();
        let st = wait_for(&svc, &job, LONG, |st| st.done == 1);
        assert_eq!(st.state, "paused");
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(llm.calls(), 1);
        assert_eq!(
            status(&svc, &job).items,
            counts(&[("done", 1), ("queued", 4)])
        );
        // Resume: the next item starts.
        llm.hold();
        let resumed = ok(
            &svc,
            "CALL drevo.ingest.resume($job) YIELD job RETURN job.state",
            &[("job", s(&job))],
        );
        assert_eq!(string(&resumed.rows[0][0]), "running");
        wait_until(LONG, "the second item to start", || {
            llm.calls() == 2 && llm.in_flight() == 1
        });
        // Cancel: the running item finishes, the rest stay queued.
        let cancelled = ok(
            &svc,
            "CALL drevo.ingest.cancel($job) YIELD job RETURN job.state",
            &[("job", s(&job))],
        );
        assert_eq!(string(&cancelled.rows[0][0]), "cancelled");
        assert_eq!(
            invalid_call(&svc, "CALL drevo.ingest.purge($job)", &[("job", s(&job))]),
            format!("job `{job}` still has an item running: purge it once that finishes")
        );
        llm.release();
        let st = wait_for(&svc, &job, LONG, |st| st.done == 2);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(llm.calls(), 2);
        let st2 = status(&svc, &job);
        assert_eq!(st2.state, "cancelled");
        assert_eq!(st2.items, counts(&[("done", 2), ("queued", 3)]));
        assert_eq!(st.done, 2);
        svc.stop_ingest();
    }

    #[test]
    fn a_higher_priority_job_goes_first() {
        let _s = serial();
        workers(1);
        let llm = Upstream::start(&[Step::Hang], Step::Answer);
        let svc = started();
        let (low, _) = submit(
            &svc,
            Value::List(vec![s("low one"), s("low two"), s("low three")]),
            map(&[("source", map(&[("id", s("low"))]))]),
        );
        wait_until(LONG, "the first low item to start", || llm.in_flight() == 1);
        let (high, _) = submit(
            &svc,
            Value::List(vec![s("high one"), s("high two")]),
            map(&[
                ("source", map(&[("id", s("high"))])),
                ("priority", Value::Integer(5)),
            ]),
        );
        llm.release();
        wait_for(&svc, &low, LONG, |st| st.state == "done");
        wait_for(&svc, &high, LONG, |st| st.state == "done");
        assert_eq!(
            llm.passages(),
            ["low one", "high one", "high two", "low two", "low three"]
        );
        svc.stop_ingest();
    }

    #[test]
    fn a_job_survives_a_restart_and_runs_its_interrupted_item_again() {
        let _s = serial();
        workers(1);
        let llm = Upstream::start(&[Step::Hang], Step::Answer);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native.wal");
        let job = {
            let svc = Arc::new(NativeService::open(&path).expect("open"));
            svc.start_ingest();
            let (job, _) = submit(
                &svc,
                Value::List(vec![s("Dana left Acme."), s("Acme stayed in Oslo.")]),
                map(&[("source", map(&[("id", s("memoir"))]))]),
            );
            wait_until(LONG, "the first item to start", || llm.in_flight() == 1);
            assert_eq!(item(&svc, &job, 0).0, "running");
            svc.stop_ingest();
            drop(svc);
            job
        };
        // The interrupted call answers late: the closed database ignores it.
        llm.release();
        let svc = Arc::new(NativeService::open(&path).expect("reopen"));
        assert_eq!(item(&svc, &job, 0).0, "running");
        assert_eq!(status(&svc, &job).state, "running");
        svc.start_ingest();
        let st = wait_for(&svc, &job, LONG, |st| st.state == "done");
        assert_eq!((st.done, st.total), (2, 2));
        // One attempt was cut short by the restart.
        assert_eq!(item(&svc, &job, 0).1, 2);
        assert_eq!(
            count(
                &svc,
                "MATCH (:IngestItem)-[:PRODUCED]->(c:Chunk) RETURN count(c)",
                &[]
            ),
            2
        );
        svc.stop_ingest();
    }

    #[test]
    fn a_paused_job_stays_paused_after_a_restart() {
        let _s = serial();
        let llm = Upstream::start(&[], Step::Answer);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native.wal");
        let job = {
            let svc = Arc::new(NativeService::open(&path).expect("open"));
            let (job, _) = submit(
                &svc,
                Value::List(vec![s("Dana left Acme.")]),
                map(&[("source", map(&[("id", s("memoir-2"))]))]),
            );
            ok(&svc, "CALL drevo.ingest.pause($job)", &[("job", s(&job))]);
            job
        };
        let svc = Arc::new(NativeService::open(&path).expect("reopen"));
        svc.start_ingest();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(status(&svc, &job).state, "paused");
        assert_eq!(llm.calls(), 0);
        ok(&svc, "CALL drevo.ingest.resume($job)", &[("job", s(&job))]);
        wait_for(&svc, &job, LONG, |st| st.state == "done");
        svc.stop_ingest();
    }

    #[test]
    fn bolt_submits_and_reads_jobs() {
        use drevo::bolt::packstream::Value as Bolt;
        use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
        let _s = serial();
        let _llm = Upstream::start(&[], Step::Answer);
        let svc = started();
        let mut session = Session::new_durable(Arc::clone(&svc));
        session.handle(ClientMessage::Hello {
            extra: BTreeMap::new(),
        });
        let mut run_bolt = |query: &str, params: BTreeMap<String, Bolt>| -> Vec<Vec<Bolt>> {
            let mut replies = session.handle(ClientMessage::Run {
                query: query.to_string(),
                parameters: params,
                extra: BTreeMap::new(),
            });
            replies.extend(session.handle(ClientMessage::Pull {
                extra: BTreeMap::new(),
            }));
            replies
                .into_iter()
                .filter_map(|m| match m {
                    ServerMessage::Record { fields } => Some(fields),
                    _ => None,
                })
                .collect()
        };
        let rows = run_bolt(
            "CALL drevo.ingest.submit($items, {source: {id: 'bolt-book'}}) \
             YIELD jobId, total RETURN jobId, total",
            BTreeMap::from([(
                "items".to_string(),
                Bolt::List(vec![Bolt::String("Dana met Acme.".into())]),
            )]),
        );
        let Bolt::String(job) = rows[0][0].clone() else {
            panic!("{rows:?}")
        };
        assert_eq!(rows[0][1], Bolt::Integer(1));
        wait_for(&svc, &job, LONG, |st| st.state == "done");
        let rows = run_bolt(
            "CALL drevo.ingest.status($job) YIELD job, items RETURN job.state, items.done",
            BTreeMap::from([("job".to_string(), Bolt::String(job.clone()))]),
        );
        assert_eq!(
            rows,
            vec![vec![Bolt::String("done".into()), Bolt::Integer(1)]]
        );
        svc.stop_ingest();
    }

    #[cfg(feature = "http")]
    #[test]
    fn progress_arrives_as_ingest_events_and_in_the_metrics() {
        use axum::body::Body;
        use axum::http::Request;
        use drevo::native_api::{build_native_router, NativeApiState};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let _s = serial();
        let _llm = Upstream::start(&[], Step::Slow(120));
        let svc = started();
        let app = build_native_router(NativeApiState::new(Arc::clone(&svc)));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/events?names=ingest")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let mut body = resp.into_body();
            let items: Vec<Value> = (0..6)
                .map(|i| s(&format!("Dana in Oslo, part {i}.")))
                .collect();
            let (job, total) = submit(
                &svc,
                Value::List(items),
                map(&[("source", map(&[("id", s("sse-book"))]))]),
            );
            let started_at = Instant::now();
            let mut buffer = String::new();
            let mut events: Vec<(String, serde_json::Value)> = Vec::new();
            let deadline = tokio::time::Instant::now() + LONG;
            loop {
                while let Some(end) = buffer.find("\n\n") {
                    let block: String = buffer.drain(..end + 2).collect();
                    let mut name = String::new();
                    let mut data = String::new();
                    for line in block.lines() {
                        if let Some(v) = line.strip_prefix("event:") {
                            name = v.trim().to_string();
                        } else if let Some(v) = line.strip_prefix("data:") {
                            data.push_str(v.trim());
                        }
                    }
                    if !data.is_empty() {
                        events.push((name, serde_json::from_str(&data).unwrap()));
                    }
                }
                if events.last().is_some_and(|(_, d)| d["state"] == "done") {
                    break;
                }
                let left = deadline
                    .checked_duration_since(tokio::time::Instant::now())
                    .unwrap_or_else(|| panic!("no final event; got {events:?}"));
                match tokio::time::timeout(left, body.frame()).await {
                    Ok(Some(Ok(frame))) => {
                        if let Ok(bytes) = frame.into_data() {
                            buffer.push_str(&String::from_utf8_lossy(&bytes));
                        }
                    }
                    other => panic!("stream ended: {other:?}; got {events:?}"),
                }
            }
            let elapsed = started_at.elapsed().as_secs_f64();
            assert!(
                events.iter().all(|(name, _)| name == "ingest"),
                "{events:?}"
            );
            assert!(events.iter().all(|(_, d)| d["jobId"] == job.as_str()));
            let done: Vec<i64> = events
                .iter()
                .map(|(_, d)| d["done"].as_i64().unwrap())
                .collect();
            assert!(done.windows(2).all(|w| w[0] <= w[1]), "{done:?}");
            let last = &events.last().unwrap().1;
            assert_eq!(last["done"], total);
            assert_eq!(last["total"], total);
            assert_eq!(last["failed"], 0);
            // At most one event a second (and the first at once).
            assert!(
                (events.len() as f64) <= elapsed + 2.0,
                "{} events in {elapsed:.1}s",
                events.len()
            );
            let metrics = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/metrics")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let text = String::from_utf8(
                metrics
                    .into_body()
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes()
                    .to_vec(),
            )
            .unwrap();
            let done_total = text
                .lines()
                .find(|l| l.starts_with("drevo_ingest_items_total{state=\"done\"}"))
                .unwrap_or_else(|| panic!("{text}"));
            let n: f64 = done_total.rsplit(' ').next().unwrap().parse().unwrap();
            assert!(n >= total as f64, "{done_total}");
            assert!(text.contains("drevo_ingest_jobs_active "), "{text}");
        });
        svc.stop_ingest();
    }

    // ----- #806: robustness of the scheduler ------------------------------------

    /// Stops the scheduler, after letting a hanging upstream answer, however
    /// the test ends.
    struct Stop<'a> {
        svc: &'a NativeService,
        llm: Option<&'a Upstream>,
    }

    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            if let Some(llm) = self.llm {
                llm.release();
            }
            self.svc.stop_ingest();
        }
    }

    const FINAL: [&str; 3] = ["done", "failed", "cancelled"];

    /// Poll the job's state for up to `limit`; the last one seen.
    fn settle(svc: &NativeService, job: &str, limit: Duration) -> Status {
        let deadline = Instant::now() + limit;
        loop {
            let st = status(svc, job);
            if FINAL.contains(&st.state.as_str()) || Instant::now() >= deadline {
                return st;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn an_item_deleted_mid_flight_does_not_stay_running_forever() {
        let _s = serial();
        let llm = Upstream::start(&[Step::Hang], Step::Answer);
        let svc = started();
        let _stop = Stop {
            svc: &svc,
            llm: Some(&llm),
        };
        let (job, _) = submit(
            &svc,
            Value::List(vec![s("Dana joined Acme.")]),
            map(&[("source", map(&[("id", s("deleted-item"))]))]),
        );
        wait_until(LONG, "the item to start", || llm.in_flight() == 1);
        assert_eq!(item(&svc, &job, 0).0, "running");
        // The item goes away while the model answers it.
        ok(
            &svc,
            "MATCH (i:IngestItem {job_id: $job}) DETACH DELETE i",
            &[("job", s(&job))],
        );
        llm.release();
        wait_until(LONG, "the model to answer", || llm.in_flight() == 0);
        let st = settle(&svc, &job, Duration::from_secs(5));
        let running = count(
            &svc,
            "MATCH (i:IngestItem {job_id: $job, state: 'running'}) RETURN count(i)",
            &[("job", s(&job))],
        );
        assert!(
            FINAL.contains(&st.state.as_str()) && running == 0,
            "the job stayed `{}` 5 s after its item was deleted mid-flight ({running} item(s) \
             still running): write_item ignored count(i) = 0 and recorded the item as done, so \
             nothing ever finishes the job (#806); last status {st:?}",
            st.state
        );
    }

    #[test]
    fn a_chunk_id_mismatch_marks_the_item_failed_with_a_reason() {
        let _s = serial();
        let llm = Upstream::start(&[], Step::Answer);
        let svc = Arc::new(NativeService::in_memory());
        let _stop = Stop {
            svc: &svc,
            llm: Some(&llm),
        };
        let (job, _) = submit(
            &svc,
            Value::List(vec![s("Dana joined Acme.")]),
            map(&[("source", map(&[("id", s("book-1"))]))]),
        );
        // The job's source id no longer matches its config's `source`: the
        // item's chunk is written as `book-1#0` and looked up as `book-2#0`.
        ok(
            &svc,
            "MATCH (j:IngestJob {id: $job}) SET j.source_id = 'book-2'",
            &[("job", s(&job))],
        );
        svc.start_ingest();
        wait_until(LONG, "the model to be asked", || llm.calls() == 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        let (state, attempts, error) = loop {
            let (state, attempts, error) = item(&svc, &job, 0);
            if (state != "running" && state != "queued") || Instant::now() >= deadline {
                break (state, attempts, error);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(
            (state == "dead" || state == "failed") && error != Value::Null,
            "the item stayed `{state}` with last_error {error:?} (attempt {attempts}) 5 s after \
             its chunk `book-2#0` was not found: write_item ignored count(i) = 0 and recorded \
             the item as done (#806); job {:?}",
            status(&svc, &job)
        );
    }

    #[cfg(feature = "http")]
    #[test]
    fn the_scheduler_sleeps_when_no_extractor_is_configured() {
        let _s = serial();
        install_problem_layer();
        drevo::extract::uninstall();
        let svc = Arc::new(NativeService::in_memory());
        let _stop = Stop {
            svc: &svc,
            llm: None,
        };
        let (_waiting, _) = submit(
            &svc,
            Value::List(vec![s(MANUSCRIPT)]),
            map(&[("source", map(&[("id", s("ms-a"))]))]),
        );
        // A second job whose stored config no longer parses: every pass of
        // the scheduler warns about it once, which makes the passes countable.
        let (marker, _) = submit(
            &svc,
            Value::List(vec![s(MANUSCRIPT)]),
            map(&[("source", map(&[("id", s("ms-b"))]))]),
        );
        ok(
            &svc,
            "MATCH (j:IngestJob {id: $job}) SET j.config = $config",
            &[
                ("job", s(&marker)),
                ("config", s(r#"{"source": {"id": "ms-b"}, "chunkSize": 1}"#)),
            ],
        );
        let before = UNUSABLE_CONFIG_WARNINGS.load(Ordering::SeqCst);
        svc.start_ingest();
        wait_until(LONG, "the scheduler's first pass", || {
            UNUSABLE_CONFIG_WARNINGS.load(Ordering::SeqCst) > before
        });
        let first = UNUSABLE_CONFIG_WARNINGS.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_secs(5));
        let passes = UNUSABLE_CONFIG_WARNINGS.load(Ordering::SeqCst) - first;
        assert!(
            passes <= 1,
            "the scheduler made {passes} more passes in 5 s with jobs queued and no extractor \
             configured: pass() returns Some(POLL) instead of sleeping until woken (#806)"
        );
    }

    #[test]
    fn installing_an_extractor_wakes_the_sleeping_scheduler_and_the_job_runs() {
        let _s = serial();
        drevo::extract::uninstall();
        let svc = started();
        let _stop = Stop {
            svc: &svc,
            llm: None,
        };
        let (job, _) = submit(
            &svc,
            Value::List(vec![s("Dana joined Acme.")]),
            map(&[("source", map(&[("id", s("waits-for-model"))]))]),
        );
        // No extractor: the job waits, its item untouched.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(status(&svc, &job).state, "queued");
        assert_eq!(item(&svc, &job, 0).0, "queued");
        // Installing one (the path `POST /config/extract` takes) wakes the
        // sleeping scheduler, which never polls while nothing can run.
        let llm = Upstream::start(&[], Step::Answer);
        let st = wait_for(&svc, &job, LONG, |st| st.state == "done");
        assert_eq!((st.done, st.failed, st.total), (1, 0, 1));
        assert_eq!(llm.calls(), 1);
        assert_eq!(
            count(
                &svc,
                "MATCH (:IngestItem {job_id: $job})-[:PRODUCED]->(c:Chunk {id: 'waits-for-model#0'}) \
                 RETURN count(c)",
                &[("job", s(&job))],
            ),
            1
        );
    }

    #[test]
    fn a_job_deleted_with_its_items_mid_flight_ends_cleanly_and_frees_its_slot() {
        let _s = serial();
        workers(1);
        let llm = Upstream::start(&[Step::Hang], Step::Answer);
        let svc = started();
        let _stop = Stop {
            svc: &svc,
            llm: Some(&llm),
        };
        let (gone, _) = submit(
            &svc,
            Value::List(vec![s("Dana joined Acme.")]),
            map(&[("source", map(&[("id", s("gone-job"))]))]),
        );
        wait_until(LONG, "the item to start", || llm.in_flight() == 1);
        ok(
            &svc,
            "MATCH (j:IngestJob {id: $job}) OPTIONAL MATCH (i:IngestItem {job_id: $job}) \
             DETACH DELETE j, i",
            &[("job", s(&gone))],
        );
        llm.release();
        wait_until(LONG, "the model to answer", || llm.in_flight() == 0);
        // The only slot comes back: the next job runs to the end.
        let (next, _) = submit(
            &svc,
            Value::List(vec![s("Acme is in Oslo.")]),
            map(&[("source", map(&[("id", s("next-job"))]))]),
        );
        let st = wait_for(&svc, &next, LONG, |st| st.state == "done");
        assert_eq!((st.done, st.total), (1, 1));
        // Nothing was written for, or left behind by, the deleted job.
        for q in [
            "MATCH (j:IngestJob {id: $job}) RETURN count(j)",
            "MATCH (i:IngestItem {job_id: $job}) RETURN count(i)",
            "MATCH (c:Chunk {id: 'gone-job#0'}) RETURN count(c)",
            "MATCH (i:IngestItem {state: 'running'}) RETURN count(i)",
        ] {
            assert_eq!(count(&svc, q, &[("job", s(&gone))]), 0, "{q}");
        }
    }
}
