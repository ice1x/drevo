//! Opt-in statement timeout — issue #547 (second half).
//!
//! Running Cypher off the async workers keeps the server *live* while a slow
//! statement runs, but the statement itself still runs to completion and
//! holds the service's index lock. With `DREVO_QUERY_TIMEOUT_MS` (or
//! [`NativeService::set_statement_timeout`]) a statement that exceeds the
//! limit fails with [`ExecError::Timeout`] instead, releasing everything it
//! held. Over Bolt that is Neo4j's `Neo.ClientError.Transaction.TransactionTimedOut`.
//!
//! These tests lock:
//! - off by default — no statement is ever cut short unless configured;
//! - a CPU-bound expression (`reduce`) and a traversal-bound pattern (the
//!   trail-enumerating `shortestPath` fallback) both stop near the limit;
//! - fast statements are unaffected, and the deadline never leaks into the
//!   next statement;
//! - databases created through the registry inherit the default's limit;
//! - the Bolt FAILURE code and the env-var parsing.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use drevo::bolt::packstream::Value as BoltValue;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::database_registry::DatabaseRegistry;
use drevo::native_service::NativeService;

fn exec(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
}

/// CPU-bound, constant-memory statement: `n²` additions via nested `reduce`.
fn slow_query(n: u32) -> String {
    format!("RETURN reduce(s = 0, i IN range(1, {n}) | s + reduce(t = 0, j IN range(1, {n}) | t + j)) AS v")
}

/// Grow `n` until one statement takes at least 1.5 s here (no timeout set).
fn calibrate(svc: &NativeService) -> u32 {
    let mut n = 400;
    loop {
        let t = Instant::now();
        exec(svc, &slow_query(n)).expect("slow statement");
        if t.elapsed() >= Duration::from_millis(1500) || n >= 20_000 {
            return n;
        }
        n *= 2;
    }
}

const LIMIT: Duration = Duration::from_millis(150);

#[test]
fn no_timeout_by_default() {
    let svc = NativeService::in_memory();
    assert_eq!(svc.statement_timeout(), None);
    let n = calibrate(&svc); // completes: nothing cuts it short
    assert!(n >= 400);
}

#[test]
fn a_cpu_bound_statement_stops_near_the_limit() {
    let svc = NativeService::in_memory();
    let n = calibrate(&svc);
    svc.set_statement_timeout(Some(LIMIT));
    assert_eq!(svc.statement_timeout(), Some(LIMIT));

    let t = Instant::now();
    let err = exec(&svc, &slow_query(n)).expect_err("must time out");
    let took = t.elapsed();
    match err {
        ExecError::Timeout { limit_ms } => assert_eq!(limit_ms, 150),
        other => panic!("expected Timeout, got {other:?}"),
    }
    assert!(took < Duration::from_millis(1000), "stopped after {took:?}");
}

#[test]
fn a_traversal_bound_statement_stops_near_the_limit() {
    // `[*2..]` keeps the trail-enumerating shortestPath fallback (#546). On a
    // dense component with unreachable targets (nodes 60..79 are isolated) it
    // enumerates every trail to the bound without evaluating any expression
    // per step — the loop itself has to honour the deadline.
    let svc = NativeService::in_memory();
    exec(&svc, "UNWIND range(0, 79) AS i CREATE (:D {i: i})").expect("nodes");
    exec(
        &svc,
        "MATCH (a:D), (b:D) WHERE a.i < b.i AND b.i < 60 AND (a.i + b.i) % 3 = 0 \
         CREATE (a)-[:R]->(b)",
    )
    .expect("edges");
    svc.set_statement_timeout(Some(LIMIT));

    let t = Instant::now();
    let err = exec(
        &svc,
        "MATCH p = allShortestPaths((a:D {i: 0})-[*2..12]-(b:D)) RETURN count(p) AS c",
    )
    .expect_err("must time out");
    assert!(matches!(err, ExecError::Timeout { .. }), "got {err:?}");
    assert!(
        t.elapsed() < Duration::from_millis(1000),
        "stopped after {:?}",
        t.elapsed()
    );
}

#[test]
fn fast_statements_run_and_the_deadline_does_not_leak() {
    let svc = NativeService::in_memory();
    let n = calibrate(&svc);
    svc.set_statement_timeout(Some(LIMIT));

    exec(&svc, &slow_query(n)).expect_err("times out");
    // The next statements start with a fresh deadline.
    for _ in 0..3 {
        let res = exec(&svc, "UNWIND range(1, 100) AS x RETURN sum(x) AS s").expect("fast");
        assert_eq!(res.rows[0][0], Value::Integer(5050));
    }
    // Clearing the limit lets the slow statement finish again.
    svc.set_statement_timeout(None);
    exec(&svc, &slow_query(n)).expect("no limit");
}

#[test]
fn registry_databases_share_the_default_limit() {
    let registry = DatabaseRegistry::new(Arc::new(NativeService::in_memory()));
    let before = registry.create("before").expect("create");
    registry.set_statement_timeout(Some(LIMIT));
    let after = registry.create("after").expect("create");

    assert_eq!(registry.default_service().statement_timeout(), Some(LIMIT));
    assert_eq!(before.statement_timeout(), Some(LIMIT));
    assert_eq!(after.statement_timeout(), Some(LIMIT));
}

#[test]
fn bolt_reports_transaction_timed_out() {
    let svc = Arc::new(NativeService::in_memory());
    let n = calibrate(&svc);
    svc.set_statement_timeout(Some(LIMIT));

    let mut s = Session::new_durable(Arc::clone(&svc));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    let replies = s.handle(ClientMessage::Run {
        query: slow_query(n),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    match replies.as_slice() {
        [ServerMessage::Failure { metadata }] => assert_eq!(
            metadata.get("code"),
            Some(&BoltValue::String(
                "Neo.ClientError.Transaction.TransactionTimedOut".into()
            ))
        ),
        other => panic!("expected one FAILURE, got {other:?}"),
    }
}

#[cfg(feature = "http")]
mod config {
    use std::time::Duration;

    use drevo::server::Config;

    fn cfg(timeout: Option<&str>) -> Result<Config, drevo::server::ConfigError> {
        Config::from_env(|key| match key {
            "DREVO_QUERY_TIMEOUT_MS" => timeout.map(str::to_string),
            _ => None,
        })
    }

    #[test]
    fn query_timeout_env_var_parses() {
        assert_eq!(cfg(None).expect("unset").query_timeout, None);
        assert_eq!(cfg(Some("0")).expect("zero = off").query_timeout, None);
        assert_eq!(
            cfg(Some("2500")).expect("ms").query_timeout,
            Some(Duration::from_millis(2500))
        );
        assert!(cfg(Some("soon")).is_err());
        assert!(cfg(Some("-5")).is_err());
    }
}
