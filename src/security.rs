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
    /// The signed-in user lacks the privilege the command needs.
    Forbidden,
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
            Self::Forbidden => "Neo.ClientError.Security.Forbidden",
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
            Self::CredentialsExpired | Self::Forbidden => 403,
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
    /// `None` for a user of the server's catalog, whose roles are there.
    /// `Some` for a user signed in with an identity provider's token: the
    /// roles its groups map to (without `PUBLIC`, which everyone has). Such a
    /// user has no record in the catalog, even when one has the same name.
    pub roles: Option<Vec<String>>,
}

impl SignedIn {
    /// A user of the server's catalog.
    #[must_use]
    pub fn local(name: impl Into<String>, password_change_required: bool) -> Self {
        Self {
            name: name.into(),
            password_change_required,
            roles: None,
        }
    }

    /// Whether the user signed in with an identity provider's token.
    #[must_use]
    pub fn is_external(&self) -> bool {
        self.roles.is_some()
    }
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
    /// Compiled security contexts by user, each with the catalog generation
    /// it was built at (#636).
    #[cfg(feature = "auth")]
    contexts: Arc<std::sync::Mutex<ContextCache>>,
    /// Where security events go (#637), if anywhere.
    #[cfg(feature = "auth")]
    audit: Option<Arc<dyn drevo_auth::audit::AuditSink>>,
    /// Whether a refused statement's text goes into its audit event.
    #[cfg(feature = "auth")]
    audit_queries: bool,
    /// Checks bearer tokens from an identity provider, when one is set up.
    #[cfg(feature = "oidc")]
    oidc: Option<Arc<drevo_auth::oidc::TokenValidator>>,
}

/// Where a request came from, for the audit log: the protocol and the
/// client's address.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Origin {
    /// `bolt`, `http`, `grpc` or `flight`.
    pub surface: &'static str,
    /// The client's `host:port`, when known.
    pub client: Option<String>,
}

impl Origin {
    /// A request over `surface` from an unknown address.
    #[must_use]
    pub fn new(surface: &'static str) -> Self {
        Self {
            surface,
            client: None,
        }
    }

    /// The same origin, from `client`.
    #[must_use]
    pub fn from_client(mut self, client: Option<impl ToString>) -> Self {
        self.client = client.map(|c| c.to_string());
        self
    }
}

/// Compiled contexts by (user, home database, the roles of an identity
/// provider's user).
#[cfg(feature = "auth")]
type ContextCache = std::collections::HashMap<
    (String, String, Option<Vec<String>>),
    (u64, drevo_auth::privileges::SecurityContext),
>;

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
            contexts: Arc::default(),
            audit: None,
            audit_queries: false,
            #[cfg(feature = "oidc")]
            oidc: None,
        }
    }

    /// Also accept bearer tokens checked by `validator`. Logins are then
    /// required even when the server was started with `DREVO_AUTH=none`:
    /// the users come from the provider.
    #[cfg(feature = "oidc")]
    #[must_use]
    pub fn with_oidc(mut self, validator: Arc<drevo_auth::oidc::TokenValidator>) -> Self {
        self.oidc = Some(validator);
        self.require_login = true;
        self
    }

    /// Record security events in `sink` (#637). With `queries`, a refused
    /// statement's text goes into its event; user and role commands never do,
    /// since they can carry passwords.
    #[cfg(feature = "auth")]
    #[must_use]
    pub fn with_audit(
        mut self,
        sink: Arc<dyn drevo_auth::audit::AuditSink>,
        queries: bool,
    ) -> Self {
        self.audit = Some(sink);
        self.audit_queries = queries;
        self
    }

    /// Record `event` in the audit log. A failed login, a lockout or a
    /// refusal is also logged at WARN, so it shows in the Web UI's problem
    /// feed whether or not an audit log is configured.
    #[cfg(feature = "auth")]
    pub fn record(&self, event: drevo_auth::audit::AuditEvent) {
        use drevo_auth::audit::AuditKind;
        if matches!(
            event.kind,
            AuditKind::LoginFailed | AuditKind::LoginLocked | AuditKind::Forbidden
        ) {
            tracing::warn!(
                target: "drevo::security",
                event = event.kind.name(),
                user = event.user.as_deref().unwrap_or(""),
                surface = event.surface.as_deref().unwrap_or(""),
                client = event.client.as_deref().unwrap_or(""),
                reason = event.reason.as_deref().unwrap_or(""),
                "security: {}",
                event.kind.name()
            );
        }
        if let Some(sink) = &self.audit {
            sink.record(&event);
        }
    }

    /// Record a statement or request refused for a missing privilege.
    pub fn record_forbidden(
        &self,
        origin: &Origin,
        user: Option<&str>,
        reason: &str,
        query: Option<&str>,
    ) {
        #[cfg(feature = "auth")]
        {
            use drevo_auth::audit::{AuditEvent, AuditKind, Outcome};
            let mut event = event_from(
                origin,
                AuditEvent::new(AuditKind::Forbidden, Outcome::Failure),
            )
            .reason(reason);
            if let Some(user) = user {
                event = event.user(user);
            }
            if let Some(query) = query.filter(|_| self.audit_queries) {
                event = event.query(query);
            }
            self.record(event);
        }
        #[cfg(not(feature = "auth"))]
        {
            let _ = (origin, user, reason, query);
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

    /// The privileges `user` runs a statement with (#636), with `home` as the
    /// user's home database: `None` when nothing is checked — the server needs
    /// no login, or has no user management. A signed-in user's context is
    /// compiled once and reused until the users, roles or privileges change.
    #[must_use]
    pub fn context_for(
        &self,
        user: Option<&SignedIn>,
        home: &str,
    ) -> Option<drevo_auth::privileges::SecurityContext> {
        #[cfg(feature = "auth")]
        {
            if !self.requires_login() {
                return None;
            }
            let users = self.users.as_ref()?;
            // A login is required, so a statement without a user may do
            // nothing; "" names no user.
            let name = user.map_or("", |u| u.name.as_str());
            let roles = user.and_then(|u| u.roles.clone());
            let generation = users.generation();
            let key = (name.to_string(), home.to_string(), roles);
            let mut cache = self.contexts.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((built, context)) = cache.get(&key) {
                if *built == generation {
                    return Some(context.clone());
                }
            }
            let context = match &key.2 {
                Some(roles) => users.security_context_for_roles(roles, home),
                None => users.security_context(name, home),
            };
            if cache.len() > 4096 {
                cache.clear();
            }
            cache.insert(key, (generation, context.clone()));
            Some(context)
        }
        #[cfg(not(feature = "auth"))]
        {
            let _ = (user, home);
            None
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
        origin: &Origin,
        scheme: Option<&str>,
        principal: Option<&str>,
        credentials: Option<&str>,
    ) -> Result<SignedIn, SecurityError> {
        #[cfg(feature = "auth")]
        {
            use drevo_auth::audit::{AuditEvent, AuditKind, Outcome};
            use drevo_auth::authn::Credentials;
            use drevo_auth::users::LoginError;
            let Some(users) = &self.users else {
                return Err(not_enabled());
            };
            let credentials =
                Credentials::from_parts(scheme, principal, credentials).map_err(|e| {
                    SecurityError::new(SecurityErrorKind::Unauthenticated, e.to_string())
                })?;
            #[cfg(feature = "oidc")]
            if let (Credentials::Token(token), Some(validator)) = (&credentials, &self.oidc) {
                // Session tokens are hex, so a JWT is never one.
                if drevo_auth::oidc::TokenValidator::looks_like_jwt(token) {
                    return self.login_with_provider(origin, validator, token);
                }
            }
            // A password login is audited either way; reusing a session token
            // (every HTTP request of a signed-in browser) only when refused.
            let password = matches!(credentials, Credentials::Basic { .. });
            let named = |event: AuditEvent| match principal {
                Some(p) if !p.is_empty() => event.user(p),
                _ => event,
            };
            match users.authenticate(&credentials) {
                Ok(p) => {
                    if password {
                        self.record(
                            event_from(
                                origin,
                                AuditEvent::new(AuditKind::LoginSucceeded, Outcome::Success),
                            )
                            .user(&p.name),
                        );
                    }
                    Ok(SignedIn::local(p.name, p.password_change_required))
                }
                Err(e @ LoginError::Locked { .. }) => {
                    self.record(named(
                        event_from(
                            origin,
                            AuditEvent::new(AuditKind::LoginLocked, Outcome::Failure),
                        )
                        .reason(e.to_string()),
                    ));
                    Err(SecurityError::new(
                        SecurityErrorKind::RateLimited,
                        e.to_string(),
                    ))
                }
                Err(e) => {
                    self.record(named(
                        event_from(
                            origin,
                            AuditEvent::new(AuditKind::LoginFailed, Outcome::Failure),
                        )
                        .reason(e.to_string()),
                    ));
                    Err(SecurityError::new(
                        SecurityErrorKind::Unauthenticated,
                        LOGIN_REFUSED,
                    ))
                }
            }
        }
        #[cfg(not(feature = "auth"))]
        {
            let _ = (origin, scheme, principal, credentials);
            Err(not_enabled())
        }
    }

    /// Log in with an identity provider's token. Like a session token it is
    /// audited only when refused, as every HTTP request carries it; the
    /// reason goes to the audit log, never to the client.
    #[cfg(feature = "oidc")]
    fn login_with_provider(
        &self,
        origin: &Origin,
        validator: &drevo_auth::oidc::TokenValidator,
        token: &str,
    ) -> Result<SignedIn, SecurityError> {
        use drevo_auth::audit::{AuditEvent, AuditKind, Outcome};
        match validator.validate(token) {
            Ok(principal) => Ok(SignedIn {
                name: principal.name,
                password_change_required: false,
                roles: Some(principal.roles),
            }),
            Err(e) => {
                self.record(
                    event_from(
                        origin,
                        AuditEvent::new(AuditKind::LoginFailed, Outcome::Failure),
                    )
                    .reason(format!("identity provider token: {e}")),
                );
                Err(SecurityError::new(
                    SecurityErrorKind::Unauthenticated,
                    LOGIN_REFUSED,
                ))
            }
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
        origin: &Origin,
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
                    self.login(origin, Some("basic"), Some(&username), Some(&password))
                }
                Credentials::Token(token) => self.login(origin, Some("bearer"), None, Some(&token)),
            }
        }
        #[cfg(not(feature = "auth"))]
        {
            let _ = (origin, authorization, cookie_token);
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

    /// [`execute`](Self::execute), recording what it changed — or why it was
    /// refused — in the audit log (#637). The command's text is never
    /// recorded: it can carry passwords.
    ///
    /// # Errors
    ///
    /// As [`execute`](Self::execute).
    pub fn execute_from(
        &self,
        origin: &Origin,
        command: &SecurityCommand,
        current_user: Option<&SignedIn>,
        param: &dyn Fn(&str) -> Option<ParamValue>,
    ) -> Result<ExecResult, SecurityError> {
        let result = self.execute(command, current_user, param);
        #[cfg(feature = "auth")]
        self.record_command(
            origin,
            command,
            current_user.map(|u| u.name.as_str()),
            &result,
        );
        #[cfg(not(feature = "auth"))]
        let _ = origin;
        result
    }

    #[cfg(feature = "auth")]
    fn record_command(
        &self,
        origin: &Origin,
        command: &SecurityCommand,
        current_user: Option<&str>,
        result: &Result<ExecResult, SecurityError>,
    ) {
        use drevo_auth::audit::{AuditEvent, Outcome};
        if let Err(e) = result {
            if e.kind == SecurityErrorKind::Forbidden {
                self.record_forbidden(origin, current_user, &e.message, None);
                return;
            }
        }
        let Some((kind, target)) = audited_change(command, current_user) else {
            return;
        };
        let outcome = if result.is_ok() {
            Outcome::Success
        } else {
            Outcome::Failure
        };
        let mut event = event_from(origin, AuditEvent::new(kind, outcome));
        if let Some(user) = current_user {
            event = event.user(user);
        }
        if let Some(target) = target {
            event = event.target(target);
        }
        if let Err(e) = result {
            event = event.reason(e.message.clone());
        }
        self.record(event);
    }

    /// Record the end of `user`'s session.
    pub fn record_logout(&self, origin: &Origin, user: &str) {
        #[cfg(feature = "auth")]
        {
            use drevo_auth::audit::{AuditEvent, AuditKind, Outcome};
            self.record(
                event_from(origin, AuditEvent::new(AuditKind::Logout, Outcome::Success)).user(user),
            );
        }
        #[cfg(not(feature = "auth"))]
        let _ = (origin, user);
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
        current_user: Option<&SignedIn>,
        param: &dyn Fn(&str) -> Option<ParamValue>,
    ) -> Result<ExecResult, SecurityError> {
        #[cfg(feature = "auth")]
        {
            let Some(users) = &self.users else {
                return Err(not_enabled());
            };
            let name = current_user.map(|u| u.name.as_str());
            // With logins required, managing users, roles and privileges
            // needs Neo4j's DBMS privileges; looking at and changing oneself
            // does not.
            if let (Some(context), Some(action)) = (
                self.context_for(current_user, ""),
                required_privilege(command, name),
            ) {
                if !context.has_dbms(action) {
                    return Err(SecurityError::new(
                        SecurityErrorKind::Forbidden,
                        format!("this command needs {}", action.keyword()),
                    ));
                }
            }
            // An identity provider's user has no record here: the commands
            // about oneself answer from the token, never from a local user of
            // the same name.
            if let Some(me) = current_user.filter(|u| u.is_external()) {
                if let Some(answer) = run_external(users, command, me) {
                    return answer;
                }
            }
            run(users, command, name, param)
        }
        #[cfg(not(feature = "auth"))]
        {
            let _ = (command, current_user, param);
            Err(not_enabled())
        }
    }
}

/// `event` with `origin`'s protocol and client address.
#[cfg(feature = "auth")]
fn event_from(
    origin: &Origin,
    event: drevo_auth::audit::AuditEvent,
) -> drevo_auth::audit::AuditEvent {
    let mut event = if origin.surface.is_empty() {
        event
    } else {
        event.surface(origin.surface)
    };
    if let Some(client) = &origin.client {
        event = event.client(client.clone());
    }
    event
}

/// What a user, role or privilege command changes, for the audit log: the
/// event kind and what it acts on. `None` for commands that only look.
#[cfg(feature = "auth")]
fn audited_change(
    command: &SecurityCommand,
    current_user: Option<&str>,
) -> Option<(drevo_auth::audit::AuditKind, Option<String>)> {
    use drevo_auth::audit::AuditKind;
    Some(match command {
        SecurityCommand::CreateUser { name, .. } | SecurityCommand::DropUser { name, .. } => {
            (AuditKind::UserChanged, Some(name.clone()))
        }
        SecurityCommand::AlterUser { name, password, .. } => (
            if password.is_some() {
                AuditKind::PasswordChanged
            } else {
                AuditKind::UserChanged
            },
            Some(name.clone()),
        ),
        SecurityCommand::AlterCurrentUserPassword { .. } => {
            (AuditKind::PasswordChanged, current_user.map(str::to_string))
        }
        SecurityCommand::CreateRole { name, .. } | SecurityCommand::DropRole { name, .. } => {
            (AuditKind::RoleChanged, Some(name.clone()))
        }
        SecurityCommand::RenameRole { from, .. } => (AuditKind::RoleChanged, Some(from.clone())),
        SecurityCommand::GrantRoles { roles, .. } | SecurityCommand::RevokeRoles { roles, .. } => {
            (AuditKind::RoleChanged, Some(roles.join(",")))
        }
        SecurityCommand::Privilege(c) => (AuditKind::PrivilegeChanged, Some(c.roles.join(","))),
        SecurityCommand::ShowUsers
        | SecurityCommand::ShowCurrentUser
        | SecurityCommand::ShowRoles { .. }
        | SecurityCommand::ShowPrivileges { .. }
        | SecurityCommand::ShowSupportedPrivileges => return None,
    })
}

/// The DBMS privilege `command` needs from `current_user`, or `None` when it
/// needs none: the commands about the user themselves.
#[cfg(feature = "auth")]
fn required_privilege(
    command: &SecurityCommand,
    current_user: Option<&str>,
) -> Option<drevo_auth::privileges::Action> {
    use crate::cypher::security_admin::PrivilegeScope;
    use drevo_auth::privileges::Action;
    match command {
        SecurityCommand::ShowCurrentUser
        | SecurityCommand::AlterCurrentUserPassword { .. }
        | SecurityCommand::ShowSupportedPrivileges => None,
        SecurityCommand::CreateUser { .. }
        | SecurityCommand::AlterUser { .. }
        | SecurityCommand::DropUser { .. }
        | SecurityCommand::ShowUsers => Some(Action::UserManagement),
        SecurityCommand::CreateRole { .. }
        | SecurityCommand::DropRole { .. }
        | SecurityCommand::RenameRole { .. }
        | SecurityCommand::GrantRoles { .. }
        | SecurityCommand::RevokeRoles { .. }
        | SecurityCommand::ShowRoles { .. } => Some(Action::RoleManagement),
        SecurityCommand::ShowPrivileges {
            scope: PrivilegeScope::CurrentUser,
            ..
        } => None,
        SecurityCommand::ShowPrivileges {
            scope: PrivilegeScope::Users(names),
            ..
        } if current_user.is_some_and(|me| names.iter().all(|n| n == me)) => None,
        SecurityCommand::Privilege(_) | SecurityCommand::ShowPrivileges { .. } => {
            Some(Action::PrivilegeManagement)
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

/// The commands about oneself for an identity provider's user `me`, or
/// `None` for the others, which run as for anyone.
#[cfg(feature = "auth")]
fn run_external(
    users: &drevo_auth::users::AuthManager,
    command: &SecurityCommand,
    me: &SignedIn,
) -> Option<Result<ExecResult, SecurityError>> {
    use crate::cypher::security_admin::PrivilegeScope;
    let roles = me.roles.clone().unwrap_or_default();
    match command {
        SecurityCommand::ShowCurrentUser => {
            let all = std::iter::once(drevo_auth::privileges::PUBLIC.to_string())
                .chain(roles.iter().cloned());
            let row = vec![
                Value::String(me.name.clone()),
                Value::List(all.map(Value::String).collect()),
                Value::Bool(false),
                Value::Bool(false),
                Value::Null,
            ];
            Some(Ok(result(&USER_COLUMNS, vec![row])))
        }
        SecurityCommand::AlterCurrentUserPassword { .. } => Some(Err(SecurityError::new(
            SecurityErrorKind::InvalidArgument,
            format!(
                "`{}` signs in with the identity provider and has no password here",
                me.name
            ),
        ))),
        SecurityCommand::ShowPrivileges {
            scope: PrivilegeScope::CurrentUser,
            as_commands,
        } => Some(own_privileges(users, me, roles, *as_commands)),
        SecurityCommand::ShowPrivileges {
            scope: PrivilegeScope::Users(names),
            as_commands,
        } if names.iter().all(|n| *n == me.name) => {
            Some(own_privileges(users, me, roles, *as_commands))
        }
        _ => None,
    }
}

/// `SHOW USER PRIVILEGES` for an identity provider's user: those of the
/// token's roles.
#[cfg(feature = "auth")]
fn own_privileges(
    users: &drevo_auth::users::AuthManager,
    me: &SignedIn,
    roles: Vec<String>,
    as_commands: Option<bool>,
) -> Result<ExecResult, SecurityError> {
    let entries = users
        .privileges(&drevo_auth::users::PrivilegeFilter::Roles(roles))
        .into_iter()
        .map(|e| (e, Some(me.name.clone())))
        .collect();
    Ok(privilege_rows(entries, as_commands, true))
}

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
        SecurityCommand::CreateRole {
            name,
            if_not_exists,
            copy_of,
        } => users
            .create_role(name, *if_not_exists, copy_of.as_deref())
            .map_err(manage_error)?,
        SecurityCommand::DropRole { name, if_exists } => {
            users.drop_role(name, *if_exists).map_err(manage_error)?;
        }
        SecurityCommand::RenameRole {
            from,
            to,
            if_exists,
        } => users
            .rename_role(from, to, *if_exists)
            .map_err(manage_error)?,
        SecurityCommand::GrantRoles {
            roles,
            users: names,
        }
        | SecurityCommand::RevokeRoles {
            roles,
            users: names,
        } => {
            // Check every name first, so a typo changes nothing.
            require_roles(users, roles)?;
            for name in names {
                if users.user(name).is_none() {
                    return Err(manage_error(drevo_auth::users::ManageError::UserNotFound(
                        name.clone(),
                    )));
                }
            }
            let grant = matches!(command, SecurityCommand::GrantRoles { .. });
            for role in roles {
                for name in names {
                    if grant {
                        users.grant_role(role, name)
                    } else {
                        users.revoke_role(role, name)
                    }
                    .map_err(manage_error)?;
                }
            }
        }
        SecurityCommand::ShowRoles {
            populated,
            with_users,
        } => return Ok(show_roles(users, *populated, *with_users)),
        SecurityCommand::Privilege(privilege) => {
            use drevo_auth::privileges::{Access, Op};
            require_roles(users, &privilege.roles)?;
            for role in &privilege.roles {
                match privilege.op {
                    Op::Grant => users.grant(Access::Grant, &privilege.privileges, role),
                    Op::Deny => users.grant(Access::Deny, &privilege.privileges, role),
                    Op::Revoke(access) => users.revoke(access, &privilege.privileges, role),
                }
                .map_err(manage_error)?;
            }
        }
        SecurityCommand::ShowPrivileges { scope, as_commands } => {
            return show_privileges(users, scope, *as_commands, current_user);
        }
        SecurityCommand::ShowSupportedPrivileges => return Ok(supported_privileges()),
    }
    Ok(result(&[], Vec::new()))
}

/// Refuse the command unless every role in `roles` exists.
#[cfg(feature = "auth")]
fn require_roles(
    users: &drevo_auth::users::AuthManager,
    roles: &[String],
) -> Result<(), SecurityError> {
    let known: std::collections::BTreeSet<String> =
        users.roles().into_iter().map(|r| r.name).collect();
    match roles.iter().find(|r| !known.contains(*r)) {
        Some(missing) => Err(manage_error(drevo_auth::users::ManageError::RoleNotFound(
            missing.clone(),
        ))),
        None => Ok(()),
    }
}

/// `SHOW ROLES`: one row per role, or per role and member `WITH USERS` (a
/// role without members has a null member).
#[cfg(feature = "auth")]
fn show_roles(
    users: &drevo_auth::users::AuthManager,
    populated: bool,
    with_users: bool,
) -> ExecResult {
    let roles = users
        .roles()
        .into_iter()
        .filter(|r| !populated || !r.users.is_empty());
    if !with_users {
        let rows = roles.map(|r| vec![Value::String(r.name)]).collect();
        return result(&["role"], rows);
    }
    let mut rows = Vec::new();
    for role in roles {
        if role.users.is_empty() {
            rows.push(vec![Value::String(role.name.clone()), Value::Null]);
        }
        for member in role.users {
            rows.push(vec![
                Value::String(role.name.clone()),
                Value::String(member),
            ]);
        }
    }
    result(&["role", "member"], rows)
}

#[cfg(feature = "auth")]
const PRIVILEGE_COLUMNS: [&str; 7] = [
    "access",
    "action",
    "resource",
    "graph",
    "segment",
    "role",
    "immutable",
];

/// `SHOW … PRIVILEGES`: Neo4j's columns, with `user` for user privileges;
/// or one `command` per privilege `AS [REVOKE] COMMANDS`.
#[cfg(feature = "auth")]
fn show_privileges(
    users: &drevo_auth::users::AuthManager,
    scope: &crate::cypher::security_admin::PrivilegeScope,
    as_commands: Option<bool>,
    current_user: Option<&str>,
) -> Result<ExecResult, SecurityError> {
    use crate::cypher::security_admin::PrivilegeScope;
    use drevo_auth::users::PrivilegeFilter;

    // (entry, user) pairs; `user` is set for user privileges.
    let entries: Vec<(drevo_auth::privileges::Entry, Option<String>)> = match scope {
        PrivilegeScope::All => users
            .privileges(&PrivilegeFilter::All)
            .into_iter()
            .map(|e| (e, None))
            .collect(),
        PrivilegeScope::Roles(roles) => users
            .privileges(&PrivilegeFilter::All)
            .into_iter()
            .filter(|e| roles.contains(&e.role))
            .map(|e| (e, None))
            .collect(),
        PrivilegeScope::Users(names) => names
            .iter()
            .flat_map(|name| {
                users
                    .privileges(&PrivilegeFilter::User(name.clone()))
                    .into_iter()
                    .map(move |e| (e, Some(name.clone())))
            })
            .collect(),
        PrivilegeScope::CurrentUser => {
            let Some(name) = current_user else {
                return Err(SecurityError::new(
                    SecurityErrorKind::Unauthenticated,
                    "SHOW USER PRIVILEGES needs a signed-in user",
                ));
            };
            users
                .privileges(&PrivilegeFilter::User(name.to_string()))
                .into_iter()
                .map(|e| (e, Some(name.to_string())))
                .collect()
        }
    };
    let with_user = matches!(
        scope,
        PrivilegeScope::Users(_) | PrivilegeScope::CurrentUser
    );
    Ok(privilege_rows(entries, as_commands, with_user))
}

/// Privilege entries as `SHOW … PRIVILEGES` rows, with a `user` column for
/// user privileges; or as commands `AS [REVOKE] COMMANDS`.
#[cfg(feature = "auth")]
fn privilege_rows(
    entries: Vec<(drevo_auth::privileges::Entry, Option<String>)>,
    as_commands: Option<bool>,
    with_user: bool,
) -> ExecResult {
    if let Some(revoke) = as_commands {
        let rows = entries
            .iter()
            .map(|(e, _)| {
                vec![Value::String(if revoke {
                    e.to_revoke_command()
                } else {
                    e.to_command()
                })]
            })
            .collect();
        return result(&["command"], rows);
    }
    let rows = entries
        .into_iter()
        .map(|(e, user)| {
            let p = &e.privilege;
            let mut row = vec![
                Value::String(e.access.name().to_string()),
                Value::String(p.action.name().to_string()),
                Value::String(p.resource_name()),
                Value::String(p.graph_name()),
                Value::String(p.segment_name()),
                Value::String(e.role.clone()),
                Value::Bool(false),
            ];
            if with_user {
                row.push(user.map_or(Value::Null, Value::String));
            }
            row
        })
        .collect();
    let mut columns = PRIVILEGE_COLUMNS.to_vec();
    if with_user {
        columns.push("user");
    }
    result(&columns, rows)
}

/// `SHOW SUPPORTED PRIVILEGES`: every privilege this server reads, with what
/// it takes and where it applies.
#[cfg(feature = "auth")]
fn supported_privileges() -> ExecResult {
    use drevo_auth::privileges::{Action, Level};
    let rows = Action::ALL
        .iter()
        .map(|action| {
            let qualifier = match action {
                Action::Traverse | Action::Create | Action::Delete => "NODES / RELATIONSHIPS",
                Action::Read | Action::Match | Action::SetProperty | Action::Merge => {
                    "{properties} NODES / RELATIONSHIPS"
                }
                Action::SetLabel | Action::RemoveLabel => "labels",
                Action::TransactionManagement => "(users)",
                Action::ExecuteProcedure => "procedures",
                Action::ExecuteFunction => "functions",
                _ => "",
            };
            let (target, scope) = match action.level() {
                Level::Graph => ("graph", "ON GRAPH * | name | HOME GRAPH"),
                Level::Database => ("database", "ON DATABASE * | name | HOME DATABASE"),
                Level::Dbms => ("dbms", "ON DBMS"),
            };
            let keyword = action.keyword().to_ascii_lowercase();
            vec![
                Value::String(keyword.clone()),
                Value::String(qualifier.to_string()),
                Value::String(target.to_string()),
                Value::String(scope.to_string()),
                Value::String(format!("{} privilege `{}`", target, action.name())),
            ]
        })
        .collect();
    result(
        &["action", "qualifier", "target", "scope", "description"],
        rows,
    )
}

#[cfg(feature = "auth")]
fn user_row(u: &drevo_auth::users::UserInfo) -> Vec<Value> {
    vec![
        Value::String(u.name.clone()),
        // PUBLIC, then the roles given.
        Value::List(u.roles.iter().cloned().map(Value::String).collect()),
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
        | M::InvalidRoleName { .. }
        | M::PublicRole { .. }
        | M::WeakPassword { .. }
        | M::SamePassword
        | M::WrongCurrentPassword => SecurityErrorKind::InvalidArgument,
        M::UserExists(_) | M::RoleExists(_) => SecurityErrorKind::AlreadyExists,
        M::UserNotFound(_) | M::RoleNotFound(_) => SecurityErrorKind::NotFound,
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
