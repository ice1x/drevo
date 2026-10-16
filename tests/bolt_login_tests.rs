//! Logging in over Bolt against the server's users: `HELLO` must carry valid
//! credentials (a password or a session token) when the server requires a
//! login, a user who must change the password can only change it, and the
//! signed-in user is the current user for `SHOW CURRENT USER` and
//! `ALTER CURRENT USER`.
#![cfg(all(feature = "http", feature = "auth"))]

use std::collections::BTreeMap;
use std::sync::Arc;

use drevo::bolt::packstream::Value;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session, State};
use drevo::native_service::NativeService;
use drevo::security::{open_server_security, Security};
use drevo_auth::catalog::MemoryStore;
use drevo_auth::users::{AlterUser, AuthManager, Bootstrap, NewUser, UserStatus};

const ANN_PW: &str = "ann-password";
const BOB_PW: &str = "bob-password";

/// A login-requiring server with `ann` (ready to use) and `bob` (must change
/// his password).
fn security() -> (Security, Arc<AuthManager>) {
    let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
    users
        .create_user(NewUser::new("ann", ANN_PW).change_required(false))
        .unwrap();
    users.create_user(NewUser::new("bob", BOB_PW)).unwrap();
    (Security::new(Arc::clone(&users)), users)
}

fn dict<I: IntoIterator<Item = (&'static str, Value)>>(entries: I) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

fn session(security: Security) -> Session<'static> {
    Session::new_durable(Arc::new(NativeService::in_memory())).with_security(security)
}

fn hello(s: &mut Session<'_>, extra: BTreeMap<String, Value>) -> Vec<ServerMessage> {
    s.handle(ClientMessage::Hello { extra })
}

fn basic(user: &str, password: &str) -> BTreeMap<String, Value> {
    dict([
        ("scheme", Value::String("basic".into())),
        ("principal", Value::String(user.into())),
        ("credentials", Value::String(password.into())),
    ])
}

fn run(s: &mut Session<'_>, query: &str) -> Vec<ServerMessage> {
    let mut replies = s.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    if matches!(replies.first(), Some(ServerMessage::Success { .. })) {
        replies.extend(s.handle(ClientMessage::Pull {
            extra: dict([("n", Value::Integer(-1))]),
        }));
    }
    replies
}

fn failure(replies: &[ServerMessage]) -> Option<(String, String)> {
    replies.iter().find_map(|m| match m {
        ServerMessage::Failure { metadata } => {
            match (metadata.get("code"), metadata.get("message")) {
                (Some(Value::String(code)), Some(Value::String(message))) => {
                    Some((code.clone(), message.clone()))
                }
                _ => None,
            }
        }
        _ => None,
    })
}

fn records(replies: &[ServerMessage]) -> Vec<Vec<Value>> {
    replies
        .iter()
        .filter_map(|m| match m {
            ServerMessage::Record { fields } => Some(fields.clone()),
            _ => None,
        })
        .collect()
}

const UNAUTHORIZED: &str = "Neo.ClientError.Security.Unauthorized";

// ── HELLO ────────────────────────────────────────────────────────────

#[test]
fn the_right_password_opens_the_session() {
    let (security, _) = security();
    let mut s = session(security);
    let replies = hello(&mut s, basic("ann", ANN_PW));
    assert!(failure(&replies).is_none(), "{replies:?}");
    assert_eq!(s.state(), State::Ready);
    assert!(failure(&run(&mut s, "RETURN 1 AS one")).is_none());
}

#[test]
fn missing_or_wrong_credentials_close_the_session() {
    let (security, _) = security();
    for extra in [
        dict([]),
        dict([("scheme", Value::String("none".into()))]),
        basic("ann", "wrong-password"),
        basic("zed", ANN_PW),
    ] {
        let mut s = session(security.clone());
        let replies = hello(&mut s, extra.clone());
        let (code, _) = failure(&replies).unwrap_or_else(|| panic!("{extra:?} must fail"));
        assert_eq!(code, UNAUTHORIZED);
        assert_eq!(s.state(), State::Defunct);
    }
}

#[test]
fn a_suspended_user_is_refused_like_a_wrong_password() {
    let (security, users) = security();
    users
        .alter_user("ann", AlterUser::default().status(UserStatus::Suspended))
        .unwrap();
    let mut s = session(security.clone());
    let suspended = failure(&hello(&mut s, basic("ann", ANN_PW))).unwrap();
    let mut s = session(security);
    let wrong = failure(&hello(&mut s, basic("bob", "wrong-password"))).unwrap();
    assert_eq!(
        suspended, wrong,
        "a client cannot tell a suspended user from a bad password"
    );
}

#[test]
fn repeated_failures_rate_limit_the_user() {
    let (security, _) = security();
    for _ in 0..3 {
        let mut s = session(security.clone());
        hello(&mut s, basic("ann", "wrong-password"));
    }
    let mut s = session(security);
    let (code, message) = failure(&hello(&mut s, basic("ann", ANN_PW))).unwrap();
    assert_eq!(code, "Neo.ClientError.Security.AuthenticationRateLimit");
    assert!(message.contains("try again"), "{message}");
}

#[test]
fn a_session_token_opens_the_session() {
    let (security, users) = security();
    let token = users.issue_session("ann").unwrap();
    let mut s = session(security);
    let replies = hello(
        &mut s,
        dict([
            ("scheme", Value::String("bearer".into())),
            ("credentials", Value::String(token)),
        ]),
    );
    assert!(failure(&replies).is_none(), "{replies:?}");
    assert_eq!(s.state(), State::Ready);
}

#[test]
fn a_failed_login_never_echoes_the_password() {
    let (security, _) = security();
    let mut s = session(security);
    let replies = hello(&mut s, basic("ann", "hunter2-guess"));
    assert!(!format!("{replies:?}").contains("hunter2"));
}

// ── The signed-in user ───────────────────────────────────────────────

#[test]
fn show_current_user_shows_the_signed_in_user() {
    let (security, _) = security();
    let mut s = session(security);
    hello(&mut s, basic("ann", ANN_PW));
    let rows = records(&run(&mut s, "SHOW CURRENT USER"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("ann".into()));
}

#[test]
fn a_user_who_must_change_the_password_can_only_change_it() {
    let (security, users) = security();
    let mut s = session(security);
    let replies = hello(&mut s, basic("bob", BOB_PW));
    assert!(failure(&replies).is_none(), "the login itself succeeds");

    let (code, _) = failure(&run(&mut s, "RETURN 1")).expect("blocked until changed");
    assert_eq!(code, "Neo.ClientError.Security.CredentialsExpired");
    s.handle(ClientMessage::Reset);

    let replies = run(
        &mut s,
        &format!("ALTER CURRENT USER SET PASSWORD FROM '{BOB_PW}' TO 'bob-new-password'"),
    );
    assert!(failure(&replies).is_none(), "{replies:?}");
    assert!(users.check_password("bob", "bob-new-password"));
    assert!(
        failure(&run(&mut s, "RETURN 1")).is_none(),
        "unblocked after the change"
    );
}

#[test]
fn alter_current_user_changes_the_signed_in_users_password() {
    let (security, users) = security();
    let mut s = session(security);
    hello(&mut s, basic("ann", ANN_PW));
    let replies = run(
        &mut s,
        &format!("ALTER CURRENT USER SET PASSWORD FROM '{ANN_PW}' TO 'ann-new-password'"),
    );
    assert!(failure(&replies).is_none(), "{replies:?}");
    assert!(users.check_password("ann", "ann-new-password"));
}

// ── When no login is required ────────────────────────────────────────

#[test]
fn without_a_login_requirement_any_hello_is_accepted() {
    let (security, _) = security();
    let mut s = session(security.without_login());
    assert!(failure(&hello(&mut s, dict([]))).is_none());
    assert_eq!(s.state(), State::Ready);
}

#[test]
fn a_server_without_user_management_accepts_any_hello() {
    let mut s = session(Security::default());
    assert!(failure(&hello(&mut s, dict([]))).is_none());
    assert_eq!(s.state(), State::Ready);
}

#[test]
fn drevo_auth_none_turns_the_login_off() {
    let (security, _) = open_server_security(None, &Bootstrap::Disabled).unwrap();
    assert!(!security.requires_login());
    let (security, _) = open_server_security(None, &Bootstrap::Generated).unwrap();
    assert!(security.requires_login());
}
