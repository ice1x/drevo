//! `CALL drevo.events.post(name, payload)`: named application events,
//! delivered on commit — over `GET /events` as `posted` events, and through
//! [`NativeService::event_bus`] for embedders.
//!
//! - An event reaches subscribers only once its statement or transaction
//!   commits, after the changes it committed; a failed statement or a
//!   rolled-back transaction posts nothing.
//! - It is posted once per row, with arguments that may use the row.
//! - The stream position `<cursor>:<n>` resumes exactly; `names` filters.
#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, Session};
use drevo::cypher::executor::{ExecError, Value};
use drevo::cypher::parser::parse;
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// One received SSE event.
#[derive(Debug, Clone)]
struct Event {
    id: String,
    name: String,
    data: serde_json::Value,
}

/// An open `/events` stream.
struct Stream {
    body: Body,
    buffer: String,
}

impl Stream {
    async fn open(app: &axum::Router, uri: &str, last_event_id: Option<&str>) -> Self {
        let mut req = Request::builder().uri(uri);
        if let Some(id) = last_event_id {
            req = req.header("last-event-id", id);
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
        Self {
            body: resp.into_body(),
            buffer: String::new(),
        }
    }

    /// The next event (keep-alive comments skipped), or `None` after `wait`.
    async fn next(&mut self, wait: Duration) -> Option<Event> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                let mut event = Event {
                    id: String::new(),
                    name: "message".into(),
                    data: serde_json::Value::Null,
                };
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("id:") {
                        event.id = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("event:") {
                        event.name = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim());
                    }
                }
                if data.is_empty() && event.id.is_empty() {
                    continue; // a comment / keep-alive
                }
                event.data = serde_json::from_str(&data).unwrap_or(serde_json::Value::Null);
                return Some(event);
            }
            let left = deadline.checked_duration_since(tokio::time::Instant::now())?;
            match tokio::time::timeout(left, self.body.frame()).await {
                Ok(Some(Ok(frame))) => {
                    if let Ok(bytes) = frame.into_data() {
                        self.buffer.push_str(&String::from_utf8_lossy(&bytes));
                    }
                }
                _ => return None,
            }
        }
    }

    async fn take(&mut self, n: usize) -> Vec<Event> {
        let mut events = Vec::new();
        while events.len() < n {
            match self.next(Duration::from_secs(5)).await {
                Some(e) => events.push(e),
                None => panic!("only {} of {n} events arrived: {events:?}", events.len()),
            }
        }
        events
    }

    async fn quiet(&mut self) {
        if let Some(e) = self.next(Duration::from_millis(700)).await {
            panic!("unexpected event {e:?}");
        }
    }
}

fn run(svc: &NativeService, q: &str) {
    svc.execute(&parse(q).unwrap(), HashMap::new())
        .unwrap_or_else(|e| panic!("{q}: {e}"));
}

fn fails(svc: &NativeService, q: &str) -> ExecError {
    match svc.execute(&parse(q).unwrap(), HashMap::new()) {
        Ok(r) => panic!("{q} ran: {:?}", r.rows),
        Err(e) => e,
    }
}

fn server() -> (Arc<NativeService>, axum::Router) {
    let svc = Arc::new(NativeService::in_memory());
    let app = build_native_router(NativeApiState::new(Arc::clone(&svc)));
    (svc, app)
}

/// The names of the published events, oldest first.
fn published(svc: &NativeService) -> Vec<String> {
    svc.event_bus()
        .after(0)
        .events
        .into_iter()
        .map(|e| e.name)
        .collect()
}

fn dict(e: Vec<(&str, Bolt)>) -> BTreeMap<String, Bolt> {
    e.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn bolt_run(session: &mut Session, query: &str) {
    session.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: dict(vec![]),
        extra: dict(vec![]),
    });
    session.handle(ClientMessage::Pull {
        extra: dict(vec![("n", Bolt::Integer(-1))]),
    });
}

#[tokio::test]
async fn a_posted_event_follows_the_changes_its_statement_committed() {
    let (svc, app) = server();
    let mut s = Stream::open(&app, "/events", None).await;
    run(
        &svc,
        "CREATE (i:Invoice {no: 'INV-7', status: 'approved'}) \
         WITH i CALL drevo.events.post('invoice_approved', {invoice: i.no}) \
         RETURN i.no",
    );
    let events = s.take(2).await;
    assert_eq!(events[0].name, "node");
    assert_eq!(events[0].id, "1");
    assert_eq!(events[1].name, "posted");
    assert_eq!(events[1].id, "1:1");
    assert_eq!(
        events[1].data,
        serde_json::json!({
            "name": "invoice_approved",
            "payload": {"invoice": "INV-7"},
            "cursor": 1,
        })
    );
    s.quiet().await;
}

#[test]
fn an_event_is_posted_once_per_row_with_that_row_s_values() {
    let svc = NativeService::in_memory();
    run(
        &svc,
        "UNWIND ['T-1', 'T-2', 'T-3'] AS task \
         CALL drevo.events.post('task_closed', {task: task}) YIELD name \
         RETURN task, name",
    );
    let after = svc.event_bus().after(0);
    let payloads: Vec<serde_json::Value> = after.events.iter().map(|e| e.payload.clone()).collect();
    assert_eq!(
        payloads,
        vec![
            serde_json::json!({"task": "T-1"}),
            serde_json::json!({"task": "T-2"}),
            serde_json::json!({"task": "T-3"}),
        ]
    );
    assert_eq!(
        after.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [1, 2, 3]
    );
}

#[test]
fn a_standalone_post_returns_the_name_and_rows_pass_through() {
    let svc = NativeService::in_memory();
    let r = svc
        .execute(
            &parse("CALL drevo.events.post('journal_entry_saved', null)").unwrap(),
            HashMap::new(),
        )
        .unwrap();
    assert_eq!(r.columns, vec!["name".to_string()]);
    assert_eq!(
        r.rows,
        vec![vec![Value::String("journal_entry_saved".into())]]
    );

    let r = svc
        .execute(
            &parse(
                "UNWIND [1, 2] AS chapter \
                 CALL drevo.events.post('chapter_saved', chapter) \
                 RETURN chapter ORDER BY chapter",
            )
            .unwrap(),
            HashMap::new(),
        )
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );
    assert_eq!(
        published(&svc),
        ["journal_entry_saved", "chapter_saved", "chapter_saved"]
    );
}

#[test]
fn parameters_and_any_plain_value_make_a_payload() {
    let svc = NativeService::in_memory();
    let params = HashMap::from([
        ("bug".to_string(), Value::String("BUG-12".into())),
        ("severity".to_string(), Value::Integer(2)),
    ]);
    svc.execute(
        &parse(
            "CALL drevo.events.post('bug_reopened', \
             {bug: $bug, severity: $severity, tags: ['ui', 'regression'], fixed: false, score: 0.5})",
        )
        .unwrap(),
        params,
    )
    .unwrap();
    let after = svc.event_bus().after(0);
    assert_eq!(
        after.events[0].payload,
        serde_json::json!({
            "bug": "BUG-12",
            "severity": 2,
            "tags": ["ui", "regression"],
            "fixed": false,
            "score": 0.5,
        })
    );
}

#[test]
fn a_failed_statement_posts_nothing() {
    let svc = NativeService::in_memory();
    // The second post fails (a node is not a payload), after the first one.
    let e = fails(
        &svc,
        "CALL drevo.events.post('first', 1) YIELD name \
         CREATE (n:Order) WITH n CALL drevo.events.post('second', n) YIELD name AS second \
         RETURN second",
    );
    assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
    assert!(published(&svc).is_empty());
    assert_eq!(svc.event_bus().head(), 0);
}

#[test]
fn a_bad_name_is_refused() {
    let svc = NativeService::in_memory();
    let e = fails(&svc, "CALL drevo.events.post(42, null)");
    assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
    let e = fails(&svc, "CALL drevo.events.post('  ', null)");
    assert!(e.to_string().contains("empty"), "{e}");
    let e = fails(&svc, "CALL drevo.events.post('only-a-name')");
    assert!(e.to_string().contains("expected 2 arguments"), "{e}");
    assert!(published(&svc).is_empty());
}

#[tokio::test]
async fn an_explicit_transaction_posts_on_commit_and_nothing_on_rollback() {
    let (svc, app) = server();
    let mut s = Stream::open(&app, "/events?names=invoice_approved", None).await;
    let mut session = Session::new_durable(Arc::clone(&svc));
    session.handle(ClientMessage::Hello {
        extra: dict(vec![]),
    });

    session.handle(ClientMessage::Begin {
        extra: dict(vec![]),
    });
    bolt_run(
        &mut session,
        "CREATE (i:Invoice {no: 'INV-1'}) WITH i \
         CALL drevo.events.post('invoice_approved', {invoice: i.no}) YIELD name RETURN name",
    );
    s.quiet().await;
    session.handle(ClientMessage::Rollback);
    s.quiet().await;

    session.handle(ClientMessage::Begin {
        extra: dict(vec![]),
    });
    bolt_run(
        &mut session,
        "CREATE (i:Invoice {no: 'INV-2'}) WITH i \
         CALL drevo.events.post('invoice_approved', {invoice: i.no}) YIELD name RETURN name",
    );
    s.quiet().await;
    session.handle(ClientMessage::Commit);
    let events = s.take(1).await;
    assert_eq!(events[0].name, "posted");
    assert_eq!(
        events[0].data["payload"],
        serde_json::json!({"invoice": "INV-2"})
    );
    s.quiet().await;
    assert_eq!(published(&svc), ["invoice_approved"]);
}

#[test]
fn a_failed_statement_in_a_transaction_drops_only_its_own_events() {
    let svc = NativeService::in_memory();
    let tx = svc.begin_tx();
    svc.execute_in_tx(
        tx,
        &parse("CALL drevo.events.post('kept', 1)").unwrap(),
        HashMap::new(),
    )
    .unwrap();
    let e = svc
        .execute_in_tx(
            tx,
            &parse(
                "CREATE (n:N) WITH n CALL drevo.events.post('dropped', n) YIELD name RETURN name",
            )
            .unwrap(),
            HashMap::new(),
        )
        .unwrap_err();
    assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
    assert!(published(&svc).is_empty());
    svc.commit_tx(tx).unwrap();
    assert_eq!(published(&svc), ["kept"]);
}

#[test]
fn a_prepared_transaction_posts_when_it_commits() {
    let svc = NativeService::in_memory();
    for (gid, commit) in [("erp-g1", true), ("erp-g2", false)] {
        let tx = svc.begin_tx();
        svc.execute_in_tx(
            tx,
            &parse(&format!("CALL drevo.events.post('{gid}', null)")).unwrap(),
            HashMap::new(),
        )
        .unwrap();
        svc.prepare_tx(tx, gid).unwrap();
        assert!(published(&svc).is_empty() || published(&svc) == ["erp-g1"]);
        if commit {
            svc.commit_prepared(gid).unwrap();
        } else {
            svc.rollback_prepared(gid).unwrap();
        }
    }
    assert_eq!(published(&svc), ["erp-g1"]);
}

#[tokio::test]
async fn names_and_labels_choose_what_the_stream_carries() {
    let (svc, app) = server();
    let mut only_named = Stream::open(&app, "/events?names=task_closed", None).await;
    let mut only_tasks = Stream::open(&app, "/events?labels=Task", None).await;
    let mut both = Stream::open(&app, "/events?labels=Task&names=task_closed", None).await;
    run(
        &svc,
        "CREATE (t:Task {key: 'T-9'}) WITH t \
         CALL drevo.events.post('task_reviewed', {task: t.key}) YIELD name AS reviewed \
         CALL drevo.events.post('task_closed', {task: t.key}) YIELD name \
         RETURN name",
    );
    let named = only_named.take(1).await;
    assert_eq!(named[0].data["name"], "task_closed");
    // The position counts every named event, shown or not.
    assert_eq!(named[0].id, "1:2");
    only_named.quiet().await;

    let tasks = only_tasks.take(1).await;
    assert_eq!(tasks[0].name, "node");
    only_tasks.quiet().await;

    let union = both.take(2).await;
    assert_eq!(
        union.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
        ["node", "posted"]
    );
    assert_eq!(union[1].data["name"], "task_closed");
    both.quiet().await;
}

#[tokio::test]
async fn a_client_resumes_from_a_position_without_gaps_or_repeats() {
    let (svc, app) = server();
    let mut s = Stream::open(&app, "/events?since=0", None).await;
    run(&svc, "CALL drevo.events.post('a', 1)");
    run(&svc, "CREATE (:Bug {key: 'B-1'})");
    run(&svc, "CALL drevo.events.post('b', 2)");
    let first = s.take(3).await;
    assert_eq!(
        first.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["0:1", "1:1", "1:2"]
    );
    drop(s);

    // Disconnected after the first event: EventSource sends its id back.
    run(&svc, "CALL drevo.events.post('c', 3)");
    let mut s = Stream::open(&app, "/events", Some("0:1")).await;
    let resumed = s.take(3).await;
    assert_eq!(
        resumed.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["1:1", "1:2", "1:3"]
    );
    assert_eq!(resumed[0].name, "node");
    s.quiet().await;

    // From a bare cursor (as in an event's data): what came after that
    // change, named events included.
    let mut s = Stream::open(&app, "/events?since=1", None).await;
    let from_cursor = s.take(2).await;
    assert_eq!(
        from_cursor
            .iter()
            .map(|e| e.data["name"].clone())
            .collect::<Vec<_>>(),
        [serde_json::json!("b"), serde_json::json!("c")]
    );
    s.quiet().await;
}

#[tokio::test]
async fn named_events_no_longer_kept_get_lagged() {
    let (svc, app) = server();
    svc.set_event_history(2);
    for i in 0..4 {
        run(&svc, &format!("CALL drevo.events.post('e{i}', {i})"));
    }
    let mut s = Stream::open(&app, "/events?since=0:1", None).await;
    let lagged = s.take(1).await;
    assert_eq!(lagged[0].name, "lagged");
    assert_eq!(lagged[0].id, "0:4");
    run(&svc, "CALL drevo.events.post('fresh', null)");
    let next = s.take(1).await;
    assert_eq!(next[0].data["name"], "fresh");
    assert_eq!(next[0].id, "0:5");

    // Kept: no lag.
    let mut s = Stream::open(&app, "/events?since=0:3", None).await;
    let kept = s.take(2).await;
    assert_eq!(kept[0].data["name"], "e3");
    assert_eq!(kept[1].data["name"], "fresh");
}

#[tokio::test]
async fn a_position_from_before_a_restart_gets_lagged() {
    // A fresh server never issued named event 3.
    let (_svc, app) = server();
    let mut s = Stream::open(&app, "/events?since=0:3", None).await;
    let lagged = s.take(1).await;
    assert_eq!(lagged[0].name, "lagged");
    assert_eq!(lagged[0].data["cursor"], 0);
}

#[tokio::test]
async fn a_malformed_position_is_refused() {
    let (_svc, app) = server();
    for bad in ["1:x", "x:1", "1:2:3", "-1"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/events?since={bad}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}
