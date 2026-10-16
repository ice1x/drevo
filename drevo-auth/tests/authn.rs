//! Authentication: the credentials a client presents, checked against a
//! [`UserStore`] of Argon2id-hashed passwords and opaque session tokens.
#![cfg(feature = "password")]

use std::collections::HashSet;

use drevo_auth::authn::{AuthError, AuthFailure, Credentials, UserStore};

fn store_with(users: &[(&str, &str)]) -> UserStore {
    let mut store = UserStore::new();
    for (name, password) in users {
        store.add_user(name, password).expect("add user");
    }
    store
}

// ── Credentials::from_parts ──────────────────────────────────────────

#[test]
fn basic_scheme_carries_username_and_password() {
    let creds = Credentials::from_parts(Some("basic"), Some("ann"), Some("s3cret")).unwrap();
    assert_eq!(
        creds,
        Credentials::Basic {
            username: "ann".into(),
            password: "s3cret".into()
        }
    );
}

#[test]
fn a_missing_scheme_means_basic() {
    let creds = Credentials::from_parts(None, Some("ann"), Some("s3cret")).unwrap();
    assert!(matches!(creds, Credentials::Basic { .. }));
}

#[test]
fn bearer_scheme_carries_the_token() {
    let creds = Credentials::from_parts(Some("bearer"), None, Some("abc")).unwrap();
    assert_eq!(creds, Credentials::Token("abc".into()));
}

#[test]
fn incomplete_or_unknown_credentials_are_refused_with_a_reason() {
    let cases = [
        (
            Some("basic"),
            None,
            Some("p"),
            AuthFailure::MissingPrincipal,
        ),
        (
            Some("basic"),
            Some("ann"),
            None,
            AuthFailure::MissingCredentials,
        ),
        (Some("bearer"), None, None, AuthFailure::MissingToken),
        (
            Some("kerberos"),
            Some("ann"),
            Some("ticket"),
            AuthFailure::UnsupportedScheme("kerberos".into()),
        ),
    ];
    for (scheme, principal, credentials, expected) in cases {
        assert_eq!(
            Credentials::from_parts(scheme, principal, credentials),
            Err(expected)
        );
    }
}

#[test]
fn failure_messages_name_the_problem_without_leaking_secrets() {
    assert_eq!(
        AuthFailure::MissingPrincipal.to_string(),
        "missing principal"
    );
    assert_eq!(
        AuthFailure::MissingCredentials.to_string(),
        "missing credentials"
    );
    assert_eq!(
        AuthFailure::MissingToken.to_string(),
        "missing bearer token"
    );
    assert_eq!(
        AuthFailure::UnsupportedScheme("kerberos".into()).to_string(),
        "unsupported auth scheme: kerberos"
    );
    // The same message for an unknown user and a wrong password, so a client
    // cannot tell which names exist.
    assert_eq!(
        AuthFailure::InvalidCredentials.to_string(),
        "invalid principal or credentials"
    );
    assert_eq!(
        AuthFailure::InvalidToken.to_string(),
        "invalid or expired token"
    );
}

// ── UserStore::authenticate ──────────────────────────────────────────

#[test]
fn the_right_password_authenticates_as_that_user() {
    let store = store_with(&[("ann", "s3cret"), ("bob", "hunter2")]);
    let creds = Credentials::Basic {
        username: "bob".into(),
        password: "hunter2".into(),
    };
    assert_eq!(store.authenticate(&creds), Ok("bob".to_string()));
}

#[test]
fn a_wrong_password_and_an_unknown_user_fail_the_same_way() {
    let store = store_with(&[("ann", "s3cret")]);
    let wrong = Credentials::Basic {
        username: "ann".into(),
        password: "nope".into(),
    };
    let unknown = Credentials::Basic {
        username: "zed".into(),
        password: "s3cret".into(),
    };
    assert_eq!(
        store.authenticate(&wrong),
        Err(AuthFailure::InvalidCredentials)
    );
    assert_eq!(
        store.authenticate(&unknown),
        Err(AuthFailure::InvalidCredentials)
    );
}

#[test]
fn passwords_are_case_sensitive_and_compared_whole() {
    let store = store_with(&[("ann", "s3cret")]);
    for attempt in ["S3cret", "s3cre", "s3cret ", ""] {
        let creds = Credentials::Basic {
            username: "ann".into(),
            password: attempt.into(),
        };
        assert!(
            store.authenticate(&creds).is_err(),
            "{attempt:?} must not log in"
        );
    }
}

#[test]
fn an_issued_token_authenticates_until_revoked() {
    let store = store_with(&[("ann", "s3cret")]);
    let token = store.issue_token("ann").expect("token for a known user");
    let creds = Credentials::Token(token.clone());
    assert_eq!(store.authenticate(&creds), Ok("ann".to_string()));

    assert!(store.revoke_token(&token));
    assert_eq!(store.authenticate(&creds), Err(AuthFailure::InvalidToken));
    assert!(!store.revoke_token(&token), "a revoked token is gone");
}

#[test]
fn a_token_that_was_never_issued_is_refused() {
    let store = store_with(&[("ann", "s3cret")]);
    let creds = Credentials::Token("0".repeat(64));
    assert_eq!(store.authenticate(&creds), Err(AuthFailure::InvalidToken));
}

#[test]
fn no_token_is_issued_for_an_unknown_user() {
    let store = store_with(&[("ann", "s3cret")]);
    assert_eq!(store.issue_token("zed"), None);
}

#[test]
fn tokens_are_256_random_bits_in_hex_and_never_repeat() {
    let store = store_with(&[("ann", "s3cret")]);
    let tokens: Vec<String> = (0..200)
        .map(|_| store.issue_token("ann").unwrap())
        .collect();
    for t in &tokens {
        assert_eq!(t.len(), 64, "32 bytes as hex");
        assert!(t
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
    let distinct: HashSet<&String> = tokens.iter().collect();
    assert_eq!(distinct.len(), tokens.len());
    // No shared prefix: a time-ordered id would make consecutive tokens share
    // their leading characters.
    let prefixes: HashSet<&str> = tokens.iter().map(|t| &t[..8]).collect();
    assert!(
        prefixes.len() > 190,
        "token prefixes repeat: {}",
        prefixes.len()
    );
}

#[test]
fn a_token_resolves_to_the_user_it_was_issued_for() {
    let store = store_with(&[("ann", "a"), ("bob", "b")]);
    let ann = store.issue_token("ann").unwrap();
    let bob = store.issue_token("bob").unwrap();
    assert_eq!(store.verify_token(&ann).as_deref(), Some("ann"));
    assert_eq!(store.verify_token(&bob).as_deref(), Some("bob"));
}

// ── Provisioning ─────────────────────────────────────────────────────

#[test]
fn adding_a_user_twice_is_an_error() {
    let mut store = store_with(&[("ann", "s3cret")]);
    assert!(matches!(
        store.add_user("ann", "other"),
        Err(AuthError::DuplicateUser(name)) if name == "ann"
    ));
    // The first password still works.
    assert!(store.verify_basic("ann", "s3cret"));
}

#[test]
fn two_users_with_one_password_get_different_hashes() {
    let store = store_with(&[("ann", "same"), ("bob", "same")]);
    let (a, b) = (
        store.password_hash("ann").unwrap(),
        store.password_hash("bob").unwrap(),
    );
    assert!(
        a.starts_with("$argon2id$"),
        "stored as an Argon2id PHC string: {a}"
    );
    assert_ne!(a, b, "each user has a random salt");
    assert!(!a.contains("same"), "the password is never stored");
}

#[test]
fn the_store_is_shared_safely_across_threads() {
    let store = std::sync::Arc::new(store_with(&[("ann", "s3cret")]));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let store = std::sync::Arc::clone(&store);
            std::thread::spawn(move || {
                let token = store.issue_token("ann").unwrap();
                assert_eq!(store.verify_token(&token).as_deref(), Some("ann"));
                token
            })
        })
        .collect();
    let tokens: HashSet<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(tokens.len(), 8);
}
