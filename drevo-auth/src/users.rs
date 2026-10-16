//! User management: create, change and drop users in a stored
//! [`Catalog`].
//!
//! An [`AuthManager`] owns the catalog for one server. Every change is checked
//! (name rules, [`PasswordPolicy`]), hashed with Argon2id and saved through the
//! [`CatalogStore`] before the call returns; a change that cannot be saved
//! leaves the users as they were.
//!
//! [`AuthManager::bootstrap`] creates the first user when the catalog is
//! empty, from a [`Bootstrap`] spec in the `NEO4J_AUTH` style: `user/password`,
//! `none`, or nothing for a `neo4j` user with a generated password that must
//! be changed on first login.
//!
//! ```
//! use std::sync::Arc;
//! use drevo_auth::catalog::MemoryStore;
//! use drevo_auth::users::{AlterUser, AuthManager, NewUser, UserStatus};
//!
//! let users = AuthManager::open(Arc::new(MemoryStore::new())).unwrap();
//! users.create_user(NewUser::new("ann", "first-password")).unwrap();
//! assert!(users.user("ann").unwrap().password_change_required);
//!
//! users.change_own_password("ann", "first-password", "second-password").unwrap();
//! assert!(users.check_password("ann", "second-password"));
//!
//! users.alter_user("ann", AlterUser::default().status(UserStatus::Suspended)).unwrap();
//! assert_eq!(users.user("ann").unwrap().status, UserStatus::Suspended);
//! ```

use std::sync::{Arc, Mutex};

use crate::authn::{hash_password, verify_password};
use crate::catalog::{Catalog, CatalogStore, StoreError, User};

pub use crate::catalog::UserStatus;

/// The longest user name, in characters.
pub const MAX_NAME_LEN: usize = 65;

/// Rules a new password must meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordPolicy {
    /// The fewest characters a password may have.
    pub min_length: usize,
}

impl Default for PasswordPolicy {
    /// At least 8 characters, as Neo4j requires.
    fn default() -> Self {
        Self { min_length: 8 }
    }
}

/// A user to create, with its options. Built with [`NewUser::new`] and the
/// builder methods.
#[derive(Clone)]
pub struct NewUser {
    name: String,
    password: String,
    change_required: bool,
    status: UserStatus,
    if_not_exists: bool,
}

impl NewUser {
    /// A user named `name` with `password`: active, and required to change
    /// the password on first login.
    #[must_use]
    pub fn new(name: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            password: password.into(),
            change_required: true,
            status: UserStatus::Active,
            if_not_exists: false,
        }
    }

    /// Whether the user must change the password on first login.
    #[must_use]
    pub fn change_required(mut self, required: bool) -> Self {
        self.change_required = required;
        self
    }

    /// Whether the user may log in.
    #[must_use]
    pub fn status(mut self, status: UserStatus) -> Self {
        self.status = status;
        self
    }

    /// Do nothing, without an error, when the user already exists.
    #[must_use]
    pub fn if_not_exists(mut self) -> Self {
        self.if_not_exists = true;
        self
    }
}

impl std::fmt::Debug for NewUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewUser")
            .field("name", &self.name)
            .field("password", &"<redacted>")
            .field("change_required", &self.change_required)
            .field("status", &self.status)
            .field("if_not_exists", &self.if_not_exists)
            .finish()
    }
}

/// Changes to an existing user; fields left unset stay as they are.
#[derive(Clone, Default)]
pub struct AlterUser {
    password: Option<String>,
    change_required: Option<bool>,
    status: Option<UserStatus>,
}

impl AlterUser {
    /// Set a new password.
    #[must_use]
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Set whether the user must change the password on next login.
    #[must_use]
    pub fn change_required(mut self, required: bool) -> Self {
        self.change_required = Some(required);
        self
    }

    /// Set whether the user may log in.
    #[must_use]
    pub fn status(mut self, status: UserStatus) -> Self {
        self.status = Some(status);
        self
    }
}

impl std::fmt::Debug for AlterUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlterUser")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("change_required", &self.change_required)
            .field("status", &self.status)
            .finish()
    }
}

/// A user as shown to administrators: everything but the password hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    /// The user's name.
    pub name: String,
    /// Whether the user may log in.
    pub status: UserStatus,
    /// Whether the user must change the password before doing anything else.
    pub password_change_required: bool,
    /// When the user was created, in Unix milliseconds.
    pub created_at_ms: i64,
}

impl From<&User> for UserInfo {
    fn from(u: &User) -> Self {
        Self {
            name: u.name.clone(),
            status: u.status,
            password_change_required: u.password_change_required,
            created_at_ms: u.created_at_ms,
        }
    }
}

/// Why a user change was refused. Messages never contain a password.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManageError {
    /// The name breaks the naming rules.
    #[error("invalid user name '{name}': {reason}")]
    InvalidName {
        /// The refused name.
        name: String,
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// The password is shorter than the policy allows.
    #[error("a password must have at least {min_length} characters")]
    WeakPassword {
        /// The policy's minimum length.
        min_length: usize,
    },
    /// A user with this name exists.
    #[error("user '{0}' already exists")]
    UserExists(String),
    /// No user has this name.
    #[error("user '{0}' does not exist")]
    UserNotFound(String),
    /// The new password is the current one.
    #[error("the new password must differ from the current one")]
    SamePassword,
    /// The current password given for a change is wrong.
    #[error("the current password is incorrect")]
    WrongCurrentPassword,
    /// Hashing the password failed.
    #[error("password hashing failed: {0}")]
    Hash(String),
    /// The catalog could not be loaded or saved.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// How to create the first user when the catalog is empty, in the style of
/// Neo4j's `NEO4J_AUTH`.
#[derive(Clone, PartialEq, Eq)]
pub enum Bootstrap {
    /// Authentication is off; create nobody (`none`).
    Disabled,
    /// Create `neo4j` with a generated password that must be changed on first
    /// login (no spec given).
    Generated,
    /// Create this administrator with this password (`user/password`).
    Admin {
        /// The administrator's name.
        user: String,
        /// The administrator's password.
        password: String,
    },
}

impl std::fmt::Debug for Bootstrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("Disabled"),
            Self::Generated => f.write_str("Generated"),
            Self::Admin { user, .. } => f
                .debug_struct("Admin")
                .field("user", user)
                .field("password", &"<redacted>")
                .finish(),
        }
    }
}

/// A [`Bootstrap`] spec that could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid authentication setting: {0}")]
pub struct BootstrapError(&'static str);

impl Bootstrap {
    /// Read a spec: `None` for [`Bootstrap::Generated`], `none` for
    /// [`Bootstrap::Disabled`], `user/password` for [`Bootstrap::Admin`] (the
    /// first `/` separates, so the password may contain `/`).
    ///
    /// ```
    /// use drevo_auth::users::Bootstrap;
    ///
    /// assert_eq!(Bootstrap::parse(Some("none")), Ok(Bootstrap::Disabled));
    /// assert!(matches!(Bootstrap::parse(Some("admin/s3cret-pass")), Ok(Bootstrap::Admin { .. })));
    /// ```
    ///
    /// # Errors
    ///
    /// A [`BootstrapError`] for an empty spec, a missing `/`, an empty user or
    /// password, or a user name that breaks the naming rules. The error never
    /// contains the password.
    pub fn parse(spec: Option<&str>) -> Result<Self, BootstrapError> {
        let Some(spec) = spec else {
            return Ok(Self::Generated);
        };
        if spec.eq_ignore_ascii_case("none") {
            return Ok(Self::Disabled);
        }
        let (user, password) = spec
            .split_once('/')
            .ok_or(BootstrapError("expected user/password or none"))?;
        if user.is_empty() || password.is_empty() {
            return Err(BootstrapError("expected user/password or none"));
        }
        if validate_name(user).is_err() {
            return Err(BootstrapError("the user name breaks the naming rules"));
        }
        Ok(Self::Admin {
            user: user.to_string(),
            password: password.to_string(),
        })
    }
}

/// What [`AuthManager::bootstrap`] did.
#[derive(Clone, PartialEq, Eq)]
pub enum BootstrapOutcome {
    /// Authentication is off; nothing was created.
    Disabled,
    /// The catalog already had users; nothing was changed.
    AlreadyInitialised,
    /// The administrator from the spec was created.
    Created,
    /// `user` was created with this generated `password`, which must be shown
    /// to the operator once and changed on first login.
    Generated {
        /// The created user (`neo4j`).
        user: String,
        /// The generated password.
        password: String,
    },
}

impl std::fmt::Debug for BootstrapOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("Disabled"),
            Self::AlreadyInitialised => f.write_str("AlreadyInitialised"),
            Self::Created => f.write_str("Created"),
            Self::Generated { user, .. } => f
                .debug_struct("Generated")
                .field("user", user)
                .field("password", &"<redacted>")
                .finish(),
        }
    }
}

/// The users of one server, stored in a [`CatalogStore`].
///
/// `AuthManager` is `Send + Sync`; changes are serialised, so concurrent
/// changes are all kept.
pub struct AuthManager {
    store: Arc<dyn CatalogStore>,
    catalog: Mutex<Catalog>,
    policy: PasswordPolicy,
}

impl AuthManager {
    /// Load the catalog from `store`; an empty store is an empty catalog.
    ///
    /// # Errors
    ///
    /// [`ManageError::Store`] when the stored catalog cannot be read.
    pub fn open(store: Arc<dyn CatalogStore>) -> Result<Self, ManageError> {
        let catalog = store.load()?.unwrap_or_default();
        Ok(Self {
            store,
            catalog: Mutex::new(catalog),
            policy: PasswordPolicy::default(),
        })
    }

    /// Use `policy` for new passwords.
    #[must_use]
    pub fn with_policy(mut self, policy: PasswordPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// The password policy in force.
    #[must_use]
    pub fn policy(&self) -> PasswordPolicy {
        self.policy
    }

    /// Create a user.
    ///
    /// # Errors
    ///
    /// [`ManageError::InvalidName`], [`ManageError::WeakPassword`],
    /// [`ManageError::UserExists`] (unless [`NewUser::if_not_exists`]), or a
    /// store or hashing failure.
    pub fn create_user(&self, new: NewUser) -> Result<(), ManageError> {
        validate_name(&new.name)?;
        self.change(|catalog| {
            if catalog.users.contains_key(&new.name) {
                return if new.if_not_exists {
                    Ok(false)
                } else {
                    Err(ManageError::UserExists(new.name.clone()))
                };
            }
            self.check_policy(&new.password)?;
            let now = now_ms();
            catalog.users.insert(
                new.name.clone(),
                User {
                    name: new.name.clone(),
                    password_hash: hash(&new.password)?,
                    status: new.status,
                    password_change_required: new.change_required,
                    created_at_ms: now,
                    password_changed_at_ms: now,
                },
            );
            Ok(true)
        })
    }

    /// Change a user's password, status or change requirement.
    ///
    /// # Errors
    ///
    /// [`ManageError::UserNotFound`], [`ManageError::WeakPassword`],
    /// [`ManageError::SamePassword`], or a store or hashing failure.
    pub fn alter_user(&self, name: &str, alter: AlterUser) -> Result<(), ManageError> {
        self.change(|catalog| {
            let user = catalog
                .users
                .get_mut(name)
                .ok_or_else(|| ManageError::UserNotFound(name.to_string()))?;
            if let Some(password) = &alter.password {
                self.check_policy(password)?;
                if verify_password(password, &user.password_hash) {
                    return Err(ManageError::SamePassword);
                }
                user.password_hash = hash(password)?;
                user.password_changed_at_ms = now_ms();
            }
            if let Some(required) = alter.change_required {
                user.password_change_required = required;
            }
            if let Some(status) = alter.status {
                user.status = status;
            }
            Ok(true)
        })
    }

    /// Drop a user.
    ///
    /// # Errors
    ///
    /// [`ManageError::UserNotFound`] unless `if_exists`, or a store failure.
    pub fn drop_user(&self, name: &str, if_exists: bool) -> Result<(), ManageError> {
        self.change(|catalog| match catalog.users.remove(name) {
            Some(_) => Ok(true),
            None if if_exists => Ok(false),
            None => Err(ManageError::UserNotFound(name.to_string())),
        })
    }

    /// A user's own password change: `current` must be the password now. A
    /// successful change clears the change requirement.
    ///
    /// # Errors
    ///
    /// [`ManageError::WrongCurrentPassword`] for a wrong `current` or an
    /// unknown user (the same error, so names cannot be probed),
    /// [`ManageError::SamePassword`], [`ManageError::WeakPassword`], or a store
    /// or hashing failure.
    pub fn change_own_password(
        &self,
        name: &str,
        current: &str,
        new: &str,
    ) -> Result<(), ManageError> {
        self.change(|catalog| {
            let user = catalog
                .users
                .get_mut(name)
                .filter(|u| verify_password(current, &u.password_hash))
                .ok_or(ManageError::WrongCurrentPassword)?;
            if new == current {
                return Err(ManageError::SamePassword);
            }
            self.check_policy(new)?;
            user.password_hash = hash(new)?;
            user.password_changed_at_ms = now_ms();
            user.password_change_required = false;
            Ok(true)
        })
    }

    /// Every user, ordered by name.
    #[must_use]
    pub fn users(&self) -> Vec<UserInfo> {
        self.read(|c| c.users.values().map(UserInfo::from).collect())
    }

    /// One user, or `None` if no user has this name.
    #[must_use]
    pub fn user(&self, name: &str) -> Option<UserInfo> {
        self.read(|c| c.users.get(name).map(UserInfo::from))
    }

    /// Whether `password` is `name`'s password. `false` for an unknown user.
    /// This checks the password only, not whether the user may log in.
    #[must_use]
    pub fn check_password(&self, name: &str, password: &str) -> bool {
        // Hash outside the lock: Argon2 is deliberately slow.
        let stored = self.read(|c| c.users.get(name).map(|u| u.password_hash.clone()));
        stored.is_some_and(|hash| verify_password(password, &hash))
    }

    /// Create the first user from `spec` when the catalog has no users.
    ///
    /// # Errors
    ///
    /// The errors of [`create_user`](Self::create_user), e.g.
    /// [`ManageError::WeakPassword`] for a short bootstrap password.
    pub fn bootstrap(&self, spec: &Bootstrap) -> Result<BootstrapOutcome, ManageError> {
        if *spec == Bootstrap::Disabled {
            return Ok(BootstrapOutcome::Disabled);
        }
        if self.read(|c| !c.users.is_empty()) {
            return Ok(BootstrapOutcome::AlreadyInitialised);
        }
        match spec {
            Bootstrap::Admin { user, password } => {
                self.create_user(NewUser::new(user, password).change_required(false))?;
                Ok(BootstrapOutcome::Created)
            }
            Bootstrap::Generated => {
                let password = generate_password();
                self.create_user(NewUser::new("neo4j", password.clone()))?;
                Ok(BootstrapOutcome::Generated {
                    user: "neo4j".to_string(),
                    password,
                })
            }
            Bootstrap::Disabled => Ok(BootstrapOutcome::Disabled),
        }
    }

    fn check_policy(&self, password: &str) -> Result<(), ManageError> {
        if password.chars().count() < self.policy.min_length {
            Err(ManageError::WeakPassword {
                min_length: self.policy.min_length,
            })
        } else {
            Ok(())
        }
    }

    fn read<T>(&self, f: impl FnOnce(&Catalog) -> T) -> T {
        f(&self.catalog.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Apply `edit` to a copy of the catalog and, when it reports a change,
    /// save the copy and keep it. A failed edit or save leaves the catalog as
    /// it was.
    fn change(
        &self,
        edit: impl FnOnce(&mut Catalog) -> Result<bool, ManageError>,
    ) -> Result<(), ManageError> {
        let mut guard = self.catalog.lock().unwrap_or_else(|p| p.into_inner());
        let mut next = guard.clone();
        if edit(&mut next)? {
            self.store.save(&next)?;
            *guard = next;
        }
        Ok(())
    }
}

/// Check a user name: 1 to [`MAX_NAME_LEN`] characters, each an ASCII letter,
/// digit, `_`, `-`, `.` or `@`.
fn validate_name(name: &str) -> Result<(), ManageError> {
    let reason = if name.is_empty() {
        Some("it is empty")
    } else if name.chars().count() > MAX_NAME_LEN {
        Some("it is longer than 65 characters")
    } else if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@'))
    {
        Some("only letters, digits, '_', '-', '.' and '@' are allowed")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(ManageError::InvalidName {
            name: name.to_string(),
            reason,
        }),
        None => Ok(()),
    }
}

fn hash(password: &str) -> Result<String, ManageError> {
    hash_password(password).map_err(|e| ManageError::Hash(e.to_string()))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// 24 random letters and digits from the operating system (about 143 bits).
fn generate_password() -> String {
    use argon2::password_hash::rand_core::{OsRng, RngCore};
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(24);
    while out.len() < 24 {
        let mut byte = [0u8; 1];
        OsRng.fill_bytes(&mut byte);
        // Reject the top of the byte range so every character is equally likely.
        if usize::from(byte[0]) < 248 {
            out.push(char::from(ALPHABET[usize::from(byte[0]) % ALPHABET.len()]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_passwords_use_letters_and_digits_only() {
        for _ in 0..50 {
            let p = generate_password();
            assert_eq!(p.len(), 24);
            assert!(p.chars().all(|c| c.is_ascii_alphanumeric()));
        }
    }

    #[test]
    fn debug_output_redacts_passwords() {
        let text = format!(
            "{:?} {:?} {:?} {:?}",
            NewUser::new("ann", "secret-one"),
            AlterUser::default().password("secret-two"),
            Bootstrap::Admin {
                user: "ann".into(),
                password: "secret-three".into()
            },
            BootstrapOutcome::Generated {
                user: "neo4j".into(),
                password: "secret-four".into()
            }
        );
        assert!(!text.contains("secret"), "{text}");
        assert!(text.contains("ann") && text.contains("neo4j"));
    }

    #[test]
    fn a_failed_save_leaves_the_users_unchanged() {
        struct FailingStore;
        impl CatalogStore for FailingStore {
            fn load(&self) -> Result<Option<Catalog>, StoreError> {
                Ok(None)
            }
            fn save(&self, _: &Catalog) -> Result<(), StoreError> {
                Err(StoreError::Io {
                    path: "x".into(),
                    message: "disk full".into(),
                })
            }
        }
        let m = AuthManager::open(Arc::new(FailingStore)).unwrap();
        assert!(matches!(
            m.create_user(NewUser::new("ann", "password1")),
            Err(ManageError::Store(StoreError::Io { .. }))
        ));
        assert!(m.users().is_empty());
    }
}
