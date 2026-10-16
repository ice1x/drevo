//! Bolt authentication — Phase 11 task `00074`.
//!
//! The Bolt handshake (task `00070`) and session loop (`00071`) accept
//! any connection. Official Neo4j drivers, however, always send an
//! authentication tuple inside the `HELLO` message:
//!
//! ```text
//! HELLO { user_agent: "neo4j-python/5.x",
//!         scheme:      "basic",
//!         principal:   "neo4j",
//!         credentials: "secret" }
//! ```
//!
//! (Bolt 4.x carries auth in `HELLO`; the `LOGON` / `LOGOFF` split only
//! arrives with Bolt 5.1, which drevo does not negotiate.)
//!
//! This module supplies:
//!
//! * [`Authenticator`](crate::bolt::auth::Authenticator) — a
//!   dependency-free trait the session layer calls with the decoded
//!   `HELLO` extras. Always compiled, so the session state machine can
//!   hold an `Option<&dyn Authenticator>` without pulling in any crypto
//!   dependency.
//! * [`AuthOutcome`](crate::bolt::auth::AuthOutcome) — the result the
//!   trait returns.
//! * [`UserStore`](crate::bolt::auth::UserStore) (feature `auth`) — the
//!   user table from the `drevo-auth` crate. Passwords are stored as
//!   Argon2id PHC strings (salted, never plaintext or an unsalted
//!   digest), and it issues and verifies opaque bearer **session
//!   tokens** so a driver can re-authenticate without resending the
//!   password. This module adapts it to the `HELLO` extras.
//!
//! ## Why the split
//!
//! The trait + outcome are free of crypto deps so default builds (no
//! `auth` feature) still compile the session integration and can
//! plug in their own authenticator (LDAP, a callback, a test double).
//! Only [`UserStore`](crate::bolt::auth::UserStore) — which pulls in `argon2` — sits behind the
//! feature flag.

use std::collections::BTreeMap;

use crate::bolt::packstream::Value;

/// Outcome of an authentication attempt against a [`HELLO`] extras map.
///
/// [`HELLO`]: crate::bolt::session::HELLO
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthOutcome {
    /// The credentials are valid; the session may transition to
    /// [`State::Ready`](crate::bolt::session::State::Ready).
    Authenticated,
    /// The credentials are missing, malformed, or wrong. The carried
    /// string is a human-readable reason surfaced to the driver as the
    /// `message` of a `Neo.ClientError.Security.Unauthorized` failure.
    Denied(String),
}

/// Anything that can authenticate a Bolt `HELLO`.
///
/// The session layer holds an `Option<&dyn Authenticator>`: `None`
/// means "accept any connection" (the pre-`00074` behaviour, kept so
/// loopback / embedded use needs no credentials), `Some` means every
/// `HELLO` is checked before the session reaches
/// [`State::Ready`](crate::bolt::session::State::Ready).
///
/// Implementors must be `Send + Sync` because a single authenticator is
/// shared across every concurrent connection on a listener.
pub trait Authenticator: Send + Sync {
    /// Validate the decoded `HELLO` extras (which carry `scheme`,
    /// `principal`, `credentials`).
    fn authenticate(&self, extra: &BTreeMap<String, Value>) -> AuthOutcome;
}

/// Pull a string-valued field out of a `HELLO` extras map, returning
/// `None` for an absent or non-string field. Shared by the concrete
/// [`UserStore`] and exposed so external [`Authenticator`] impls decode
/// the standard tuple the same way.
pub fn string_field<'a>(extra: &'a BTreeMap<String, Value>, key: &str) -> Option<&'a str> {
    match extra.get(key) {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

// ===========================================================================
// UserStore — the Argon2id store from `drevo-auth` (feature `auth`).
// ===========================================================================

/// Users with Argon2id password hashes plus session tokens, from the
/// `drevo-auth` crate, and its error type. Usable directly as a Bolt
/// [`Authenticator`].
#[cfg(feature = "auth")]
pub use drevo_auth::authn::{AuthError, UserStore};

#[cfg(feature = "auth")]
impl Authenticator for UserStore {
    fn authenticate(&self, extra: &BTreeMap<String, Value>) -> AuthOutcome {
        use drevo_auth::authn::Credentials;
        let credentials = Credentials::from_parts(
            string_field(extra, "scheme"),
            string_field(extra, "principal"),
            string_field(extra, "credentials"),
        );
        match credentials.and_then(|c| UserStore::authenticate(self, &c)) {
            Ok(_) => AuthOutcome::Authenticated,
            Err(failure) => AuthOutcome::Denied(failure.to_string()),
        }
    }
}

#[cfg(test)]
#[cfg(feature = "auth")]
mod tests {
    use super::*;

    fn basic_extra(principal: &str, credentials: &str) -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("scheme".to_string(), Value::String("basic".to_string())),
            (
                "principal".to_string(),
                Value::String(principal.to_string()),
            ),
            (
                "credentials".to_string(),
                Value::String(credentials.to_string()),
            ),
        ])
    }

    #[test]
    fn authenticate_basic_round_trips() {
        let mut store = UserStore::new();
        store.add_user("neo4j", "s3cret").unwrap();
        assert_eq!(
            Authenticator::authenticate(&store, &basic_extra("neo4j", "s3cret")),
            AuthOutcome::Authenticated
        );
        assert!(matches!(
            Authenticator::authenticate(&store, &basic_extra("neo4j", "nope")),
            AuthOutcome::Denied(_)
        ));
    }

    #[test]
    fn authenticate_missing_fields_denied() {
        let store = UserStore::new();
        let no_principal =
            BTreeMap::from([("credentials".to_string(), Value::String("x".to_string()))]);
        assert!(matches!(
            Authenticator::authenticate(&store, &no_principal),
            AuthOutcome::Denied(_)
        ));
    }

    #[test]
    fn bearer_token_round_trips() {
        let mut store = UserStore::new();
        store.add_user("neo4j", "s3cret").unwrap();
        let token = store.issue_token("neo4j").expect("token for known user");
        let extra = BTreeMap::from([
            ("scheme".to_string(), Value::String("bearer".to_string())),
            ("credentials".to_string(), Value::String(token.clone())),
        ]);
        assert_eq!(
            Authenticator::authenticate(&store, &extra),
            AuthOutcome::Authenticated
        );
        assert_eq!(store.verify_token(&token).as_deref(), Some("neo4j"));
    }

    #[test]
    fn revoked_token_no_longer_authenticates() {
        let mut store = UserStore::new();
        store.add_user("neo4j", "s3cret").unwrap();
        let token = store.issue_token("neo4j").unwrap();
        assert!(store.revoke_token(&token));
        assert!(store.verify_token(&token).is_none());
        let extra = BTreeMap::from([
            ("scheme".to_string(), Value::String("bearer".to_string())),
            ("credentials".to_string(), Value::String(token)),
        ]);
        assert!(matches!(
            Authenticator::authenticate(&store, &extra),
            AuthOutcome::Denied(_)
        ));
    }

    #[test]
    fn denial_reasons_reach_the_driver_unchanged() {
        let mut store = UserStore::new();
        store.add_user("neo4j", "s3cret").unwrap();
        let cases = [
            (
                BTreeMap::from([("credentials".to_string(), Value::String("x".to_string()))]),
                "missing principal",
            ),
            (
                BTreeMap::from([("principal".to_string(), Value::String("neo4j".to_string()))]),
                "missing credentials",
            ),
            (
                BTreeMap::from([("scheme".to_string(), Value::String("bearer".to_string()))]),
                "missing bearer token",
            ),
            (
                basic_extra("neo4j", "wrong"),
                "invalid principal or credentials",
            ),
            (
                basic_extra("ghost", "s3cret"),
                "invalid principal or credentials",
            ),
        ];
        for (extra, reason) in cases {
            assert_eq!(
                Authenticator::authenticate(&store, &extra),
                AuthOutcome::Denied(reason.to_string())
            );
        }
    }

    #[test]
    fn a_non_string_field_counts_as_missing() {
        let mut store = UserStore::new();
        store.add_user("neo4j", "s3cret").unwrap();
        let extra = BTreeMap::from([
            ("principal".to_string(), Value::Integer(7)),
            (
                "credentials".to_string(),
                Value::String("s3cret".to_string()),
            ),
        ]);
        assert_eq!(
            Authenticator::authenticate(&store, &extra),
            AuthOutcome::Denied("missing principal".to_string())
        );
    }

    #[test]
    fn unsupported_scheme_denied() {
        let store = UserStore::new();
        let extra = BTreeMap::from([("scheme".to_string(), Value::String("kerberos".to_string()))]);
        assert!(matches!(
            Authenticator::authenticate(&store, &extra),
            AuthOutcome::Denied(msg) if msg.contains("kerberos")
        ));
    }
}
