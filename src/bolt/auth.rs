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
//! * [`UserStore`](crate::bolt::auth::UserStore) (feature `bolt-auth`) — the concrete "user table".
//!   Credentials are stored as Argon2id PHC strings (salted, never
//!   plaintext or an unsalted digest), and it additionally issues and
//!   verifies opaque bearer **session tokens** so a driver can
//!   re-authenticate without resending the password.
//!
//! ## Why the split
//!
//! The trait + outcome are free of crypto deps so default builds (no
//! `bolt-auth` feature) still compile the session integration and can
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
// UserStore — concrete argon2-backed authenticator (feature `bolt-auth`).
// ===========================================================================

/// Errors raised while populating or driving a [`UserStore`].
#[cfg(feature = "bolt-auth")]
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Argon2 failed to hash the supplied password (out-of-memory in the
    /// KDF, or a malformed salt). Practically unreachable for normal
    /// inputs — surfaced rather than `unwrap`'d so a caller never panics
    /// while provisioning a user.
    #[error("password hashing failed: {0}")]
    Hash(String),
    /// A user with the same name already exists. Adding users is
    /// idempotent at the call site's discretion, not silently
    /// last-write-wins, so a typo'd duplicate is caught.
    #[error("user already exists: {0}")]
    DuplicateUser(String),
}

/// In-memory "user table": username → Argon2id PHC hash, plus a set of
/// live opaque bearer tokens.
///
/// # Security model
///
/// * Passwords are stored as Argon2id PHC strings (`$argon2id$...`) with
///   a per-user random salt, so two users with the same password get
///   different stored hashes and the table never holds a plaintext or
///   reversible secret.
/// * Session tokens are opaque 256-bit values (two UUIDv7s) handed out
///   by [`issue_token`](UserStore::issue_token) after a successful basic
///   auth. They live only in memory (a process restart invalidates every
///   token), matching the MVP single-process model — persistent tokens
///   wait for the user-table-on-disk work tracked alongside RBAC
///   (`00094`).
///
/// `UserStore` is `Send + Sync` (the token map is behind a `Mutex`), so
/// one instance is shared by reference across every connection on a
/// listener.
#[cfg(feature = "bolt-auth")]
pub struct UserStore {
    users: std::collections::HashMap<String, String>,
    tokens: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

#[cfg(feature = "bolt-auth")]
impl Default for UserStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "bolt-auth")]
impl UserStore {
    /// Create an empty store. With no users added, every basic-auth
    /// `HELLO` is denied — provision at least one user before exposing
    /// the listener.
    pub fn new() -> Self {
        Self {
            users: std::collections::HashMap::new(),
            tokens: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Add a user, hashing `password` with Argon2id before storage.
    ///
    /// # Errors
    ///
    /// * [`AuthError::DuplicateUser`] — `username` is already present.
    /// * [`AuthError::Hash`] — the KDF failed (effectively unreachable
    ///   for normal inputs).
    pub fn add_user(&mut self, username: &str, password: &str) -> Result<(), AuthError> {
        if self.users.contains_key(username) {
            return Err(AuthError::DuplicateUser(username.to_string()));
        }
        let hash = hash_password(password)?;
        self.users.insert(username.to_string(), hash);
        Ok(())
    }

    /// Verify a `username` / `password` pair against the stored hash.
    /// Returns `false` for an unknown user or a wrong password — the
    /// caller must not distinguish the two on the wire (user-enumeration
    /// guard).
    pub fn verify_basic(&self, username: &str, password: &str) -> bool {
        let Some(stored) = self.users.get(username) else {
            return false;
        };
        verify_password(password, stored)
    }

    /// Issue an opaque bearer **session token** for an existing user.
    /// Returns `None` if the user is unknown so a token is never minted
    /// for a non-existent principal.
    pub fn issue_token(&self, username: &str) -> Option<String> {
        if !self.users.contains_key(username) {
            return None;
        }
        let token = format!(
            "{}{}",
            uuid::Uuid::now_v7().as_simple(),
            uuid::Uuid::now_v7().as_simple()
        );
        self.tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(token.clone(), username.to_string());
        Some(token)
    }

    /// Resolve a bearer token back to its username, or `None` if the
    /// token was never issued (or the issuing process has restarted).
    pub fn verify_token(&self, token: &str) -> Option<String> {
        self.tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(token)
            .cloned()
    }

    /// Invalidate a previously issued token (logoff). Returns `true` if
    /// the token existed.
    pub fn revoke_token(&self, token: &str) -> bool {
        self.tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(token)
            .is_some()
    }
}

#[cfg(feature = "bolt-auth")]
impl Authenticator for UserStore {
    fn authenticate(&self, extra: &BTreeMap<String, Value>) -> AuthOutcome {
        // Neo4j drivers default the scheme to "basic"; treat an absent
        // scheme as basic so a hand-rolled client that only sends
        // principal/credentials still works.
        let scheme = string_field(extra, "scheme").unwrap_or("basic");
        match scheme {
            "basic" => {
                let Some(principal) = string_field(extra, "principal") else {
                    return AuthOutcome::Denied("missing principal".to_string());
                };
                let Some(credentials) = string_field(extra, "credentials") else {
                    return AuthOutcome::Denied("missing credentials".to_string());
                };
                if self.verify_basic(principal, credentials) {
                    AuthOutcome::Authenticated
                } else {
                    AuthOutcome::Denied("invalid principal or credentials".to_string())
                }
            }
            "bearer" => {
                let Some(token) = string_field(extra, "credentials") else {
                    return AuthOutcome::Denied("missing bearer token".to_string());
                };
                if self.verify_token(token).is_some() {
                    AuthOutcome::Authenticated
                } else {
                    AuthOutcome::Denied("invalid or expired token".to_string())
                }
            }
            other => AuthOutcome::Denied(format!("unsupported auth scheme: {other}")),
        }
    }
}

#[cfg(feature = "bolt-auth")]
fn hash_password(password: &str) -> Result<String, AuthError> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    use argon2::Argon2;

    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Hash(e.to_string()))
}

#[cfg(feature = "bolt-auth")]
fn verify_password(password: &str, stored_phc: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    use argon2::Argon2;

    let Ok(parsed) = PasswordHash::new(stored_phc) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
#[cfg(feature = "bolt-auth")]
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
    fn verify_basic_accepts_correct_password() {
        let mut store = UserStore::new();
        store.add_user("neo4j", "s3cret").unwrap();
        assert!(store.verify_basic("neo4j", "s3cret"));
    }

    #[test]
    fn verify_basic_rejects_wrong_password() {
        let mut store = UserStore::new();
        store.add_user("neo4j", "s3cret").unwrap();
        assert!(!store.verify_basic("neo4j", "wrong"));
    }

    #[test]
    fn verify_basic_rejects_unknown_user() {
        let store = UserStore::new();
        assert!(!store.verify_basic("ghost", "anything"));
    }

    #[test]
    fn add_user_rejects_duplicate() {
        let mut store = UserStore::new();
        store.add_user("neo4j", "a").unwrap();
        let err = store.add_user("neo4j", "b").unwrap_err();
        assert!(matches!(err, AuthError::DuplicateUser(u) if u == "neo4j"));
    }

    #[test]
    fn stored_hashes_are_salted_and_not_plaintext() {
        let mut store = UserStore::new();
        store.add_user("alice", "samepass").unwrap();
        store.add_user("bob", "samepass").unwrap();
        let a = store.users.get("alice").unwrap();
        let b = store.users.get("bob").unwrap();
        // Salted: identical passwords yield different stored hashes.
        assert_ne!(a, b);
        // Never plaintext, and a recognisable Argon2id PHC string.
        assert!(a.starts_with("$argon2id$"));
        assert!(!a.contains("samepass"));
    }

    #[test]
    fn authenticate_basic_round_trips() {
        let mut store = UserStore::new();
        store.add_user("neo4j", "s3cret").unwrap();
        assert_eq!(
            store.authenticate(&basic_extra("neo4j", "s3cret")),
            AuthOutcome::Authenticated
        );
        assert!(matches!(
            store.authenticate(&basic_extra("neo4j", "nope")),
            AuthOutcome::Denied(_)
        ));
    }

    #[test]
    fn authenticate_missing_fields_denied() {
        let store = UserStore::new();
        let no_principal =
            BTreeMap::from([("credentials".to_string(), Value::String("x".to_string()))]);
        assert!(matches!(
            store.authenticate(&no_principal),
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
        assert_eq!(store.authenticate(&extra), AuthOutcome::Authenticated);
        assert_eq!(store.verify_token(&token).as_deref(), Some("neo4j"));
    }

    #[test]
    fn issue_token_refuses_unknown_user() {
        let store = UserStore::new();
        assert!(store.issue_token("ghost").is_none());
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
        assert!(matches!(store.authenticate(&extra), AuthOutcome::Denied(_)));
    }

    #[test]
    fn issued_tokens_are_unique() {
        let mut store = UserStore::new();
        store.add_user("neo4j", "s3cret").unwrap();
        let t1 = store.issue_token("neo4j").unwrap();
        let t2 = store.issue_token("neo4j").unwrap();
        assert_ne!(t1, t2);
    }

    #[test]
    fn unsupported_scheme_denied() {
        let store = UserStore::new();
        let extra = BTreeMap::from([("scheme".to_string(), Value::String("kerberos".to_string()))]);
        assert!(matches!(
            store.authenticate(&extra),
            AuthOutcome::Denied(msg) if msg.contains("kerberos")
        ));
    }
}
