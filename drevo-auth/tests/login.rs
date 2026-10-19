//! Logging in against the stored users: [`AuthManager::authenticate`] checks
//! credentials, the user's status and the failed-attempt lockout, and hands out
//! session tokens that expire.
#![cfg(feature = "password")]

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use drevo_auth::authn::Credentials;
use drevo_auth::catalog::MemoryStore;
use drevo_auth::users::{
    AlterUser, AuthManager, Clock, LockoutPolicy, LoginError, NewUser, Principal, SessionPolicy,
    UserStatus,
};

/// A clock the test moves by hand.
#[derive(Default)]
struct TestClock(AtomicI64);
impl TestClock {
    fn advance(&self, d: Duration) {
        self.0
            .fetch_add(i64::try_from(d.as_millis()).unwrap(), Ordering::SeqCst);
    }
}
impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn setup() -> (AuthManager, Arc<TestClock>) {
    let clock = Arc::new(TestClock::default());
    let m = AuthManager::open(Arc::new(MemoryStore::new()))
        .unwrap()
        .with_clock(clock.clone());
    m.create_user(NewUser::new("ann", "ann-password").change_required(false))
        .unwrap();
    m.create_user(NewUser::new("bob", "bob-password")).unwrap();
    (m, clock)
}

fn basic(user: &str, password: &str) -> Credentials {
    Credentials::Basic {
        username: user.into(),
        password: password.into(),
    }
}

// ── Passwords ────────────────────────────────────────────────────────

#[test]
fn the_right_password_logs_in() {
    let (m, _) = setup();
    assert_eq!(
        m.authenticate(&basic("ann", "ann-password")),
        Ok(Principal {
            name: "ann".into(),
            password_change_required: false
        })
    );
}

#[test]
fn a_user_who_must_change_the_password_logs_in_flagged() {
    let (m, _) = setup();
    let p = m.authenticate(&basic("bob", "bob-password")).unwrap();
    assert!(p.password_change_required);
}

#[test]
fn a_wrong_password_and_an_unknown_user_fail_alike() {
    let (m, _) = setup();
    assert_eq!(
        m.authenticate(&basic("ann", "wrong")),
        Err(LoginError::InvalidCredentials)
    );
    assert_eq!(
        m.authenticate(&basic("zed", "ann-password")),
        Err(LoginError::InvalidCredentials)
    );
}

#[test]
fn a_suspended_user_cannot_log_in() {
    let (m, _) = setup();
    m.alter_user("ann", AlterUser::default().status(UserStatus::Suspended))
        .unwrap();
    assert_eq!(
        m.authenticate(&basic("ann", "ann-password")),
        Err(LoginError::Suspended)
    );
    m.alter_user("ann", AlterUser::default().status(UserStatus::Active))
        .unwrap();
    assert!(m.authenticate(&basic("ann", "ann-password")).is_ok());
}

// ── Lockout ──────────────────────────────────────────────────────────

#[test]
fn three_failures_in_a_row_lock_the_user_for_a_while() {
    let (m, clock) = setup();
    for _ in 0..3 {
        assert_eq!(
            m.authenticate(&basic("ann", "wrong")),
            Err(LoginError::InvalidCredentials)
        );
    }
    // Locked: even the right password is refused, with the time left.
    assert_eq!(
        m.authenticate(&basic("ann", "ann-password")),
        Err(LoginError::Locked {
            retry_after: Duration::from_secs(5)
        })
    );
    clock.advance(Duration::from_secs(2));
    assert_eq!(
        m.authenticate(&basic("ann", "ann-password")),
        Err(LoginError::Locked {
            retry_after: Duration::from_secs(3)
        })
    );
    clock.advance(Duration::from_secs(3));
    assert!(m.authenticate(&basic("ann", "ann-password")).is_ok());
}

#[test]
fn a_success_resets_the_failure_count() {
    let (m, _) = setup();
    for _ in 0..2 {
        let _ = m.authenticate(&basic("ann", "wrong"));
    }
    assert!(m.authenticate(&basic("ann", "ann-password")).is_ok());
    for _ in 0..2 {
        let _ = m.authenticate(&basic("ann", "wrong"));
    }
    assert!(
        m.authenticate(&basic("ann", "ann-password")).is_ok(),
        "two failures after a success do not lock"
    );
}

#[test]
fn lockout_is_per_user() {
    let (m, _) = setup();
    for _ in 0..3 {
        let _ = m.authenticate(&basic("ann", "wrong"));
    }
    assert!(matches!(
        m.authenticate(&basic("ann", "ann-password")),
        Err(LoginError::Locked { .. })
    ));
    assert!(m.authenticate(&basic("bob", "bob-password")).is_ok());
}

#[test]
fn unknown_names_do_not_grow_memory() {
    // Failures for names that are not users must not be tracked: a client
    // could otherwise fill memory with made-up names.
    let (m, _) = setup();
    for i in 0..1000 {
        let _ = m.authenticate(&basic(&format!("ghost{i}"), "x"));
    }
    assert_eq!(m.tracked_failures(), 0);
}

#[test]
fn the_lockout_policy_is_configurable() {
    let (m, clock) = setup();
    let m = m.with_lockout(LockoutPolicy {
        max_failed_attempts: 5,
        lock_for: Duration::from_secs(60),
    });
    for _ in 0..4 {
        let _ = m.authenticate(&basic("ann", "wrong"));
    }
    assert!(
        m.authenticate(&basic("ann", "ann-password")).is_ok(),
        "4 < 5"
    );
    for _ in 0..5 {
        let _ = m.authenticate(&basic("ann", "wrong"));
    }
    clock.advance(Duration::from_secs(59));
    assert!(matches!(
        m.authenticate(&basic("ann", "ann-password")),
        Err(LoginError::Locked { .. })
    ));
}

#[test]
fn a_zero_attempt_policy_never_locks() {
    let (m, _) = setup();
    let m = m.with_lockout(LockoutPolicy {
        max_failed_attempts: 0,
        lock_for: Duration::from_secs(5),
    });
    for _ in 0..20 {
        let _ = m.authenticate(&basic("ann", "wrong"));
    }
    assert!(m.authenticate(&basic("ann", "ann-password")).is_ok());
}

// ── Sessions ─────────────────────────────────────────────────────────

#[test]
fn a_session_token_logs_in_as_its_user() {
    let (m, _) = setup();
    let token = m.issue_session("ann").expect("token for an active user");
    assert_eq!(token.len(), 64);
    assert_eq!(
        m.authenticate(&Credentials::Token(token)),
        Ok(Principal {
            name: "ann".into(),
            password_change_required: false
        })
    );
}

#[test]
fn no_session_for_an_unknown_or_suspended_user() {
    let (m, _) = setup();
    assert_eq!(m.issue_session("zed"), None);
    m.alter_user("ann", AlterUser::default().status(UserStatus::Suspended))
        .unwrap();
    assert_eq!(m.issue_session("ann"), None);
}

#[test]
fn an_unknown_or_revoked_token_is_refused() {
    let (m, _) = setup();
    assert_eq!(
        m.authenticate(&Credentials::Token("0".repeat(64))),
        Err(LoginError::InvalidToken)
    );
    let token = m.issue_session("ann").unwrap();
    assert!(m.revoke_session(&token));
    assert_eq!(
        m.authenticate(&Credentials::Token(token.clone())),
        Err(LoginError::InvalidToken)
    );
    assert!(!m.revoke_session(&token));
}

#[test]
fn a_session_expires_after_idling_and_after_its_lifetime() {
    let (m, clock) = setup();
    let m = m.with_sessions(SessionPolicy {
        idle: Duration::from_secs(60),
        lifetime: Duration::from_secs(300),
    });
    let token = m.issue_session("ann").unwrap();
    let creds = Credentials::Token(token);
    // Used every 50 s it stays alive past the idle limit...
    for _ in 0..5 {
        clock.advance(Duration::from_secs(50));
        assert!(m.authenticate(&creds).is_ok());
    }
    // ...but not past its lifetime (250 s used, 300 s total).
    clock.advance(Duration::from_secs(51));
    assert_eq!(m.authenticate(&creds), Err(LoginError::InvalidToken));

    let idle = Credentials::Token(m.issue_session("ann").unwrap());
    clock.advance(Duration::from_secs(61));
    assert_eq!(m.authenticate(&idle), Err(LoginError::InvalidToken));
}

#[test]
fn sessions_end_when_the_user_is_dropped_suspended_or_changes_password() {
    let (m, _) = setup();
    let t1 = Credentials::Token(m.issue_session("ann").unwrap());
    m.alter_user("ann", AlterUser::default().status(UserStatus::Suspended))
        .unwrap();
    assert!(m.authenticate(&t1).is_err());
    m.alter_user("ann", AlterUser::default().status(UserStatus::Active))
        .unwrap();
    assert!(
        m.authenticate(&t1).is_err(),
        "reactivating does not revive old sessions"
    );

    let t2 = Credentials::Token(m.issue_session("ann").unwrap());
    m.change_own_password("ann", "ann-password", "new-ann-password")
        .unwrap();
    assert!(m.authenticate(&t2).is_err());

    let t3 = Credentials::Token(m.issue_session("ann").unwrap());
    m.drop_user("ann", false).unwrap();
    assert!(m.authenticate(&t3).is_err());
}

#[test]
fn ending_all_sessions_of_a_user_leaves_others() {
    let (m, _) = setup();
    let ann = Credentials::Token(m.issue_session("ann").unwrap());
    let bob = Credentials::Token(m.issue_session("bob").unwrap());
    assert_eq!(m.revoke_sessions_of("ann"), 1);
    assert!(m.authenticate(&ann).is_err());
    assert!(m.authenticate(&bob).is_ok());
}

#[test]
fn errors_say_what_happened_without_secrets() {
    assert_eq!(
        LoginError::InvalidCredentials.to_string(),
        "invalid principal or credentials"
    );
    assert_eq!(
        LoginError::InvalidToken.to_string(),
        "invalid or expired token"
    );
    assert_eq!(LoginError::Suspended.to_string(), "the user is suspended");
    assert_eq!(
        LoginError::Locked {
            retry_after: Duration::from_millis(2500)
        }
        .to_string(),
        "too many failed logins; try again in 3 s"
    );
}

#[test]
fn live_sessions_are_listed_by_number_and_end_by_number() {
    let (m, clock) = setup();
    let m = m.with_sessions(SessionPolicy {
        idle: Duration::from_secs(60),
        lifetime: Duration::from_secs(300),
    });
    let first = m.issue_session("ann").unwrap();
    clock.advance(Duration::from_secs(10));
    let second = m.issue_session("ann").unwrap();

    let listed = m.sessions();
    assert_eq!(listed.len(), 2);
    assert!(listed[0].number < listed[1].number);
    assert!(listed.iter().all(|s| s.user == "ann"));
    assert_eq!(listed[1].created_ms - listed[0].created_ms, 10_000);
    let shown = format!("{listed:?}");
    assert!(
        !shown.contains(&first) && !shown.contains(&second),
        "tokens stay hidden"
    );

    // Ending one by number logs it out; the other still works.
    assert_eq!(
        m.revoke_session_number(listed[0].number),
        Some("ann".into())
    );
    assert_eq!(m.revoke_session_number(listed[0].number), None);
    assert_eq!(
        m.authenticate(&Credentials::Token(first)),
        Err(LoginError::InvalidToken)
    );
    assert!(m.authenticate(&Credentials::Token(second)).is_ok());

    // An expired session is not listed.
    clock.advance(Duration::from_secs(61));
    assert!(m.sessions().is_empty());
}
