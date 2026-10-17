//! `GET /events`: a Server-Sent Events stream of committed changes.
//!
//! - Each node or relationship written or deleted is one event, in commit
//!   order, with its position in the change feed as the SSE `id`.
//! - A client resumes from `?since=<cursor>` or the `Last-Event-ID` header.
//! - Rolled-back work sends nothing.
//! - A cursor the server no longer has gets a `lagged` event.
#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, Session};
use drevo::cypher::parser::parse;
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// One received SSE event.
#[derive(Debug, Clone)]
struct Event {
    id: Option<u64>,
    name: String,
    data: serde_json::Value,
}

/// An open `/events` stream.
struct Stream {
    body: Body,
    buffer: String,
}

impl Stream {
    async fn open(app: &axum::Router, uri: &str, last_event_id: Option<u64>) -> Self {
        let mut req = Request::builder().uri(uri);
        if let Some(id) = last_event_id {
            req = req.header("last-event-id", id.to_string());
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(ct.starts_with("text/event-stream"), "{ct}");
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
                    id: None,
                    name: "message".into(),
                    data: serde_json::Value::Null,
                };
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("id:") {
                        event.id = v.trim().parse().ok();
                    } else if let Some(v) = line.strip_prefix("event:") {
                        event.name = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim());
                    }
                }
                if data.is_empty() && event.id.is_none() {
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

fn server() -> (Arc<NativeService>, axum::Router) {
    let svc = Arc::new(NativeService::in_memory());
    let app = build_native_router(NativeApiState::new(Arc::clone(&svc)));
    (svc, app)
}

#[tokio::test]
async fn committed_changes_arrive_in_order_with_their_cursor() {
    let (svc, app) = server();
    run(
        &svc,
        "CREATE (a:Task {title: 'ship'})-[:DEPENDS_ON]->(b:Task:Urgent {title: 'build'})",
    );
    let mut s = Stream::open(&app, "/events?since=0", None).await;
    let events = s.take(3).await;
    let ids: Vec<u64> = events.iter().map(|e| e.id.unwrap()).collect();
    assert_eq!(ids, [1, 2, 3], "{events:?}");
    let nodes: Vec<&Event> = events.iter().filter(|e| e.name == "node").collect();
    assert_eq!(nodes.len(), 2, "{events:?}");
    for n in &nodes {
        assert_eq!(n.data["op"], "upsert");
        assert!(n.data["id"].is_u64());
        assert_eq!(n.data["uuid"].as_str().unwrap().len(), 36);
        assert_eq!(n.data["cursor"].as_u64(), n.id);
    }
    let labels: Vec<serde_json::Value> = nodes.iter().map(|n| n.data["labels"].clone()).collect();
    assert!(
        labels.contains(&serde_json::json!(["Task", "Urgent"])),
        "{labels:?}"
    );
    let rel = events.iter().find(|e| e.name == "relationship").unwrap();
    assert_eq!(rel.data["op"], "upsert");
    assert_eq!(rel.data["type"], "DEPENDS_ON");
    assert!(rel.data["from"].is_u64() && rel.data["to"].is_u64());
    assert!(
        rel.data.get("properties").is_none() && nodes[0].data.get("title").is_none(),
        "events name what changed, not its values"
    );
    s.quiet().await;
}

#[tokio::test]
async fn a_new_subscriber_gets_only_what_commits_after_it_connects() {
    let (svc, app) = server();
    run(&svc, "CREATE (:Task {title: 'old'})");
    let mut s = Stream::open(&app, "/events", None).await;
    s.quiet().await;
    run(&svc, "CREATE (:Task {title: 'new'})");
    let events = s.take(1).await;
    assert_eq!(events[0].name, "node");
    assert_eq!(events[0].id, Some(2));
}

#[tokio::test]
async fn a_client_resumes_after_a_reconnect_without_gaps() {
    let (svc, app) = server();
    let mut s = Stream::open(&app, "/events?since=0", None).await;
    run(&svc, "CREATE (:Bug {n: 1})");
    run(&svc, "CREATE (:Bug {n: 2})");
    let first = s.take(2).await;
    drop(s);
    // Changes made while disconnected.
    run(&svc, "CREATE (:Bug {n: 3})");
    run(&svc, "MATCH (b:Bug {n: 1}) DETACH DELETE b");
    // EventSource sends the last id it saw as Last-Event-ID.
    let mut s = Stream::open(&app, "/events", first[1].id).await;
    let resumed = s.take(2).await;
    assert_eq!(
        resumed.iter().map(|e| e.id.unwrap()).collect::<Vec<_>>(),
        [3, 4]
    );
    assert_eq!(resumed[1].data["op"], "delete");
    assert_eq!(resumed[1].data["id"], first[0].data["id"]);
    // `since` does the same.
    let mut s = Stream::open(
        &app,
        &format!("/events?since={}", first[0].id.unwrap()),
        None,
    )
    .await;
    assert_eq!(s.take(3).await[0].id, Some(2));
}

#[tokio::test]
async fn rolled_back_work_sends_nothing() {
    let (svc, app) = server();
    let mut s = Stream::open(&app, "/events", None).await;
    let dict = |e: Vec<(&str, Bolt)>| -> BTreeMap<String, Bolt> {
        e.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    };
    let mut session = Session::new_durable(Arc::clone(&svc));
    session.handle(ClientMessage::Hello {
        extra: dict(vec![]),
    });
    for commit in [false, true] {
        session.handle(ClientMessage::Begin {
            extra: dict(vec![]),
        });
        session.handle(ClientMessage::Run {
            query: format!("CREATE (:Invoice {{committed: {commit}}})"),
            parameters: dict(vec![]),
            extra: dict(vec![]),
        });
        session.handle(ClientMessage::Pull {
            extra: dict(vec![("n", Bolt::Integer(-1))]),
        });
        if commit {
            session.handle(ClientMessage::Commit);
        } else {
            session.handle(ClientMessage::Rollback);
            s.quiet().await;
        }
    }
    let events = s.take(1).await;
    assert_eq!(events[0].data["labels"], serde_json::json!(["Invoice"]));
    s.quiet().await;
}

#[tokio::test]
async fn labels_narrow_the_stream() {
    let (svc, app) = server();
    let mut s = Stream::open(&app, "/events?labels=Bug,BLOCKS", None).await;
    run(&svc, "CREATE (:Task)-[:BLOCKS]->(:Bug)-[:MENTIONS]->(:Doc)");
    let events = s.take(2).await;
    let names: Vec<String> = events
        .iter()
        .map(|e| {
            e.data["labels"]
                .get(0)
                .or(e.data.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    let mut names = names;
    names.sort();
    assert_eq!(names, ["BLOCKS", "Bug"], "{events:?}");
    s.quiet().await;
}

#[tokio::test]
async fn a_cursor_the_server_does_not_have_gets_lagged() {
    // A cursor past the head: the server restarted, or the cursor is not
    // ours. The client must reload, then follows from the head.
    let (svc, app) = server();
    run(&svc, "CREATE (:Task)");
    let mut s = Stream::open(&app, "/events?since=999", None).await;
    let lagged = s.take(1).await;
    assert_eq!(lagged[0].name, "lagged");
    assert_eq!(lagged[0].data["cursor"], 1);
    assert_eq!(lagged[0].id, Some(1));
    run(&svc, "CREATE (:Task)");
    assert_eq!(s.take(1).await[0].id, Some(2));
}

#[tokio::test]
async fn history_trimmed_away_gets_lagged() {
    // A durable store compacts every few writes and keeps only recent
    // history; an older cursor is told to reload.
    let dir = tempfile::tempdir().unwrap();
    let svc = Arc::new(
        NativeService::open_with_compact_threshold(dir.path().join("native.wal"), 4).unwrap(),
    );
    svc.set_event_history(3);
    for i in 0..12 {
        run(&svc, &format!("CREATE (:Task {{n: {i}}})"));
    }
    let app = build_native_router(NativeApiState::new(Arc::clone(&svc)));
    let mut s = Stream::open(&app, "/events?since=0", None).await;
    let first = s.take(1).await;
    assert_eq!(first[0].name, "lagged", "{first:?}");
    let resume_from = first[0].data["cursor"].as_u64().unwrap();
    assert!(
        resume_from >= 9,
        "the newest history is kept: {resume_from}"
    );
    s.quiet().await;
    // The recent history is still there for a client that kept up.
    let mut s = Stream::open(&app, "/events?since=11", None).await;
    assert_eq!(s.take(1).await[0].id, Some(12));
}

#[tokio::test]
async fn a_bad_cursor_is_refused() {
    let (_, app) = server();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/events?since=soon")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[cfg(feature = "auth")]
mod privileges {
    use super::*;
    use drevo::security::Security;
    use drevo_auth::catalog::MemoryStore;
    use drevo_auth::users::{AuthManager, NewUser};

    #[tokio::test]
    async fn a_subscriber_gets_only_what_it_may_traverse() {
        let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
        users
            .create_user(NewUser::new("rita", "long-password").change_required(false))
            .unwrap();
        users.grant_role("reader", "rita").unwrap();
        users.create_role("no_secrets", false, None).unwrap();
        let deny = drevo_auth::privileges::parse("DENY TRAVERSE ON GRAPH * NODES Secret TO x")
            .unwrap()
            .privileges;
        users
            .grant(drevo_auth::privileges::Access::Deny, &deny, "no_secrets")
            .unwrap();
        users.grant_role("no_secrets", "rita").unwrap();
        let svc = Arc::new(NativeService::in_memory());
        let app = build_native_router(
            NativeApiState::new(Arc::clone(&svc)).with_security(Security::new(Arc::clone(&users))),
        );
        // No login, no stream.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let token = users.issue_session("rita").unwrap();
        let req = Request::builder()
            .uri("/events")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let mut s = Stream {
            body: resp.into_body(),
            buffer: String::new(),
        };
        run(&svc, "CREATE (:Secret {code: 1})");
        run(&svc, "CREATE (:Task {title: 'open'})");
        let events = s.take(1).await;
        assert_eq!(events[0].data["labels"], serde_json::json!(["Task"]));
        s.quiet().await;
    }
}
