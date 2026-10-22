//! A query that uses a Cypher form the server does not support is the
//! client's error: Bolt answers `Neo.ClientError.Statement.UnsupportedOperationError`
//! (drivers do not retry client errors) with a message that names the form.

use std::collections::BTreeMap;
use std::sync::Arc;

use drevo::bolt::packstream::Value as BoltValue;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::native_service::NativeService;

const UNSUPPORTED: &str = "Neo.ClientError.Statement.UnsupportedOperationError";

fn session(svc: &Arc<NativeService>) -> Session<'static> {
    let mut s = Session::new_durable(Arc::clone(svc));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    s
}

fn run(s: &mut Session<'_>, query: &str) -> ServerMessage {
    let mut replies = s.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    assert_eq!(replies.len(), 1, "{replies:?}");
    replies.remove(0)
}

#[track_caller]
fn failure(reply: &ServerMessage) -> (String, String) {
    let ServerMessage::Failure { metadata } = reply else {
        panic!("expected FAILURE, got {reply:?}");
    };
    let text = |key: &str| match metadata.get(key) {
        Some(BoltValue::String(s)) => s.clone(),
        other => panic!("{key}: {other:?}"),
    };
    (text("code"), text("message"))
}

#[test]
fn an_aggregation_in_a_comprehension_body_is_a_client_error() {
    let svc = Arc::new(NativeService::in_memory());
    let mut s = session(&svc);
    let (code, message) = failure(&run(
        &mut s,
        "UNWIND [1, 2] AS bug RETURN [y IN [1, 2] | count(y)] AS counts",
    ));
    assert_eq!(code, UNSUPPORTED);
    assert!(message.contains("count"), "{message}");
    assert!(message.contains("not supported"), "{message}");
    assert!(!message.contains("task"), "no roadmap note: {message}");
    assert!(!message.contains("Phase"), "no roadmap note: {message}");
}

#[test]
fn an_unknown_function_is_a_client_error_and_the_session_carries_on() {
    let svc = Arc::new(NativeService::in_memory());
    let mut s = session(&svc);
    let (code, message) = failure(&run(
        &mut s,
        "MATCH (b:Bug) RETURN triage.score(b) AS score",
    ));
    assert_eq!(code, UNSUPPORTED);
    assert!(message.contains("triage.score"), "{message}");

    // A client error leaves the connection usable after RESET.
    assert!(matches!(
        s.handle(ClientMessage::Reset).as_slice(),
        [ServerMessage::Success { .. }]
    ));
    assert!(matches!(
        run(&mut s, "CREATE (:Bug {title: 'Crash on save'})"),
        ServerMessage::Success { .. }
    ));
}
