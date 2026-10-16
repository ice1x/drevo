//! Authentication: users with Argon2id password hashes, and session tokens.
//!
//! A client presents [`Credentials`] — a username and password, or a session
//! token — and a [`UserStore`] answers with the username they prove, or an
//! [`AuthFailure`] saying why not.
//!
//! - Passwords are stored as Argon2id PHC strings (`$argon2id$…`) with a
//!   random salt per user: two users with the same password get different
//!   hashes, and the store never holds a password.
//! - A session token is 32 random bytes from the operating system, written as
//!   64 lowercase hex characters. [`UserStore::issue_token`] hands one out
//!   after a successful login, so a client can reconnect without resending the
//!   password, and [`UserStore::revoke_token`] ends it. Tokens live in memory.
//! - An unknown user and a wrong password fail the same way
//!   ([`AuthFailure::InvalidCredentials`]), so a client cannot probe which
//!   names exist.
//!
//! ```
//! use drevo_auth::authn::{AuthFailure, Credentials, UserStore};
//!
//! let mut users = UserStore::new();
//! users.add_user("ann", "s3cret").unwrap();
//!
//! let login = Credentials::from_parts(Some("basic"), Some("ann"), Some("s3cret")).unwrap();
//! assert_eq!(users.authenticate(&login), Ok("ann".to_string()));
//!
//! let token = users.issue_token("ann").unwrap();
//! assert_eq!(users.authenticate(&Credentials::Token(token)), Ok("ann".to_string()));
//!
//! let wrong = Credentials::Basic { username: "ann".into(), password: "nope".into() };
//! assert_eq!(users.authenticate(&wrong), Err(AuthFailure::InvalidCredentials));
//! ```

use std::collections::HashMap;
use std::sync::Mutex;

/// What a client presents to prove who it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credentials {
    /// A username and password.
    Basic {
        /// The user's name.
        username: String,
        /// The password, as typed.
        password: String,
    },
    /// A session token from [`UserStore::issue_token`].
    Token(String),
}

impl Credentials {
    /// Credentials from the `(scheme, principal, credentials)` triple that
    /// Neo4j-style clients send. A missing scheme means `basic`; `bearer`
    /// carries a session token in `credentials`.
    ///
    /// ```
    /// use drevo_auth::authn::{AuthFailure, Credentials};
    ///
    /// assert_eq!(
    ///     Credentials::from_parts(Some("bearer"), None, Some("abc")),
    ///     Ok(Credentials::Token("abc".into()))
    /// );
    /// assert_eq!(
    ///     Credentials::from_parts(Some("basic"), None, Some("pw")),
    ///     Err(AuthFailure::MissingPrincipal)
    /// );
    /// ```
    ///
    /// # Errors
    ///
    /// [`AuthFailure::MissingPrincipal`], [`AuthFailure::MissingCredentials`]
    /// or [`AuthFailure::MissingToken`] when a needed part is absent, and
    /// [`AuthFailure::UnsupportedScheme`] for any scheme other than `basic`
    /// and `bearer`.
    pub fn from_parts(
        scheme: Option<&str>,
        principal: Option<&str>,
        credentials: Option<&str>,
    ) -> Result<Self, AuthFailure> {
        match scheme.unwrap_or("basic") {
            "basic" => {
                let username = principal.ok_or(AuthFailure::MissingPrincipal)?;
                let password = credentials.ok_or(AuthFailure::MissingCredentials)?;
                Ok(Credentials::Basic {
                    username: username.to_string(),
                    password: password.to_string(),
                })
            }
            "bearer" => {
                let token = credentials.ok_or(AuthFailure::MissingToken)?;
                Ok(Credentials::Token(token.to_string()))
            }
            other => Err(AuthFailure::UnsupportedScheme(other.to_string())),
        }
    }
}

/// Why credentials were refused. The message is safe to send to the client:
/// it never contains a password or a token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthFailure {
    /// `basic` credentials without a username.
    #[error("missing principal")]
    MissingPrincipal,
    /// `basic` credentials without a password.
    #[error("missing credentials")]
    MissingCredentials,
    /// `bearer` credentials without a token.
    #[error("missing bearer token")]
    MissingToken,
    /// A scheme other than `basic` or `bearer`.
    #[error("unsupported auth scheme: {0}")]
    UnsupportedScheme(String),
    /// An unknown user or a wrong password — deliberately the same failure.
    #[error("invalid principal or credentials")]
    InvalidCredentials,
    /// A token that was never issued or has been revoked.
    #[error("invalid or expired token")]
    InvalidToken,
}

/// An error while adding a user to a [`UserStore`].
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Argon2 could not hash the password. Not expected for normal input; it
    /// is returned rather than panicking.
    #[error("password hashing failed: {0}")]
    Hash(String),
    /// A user with this name already exists.
    #[error("user already exists: {0}")]
    DuplicateUser(String),
}

/// Users and their Argon2id password hashes, plus the live session tokens.
///
/// `UserStore` is `Send + Sync`: one store is shared by reference across
/// every connection.
pub struct UserStore {
    users: HashMap<String, String>,
    tokens: Mutex<HashMap<String, String>>,
}

impl Default for UserStore {
    fn default() -> Self {
        Self::new()
    }
}

impl UserStore {
    /// An empty store. With no users every login fails.
    #[must_use]
    pub fn new() -> Self {
        Self {
            users: HashMap::new(),
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// Add a user, storing an Argon2id hash of `password`.
    ///
    /// # Errors
    ///
    /// [`AuthError::DuplicateUser`] when `username` already exists;
    /// [`AuthError::Hash`] if hashing fails.
    pub fn add_user(&mut self, username: &str, password: &str) -> Result<(), AuthError> {
        if self.users.contains_key(username) {
            return Err(AuthError::DuplicateUser(username.to_string()));
        }
        let hash = hash_password(password)?;
        self.users.insert(username.to_string(), hash);
        Ok(())
    }

    /// The stored Argon2id PHC string for `username`, or `None` for an unknown
    /// user. It identifies the hash parameters and salt; it does not reveal
    /// the password.
    #[must_use]
    pub fn password_hash(&self, username: &str) -> Option<&str> {
        self.users.get(username).map(String::as_str)
    }

    /// Whether `password` is `username`'s password. `false` for an unknown
    /// user as for a wrong password.
    #[must_use]
    pub fn verify_basic(&self, username: &str, password: &str) -> bool {
        self.users
            .get(username)
            .is_some_and(|stored| verify_password(password, stored))
    }

    /// The username that `credentials` prove.
    ///
    /// # Errors
    ///
    /// [`AuthFailure::InvalidCredentials`] for an unknown user or a wrong
    /// password; [`AuthFailure::InvalidToken`] for a token that was never
    /// issued or has been revoked.
    pub fn authenticate(&self, credentials: &Credentials) -> Result<String, AuthFailure> {
        match credentials {
            Credentials::Basic { username, password } => {
                if self.verify_basic(username, password) {
                    Ok(username.clone())
                } else {
                    Err(AuthFailure::InvalidCredentials)
                }
            }
            Credentials::Token(token) => self.verify_token(token).ok_or(AuthFailure::InvalidToken),
        }
    }

    /// Issue a new session token for `username`, or `None` for an unknown user.
    #[must_use]
    pub fn issue_token(&self, username: &str) -> Option<String> {
        if !self.users.contains_key(username) {
            return None;
        }
        let token = new_token();
        self.tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(token.clone(), username.to_string());
        Some(token)
    }

    /// The username a session token was issued for, or `None` if it was never
    /// issued or has been revoked.
    #[must_use]
    pub fn verify_token(&self, token: &str) -> Option<String> {
        self.tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(token)
            .cloned()
    }

    /// End a session: the token no longer authenticates. Returns whether the
    /// token existed.
    pub fn revoke_token(&self, token: &str) -> bool {
        self.tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(token)
            .is_some()
    }
}

/// 32 random bytes from the operating system as 64 lowercase hex characters.
pub(crate) fn new_token() -> String {
    use argon2::password_hash::rand_core::{OsRng, RngCore};
    use std::fmt::Write;

    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().fold(String::with_capacity(64), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

pub(crate) fn hash_password(password: &str) -> Result<String, AuthError> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    use argon2::Argon2;

    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Hash(e.to_string()))
}

pub(crate) fn verify_password(password: &str, stored_phc: &str) -> bool {
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
mod tests {
    use super::*;

    #[test]
    fn a_corrupt_stored_hash_never_verifies() {
        assert!(!verify_password("anything", "not-a-phc-string"));
        assert!(!verify_password("", ""));
    }

    #[test]
    fn a_token_is_lowercase_hex_of_32_bytes() {
        let t = new_token();
        assert_eq!(t.len(), 64);
        assert!(t
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }
}
