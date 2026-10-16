//! The server's users: running the Cypher user-management commands
//! ([`crate::cypher::security_admin`]) against the security catalog of the
//! `drevo-auth` crate.
//!
//! A [`Security`](crate::security::Security) handle is shared by the HTTP API and every Bolt session.
//! With the `auth` feature it holds the server's
//! [`AuthManager`](drevo_auth::users::AuthManager), whose catalog lives in
//! `<data dir>/security.json` (see `open_server_security`); without it, or
//! when the server was started without one, the commands answer that user
//! management is not enabled.
//!
//! `SHOW USERS` returns the Neo4j columns `user`, `roles`,
//! `passwordChangeRequired`, `suspended` and `home`. A password or its hash
//! never appears in a result or an error.

use crate::cypher::executor::ExecResult;
use crate::cypher::security_admin::SecurityCommand;
#[cfg(feature = "auth")]
use crate::cypher::{
    executor::{ExecStats, Value},
    security_admin::Secret,
};

#[cfg(feature = "auth")]
use std::sync::Arc;

/// What kind of failure a user command hit; it decides the Neo4j error code
/// and the HTTP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityErrorKind {
    /// The command is malformed.
    Syntax,
    /// A `$parameter` the command names was not given.
    MissingParameter,
    /// A name, password or parameter value is not acceptable.
    InvalidArgument,
    /// The user already exists.
    AlreadyExists,
    /// The user does not exist.
    NotFound,
    /// The command needs a signed-in user and there is none, or a login was
    /// refused.
    Unauthenticated,
    /// Too many failed logins in a row for this user.
    RateLimited,
    /// The signed-in user must change the password before anything else.
    CredentialsExpired,
    /// User management is not enabled on this server.
    Unavailable,
    /// The security catalog could not be saved or read.
    Internal,
}

impl SecurityErrorKind {
    /// The Neo4j status code reported over Bolt and in HTTP error bodies.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Syntax => "Neo.ClientError.Statement.SyntaxError",
            Self::MissingParameter => "Neo.ClientError.Statement.ParameterMissing",
            Self::InvalidArgument | Self::AlreadyExists | Self::NotFound => {
                "Neo.ClientError.General.InvalidArguments"
            }
            Self::Unauthenticated => "Neo.ClientError.Security.Unauthorized",
            Self::RateLimited => "Neo.ClientError.Security.AuthenticationRateLimit",
            Self::CredentialsExpired => "Neo.ClientError.Security.CredentialsExpired",
            Self::Unavailable | Self::Internal => "Neo.DatabaseError.General.UnknownError",
        }
    }

    /// The HTTP status for this failure.
    #[must_use]
    pub fn http_status(self) -> u16 {
        match self {
            Self::Syntax | Self::MissingParameter | Self::InvalidArgument => 400,
            Self::AlreadyExists => 409,
            Self::NotFound => 404,
            Self::Unauthenticated => 401,
            Self::RateLimited => 429,
            Self::CredentialsExpired => 403,
            Self::Unavailable => 503,
            Self::Internal => 500,
        }
    }
}

/// A failed user command. The message never contains a password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityError {
    /// What went wrong.
    pub kind: SecurityErrorKind,
    /// A message for the client.
    pub message: String,
}

impl SecurityError {
    fn new(kind: SecurityErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// A syntax error from [`crate::cypher::security_admin::parse`].
    #[must_use]
    pub fn syntax(message: String) -> Self {
        Self::new(SecurityErrorKind::Syntax, message)
    }
}

impl std::fmt::Display for SecurityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SecurityError {}

/// A query parameter as a user command sees it: a string, or something else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamValue {
    /// A string value.
    String(String),
    /// Any other kind of value (number, list, …).
    Other,
}

/// A logged-in user of a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedIn {
    /// The user's name.
    pub name: String,
    /// Whether the user must change the password before anything else.
    pub password_change_required: bool,
}

/// The message for every refused login: the same whether the user is
/// unknown, the password wrong, the token expired or the user suspended, so
/// a client learns nothing about which.
pub const LOGIN_REFUSED: &str = "The client is unauthorized due to authentication failure.";

/// The message for a user who must change the password first.
pub const CHANGE_PASSWORD_FIRST: &str = "The credentials you provided were valid, but must be \
     changed before you can use this instance. Run ALTER CURRENT USER SET PASSWORD FROM \
     <current> TO <new>.";

/// The server's users, shared by HTTP, Bolt, gRPC and Arrow Flight. Cheap to clone.
#[derive(Clone, Default)]
pub struct Security {
    #[cfg(feature = "auth")]
    users: Option<Arc<drevo_auth::users::AuthManager>>,
    #[cfg(feature = "auth")]
    require_login: bool,
}

impl std::fmt::Debug for Security {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Security")
            .field("enabled", &self.is_enabled())
            .field("requires_login", &self.requires_login())
            .finish()
    }
}

impl Security {
    /// User management backed by `users`.
    #[cfg(feature = "auth")]
    #[must_use]
    pub fn new(users: Arc<drevo_auth::users::AuthManager>) -> Self {
        Self {
            users: Some(users),
            require_login: true,
        }
    }

    /// The same users, but connections need no login (`DREVO_AUTH=none`).
    #[cfg(feature = "auth")]
    #[must_use]
    pub fn without_login(mut self) -> Self {
        self.require_login = false;
        self
    }

    /// The same users, but connections need no login (`DREVO_AUTH=none`).
    /// Without user management nothing needs a login anyway.
    #[cfg(not(feature = "auth"))]
    #[must_use]
    pub fn without_login(self) -> Self {
        self
    }

    /// The users, when user management is enabled.
    #[cfg(feature = "auth")]
    #[must_use]
    pub fn users(&self) -> Option<&Arc<drevo_auth::users::AuthManager>> {
        self.users.as_ref()
    }

    /// Whether a connection must log in before it runs anything.
    #[must_use]
    pub fn requires_login(&self) -> bool {
        #[cfg(feature = "auth")]
        {
            self.users.is_some() && self.require_login
        }
        #[cfg(not(feature = "auth"))]
        {
            false
        }
    }

    /// Log a connection in from the Neo4j `(scheme, principal, credentials)`
    /// triple: a password, or a session token as `bearer`.
    ///
    /// # Errors
    ///
    /// [`SecurityErrorKind::Unauthenticated`] with [`LOGIN_REFUSED`] for any
    /// refused login, [`SecurityErrorKind::RateLimited`] after repeated
    /// failures, [`SecurityErrorKind::Unavailable`] without user management.
    pub fn login(
        &self,
        scheme: Option<&str>,
        principal: Option<&str>,
        credentials: Option<&str>,
    ) -> Result<SignedIn, SecurityError> {
        #[cfg(feature = "auth")]
        {
            use drevo_auth::authn::Credentials;
            use drevo_auth::users::LoginError;
            let Some(users) = &self.users else {
                return Err(not_enabled());
            };
            let credentials =
                Credentials::from_parts(scheme, principal, credentials).map_err(|e| {
                    SecurityError::new(SecurityErrorKind::Unauthenticated, e.to_string())
                })?;
            match users.authenticate(&credentials) {
                Ok(p) => Ok(SignedIn {
                    name: p.name,
                    password_change_required: p.password_change_required,
                }),
                Err(e @ LoginError::Locked { .. }) => Err(SecurityError::new(
                    SecurityErrorKind::RateLimited,
                    e.to_string(),
                )),
                Err(_) => Err(SecurityError::new(
                    SecurityErrorKind::Unauthenticated,
                    LOGIN_REFUSED,
                )),
            }
        }
        #[cfg(not(feature = "auth"))]
        {
            let _ = (scheme, principal, credentials);
            Err(not_enabled())
        }
    }

    /// Log an HTTP request in from its `Authorization` header (`Basic` or
    /// `Bearer`) or, failing that, the session token from its cookie.
    ///
    /// # Errors
    ///
    /// As [`login`](Self::login); no or malformed credentials are refused
    /// with [`LOGIN_REFUSED`].
    pub fn login_http(
        &self,
        authorization: Option<&str>,
        cookie_token: Option<&str>,
    ) -> Result<SignedIn, SecurityError> {
        #[cfg(feature = "auth")]
        {
            use drevo_auth::authn::Credentials;
            let refused = || SecurityError::new(SecurityErrorKind::Unauthenticated, LOGIN_REFUSED);
            let credentials = match (authorization, cookie_token) {
                (Some(header), _) => {
                    Credentials::from_http_authorization(header).ok_or_else(refused)?
                }
                (None, Some(token)) => Credentials::Token(token.to_string()),
                (None, None) => return Err(refused()),
            };
            match credentials {
                Credentials::Basic { username, password } => {
                    self.login(Some("basic"), Some(&username), Some(&password))
                }
                Credentials::Token(token) => self.login(Some("bearer"), None, Some(&token)),
            }
        }
        #[cfg(not(feature = "auth"))]
        {
            let _ = (authorization, cookie_token);
            Err(not_enabled())
        }
    }

    /// Issue a session token for `name`, or `None` for an unknown or
    /// suspended user, or without user management.
    #[must_use]
    pub fn issue_session(&self, name: &str) -> Option<String> {
        #[cfg(feature = "auth")]
        {
            self.users.as_ref()?.issue_session(name)
        }
        #[cfg(not(feature = "auth"))]
        {
            let _ = name;
            None
        }
    }

    /// End a session; returns whether the token existed.
    pub fn revoke_session(&self, token: &str) -> bool {
        #[cfg(feature = "auth")]
        {
            self.users.as_ref().is_some_and(|u| u.revoke_session(token))
        }
        #[cfg(not(feature = "auth"))]
        {
            let _ = token;
            false
        }
    }

    /// Whether user management is enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        #[cfg(feature = "auth")]
        {
            self.users.is_some()
        }
        #[cfg(not(feature = "auth"))]
        {
            false
        }
    }

    /// Run `command` for `current_user` (the signed-in user, if any), reading
    /// `$parameters` through `param`.
    ///
    /// # Errors
    ///
    /// A [`SecurityError`] when the command is refused or user management is
    /// not enabled.
    pub fn execute(
        &self,
        command: &SecurityCommand,
        current_user: Option<&str>,
        param: &dyn Fn(&str) -> Option<ParamValue>,
    ) -> Result<ExecResult, SecurityError> {
        #[cfg(feature = "auth")]
        {
            let Some(users) = &self.users else {
                return Err(not_enabled());
            };
            run(users, command, current_user, param)
        }
        #[cfg(not(feature = "auth"))]
        {
            let _ = (command, current_user, param);
            Err(not_enabled())
        }
    }
}

fn not_enabled() -> SecurityError {
    SecurityError::new(
        SecurityErrorKind::Unavailable,
        "user management is not enabled on this server",
    )
}

/// Resolve a password: a literal as is, a `$parameter` from `param`.
#[cfg(feature = "auth")]
fn secret(
    secret: &Secret,
    param: &dyn Fn(&str) -> Option<ParamValue>,
) -> Result<String, SecurityError> {
    match secret {
        Secret::Literal(s) => Ok(s.clone()),
        Secret::Param(name) => match param(name) {
            Some(ParamValue::String(s)) => Ok(s),
            Some(ParamValue::Other) => Err(SecurityError::new(
                SecurityErrorKind::InvalidArgument,
                format!("parameter ${name} must be a string password"),
            )),
            None => Err(SecurityError::new(
                SecurityErrorKind::MissingParameter,
                format!("expected parameter ${name}"),
            )),
        },
    }
}

#[cfg(feature = "auth")]
fn result(columns: &[&str], rows: Vec<Vec<Value>>) -> ExecResult {
    ExecResult {
        columns: columns.iter().map(|c| (*c).to_string()).collect(),
        rows,
        stats: ExecStats::default(),
    }
}

#[cfg(feature = "auth")]
const USER_COLUMNS: [&str; 5] = [
    "user",
    "roles",
    "passwordChangeRequired",
    "suspended",
    "home",
];

#[cfg(feature = "auth")]
fn run(
    users: &drevo_auth::users::AuthManager,
    command: &SecurityCommand,
    current_user: Option<&str>,
    param: &dyn Fn(&str) -> Option<ParamValue>,
) -> Result<ExecResult, SecurityError> {
    use drevo_auth::users::{AlterUser, NewUser};

    match command {
        SecurityCommand::CreateUser {
            name,
            password,
            change_required,
            status,
            if_not_exists,
        } => {
            let mut new = NewUser::new(name, secret(password, param)?);
            if let Some(required) = change_required {
                new = new.change_required(*required);
            }
            if let Some(status) = status {
                new = new.status(to_status(*status));
            }
            if *if_not_exists {
                new = new.if_not_exists();
            }
            users.create_user(new).map_err(manage_error)?;
        }
        SecurityCommand::AlterUser {
            name,
            if_exists,
            password,
            change_required,
            status,
        } => {
            let mut alter = AlterUser::default();
            if let Some(password) = password {
                alter = alter.password(secret(password, param)?);
            }
            if let Some(required) = change_required {
                alter = alter.change_required(*required);
            }
            if let Some(status) = status {
                alter = alter.status(to_status(*status));
            }
            match users.alter_user(name, alter) {
                Err(drevo_auth::users::ManageError::UserNotFound(_)) if *if_exists => {}
                other => other.map_err(manage_error)?,
            }
        }
        SecurityCommand::DropUser { name, if_exists } => {
            users.drop_user(name, *if_exists).map_err(manage_error)?;
        }
        SecurityCommand::ShowUsers => {
            let rows = users.users().iter().map(user_row).collect();
            return Ok(result(&USER_COLUMNS, rows));
        }
        SecurityCommand::ShowCurrentUser => {
            let rows = current_user
                .and_then(|name| users.user(name))
                .iter()
                .map(user_row)
                .collect();
            return Ok(result(&USER_COLUMNS, rows));
        }
        SecurityCommand::AlterCurrentUserPassword { current, new } => {
            let Some(name) = current_user else {
                return Err(SecurityError::new(
                    SecurityErrorKind::Unauthenticated,
                    "ALTER CURRENT USER needs a signed-in user",
                ));
            };
            let (current, new) = (secret(current, param)?, secret(new, param)?);
            users
                .change_own_password(name, &current, &new)
                .map_err(manage_error)?;
        }
    }
    Ok(result(&[], Vec::new()))
}

#[cfg(feature = "auth")]
fn user_row(u: &drevo_auth::users::UserInfo) -> Vec<Value> {
    vec![
        Value::String(u.name.clone()),
        // Roles arrive with role management; until then a user has none.
        Value::List(Vec::new()),
        Value::Bool(u.password_change_required),
        Value::Bool(u.status == drevo_auth::users::UserStatus::Suspended),
        Value::Null,
    ]
}

#[cfg(feature = "auth")]
fn to_status(s: crate::cypher::security_admin::UserStatus) -> drevo_auth::users::UserStatus {
    match s {
        crate::cypher::security_admin::UserStatus::Active => drevo_auth::users::UserStatus::Active,
        crate::cypher::security_admin::UserStatus::Suspended => {
            drevo_auth::users::UserStatus::Suspended
        }
    }
}

#[cfg(feature = "auth")]
fn manage_error(e: drevo_auth::users::ManageError) -> SecurityError {
    use drevo_auth::users::ManageError as M;
    let kind = match &e {
        M::InvalidName { .. }
        | M::WeakPassword { .. }
        | M::SamePassword
        | M::WrongCurrentPassword => SecurityErrorKind::InvalidArgument,
        M::UserExists(_) => SecurityErrorKind::AlreadyExists,
        M::UserNotFound(_) => SecurityErrorKind::NotFound,
        M::Hash(_) | M::Store(_) => SecurityErrorKind::Internal,
    };
    SecurityError::new(kind, e.to_string())
}

/// Open the server's users: `<data_dir>/security.json` for a durable server,
/// memory when `data_dir` is `None`; then create the first user from
/// `bootstrap` when there are none.
///
/// # Errors
///
/// A [`ManageError`](drevo_auth::users::ManageError) when the catalog cannot
/// be read (a damaged file stops the server rather than starting it empty) or
/// the bootstrap user cannot be created.
#[cfg(feature = "auth")]
pub fn open_server_security(
    data_dir: Option<&std::path::Path>,
    bootstrap: &drevo_auth::users::Bootstrap,
) -> Result<(Security, drevo_auth::users::BootstrapOutcome), drevo_auth::users::ManageError> {
    use drevo_auth::catalog::{CatalogStore, FileStore, MemoryStore};
    let store: Arc<dyn CatalogStore> = match data_dir {
        Some(dir) => Arc::new(FileStore::new(dir.join("security.json"))),
        None => Arc::new(MemoryStore::new()),
    };
    let users = Arc::new(drevo_auth::users::AuthManager::open(store)?);
    let outcome = users.bootstrap(bootstrap)?;
    let security = Security::new(users);
    let security = if *bootstrap == drevo_auth::users::Bootstrap::Disabled {
        security.without_login()
    } else {
        security
    };
    Ok((security, outcome))
}
