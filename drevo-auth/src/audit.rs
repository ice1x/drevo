//! The security audit log: who logged in, who was refused, and what changed
//! in the users, roles and privileges.
//!
//! An [`AuditEvent`] has a fixed set of fields — time, event kind, outcome,
//! user, client address, surface, target, reason and (optionally) query text
//! — and none that could carry a password or session token. An
//! [`AuditSink`] receives each event:
//!
//! - [`JsonLinesFile`] appends one JSON object per line to a file readable by
//!   its owner only, and rotates it by size (`audit.log` → `audit.log.1` →
//!   …), keeping a set number of old files;
//! - [`Stdout`] writes the same lines to standard output, for a container's
//!   log collector;
//! - [`MemorySink`] keeps events in memory, for tests.
//!
//! ```
//! use drevo_auth::audit::{AuditEvent, AuditKind, AuditSink, MemorySink, Outcome};
//!
//! let sink = MemorySink::new();
//! sink.record(
//!     &AuditEvent::new(AuditKind::LoginFailed, Outcome::Failure)
//!         .user("ann")
//!         .surface("bolt")
//!         .reason("invalid principal or credentials"),
//! );
//! let line = sink.events()[0].to_json_line();
//! assert!(line.contains(r#""event":"login_failed""#));
//! ```

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuditKind {
    /// A login with a password or session token was accepted.
    LoginSucceeded,
    /// A login was refused: unknown user, wrong password, bad token or
    /// suspended user.
    LoginFailed,
    /// A login was refused because the user is locked after failed logins.
    LoginLocked,
    /// A session was ended.
    Logout,
    /// A password was changed, by its user or by an administrator.
    PasswordChanged,
    /// A user was created, altered (other than its password) or dropped.
    UserChanged,
    /// A role was created, renamed or dropped, or given to or taken from a
    /// user.
    RoleChanged,
    /// A privilege was granted, denied or revoked.
    PrivilegeChanged,
    /// A statement or request was refused for a missing privilege.
    Forbidden,
}

impl AuditKind {
    /// Every kind, in the order they are listed above.
    pub const ALL: [AuditKind; 9] = [
        AuditKind::LoginSucceeded,
        AuditKind::LoginFailed,
        AuditKind::LoginLocked,
        AuditKind::Logout,
        AuditKind::PasswordChanged,
        AuditKind::UserChanged,
        AuditKind::RoleChanged,
        AuditKind::PrivilegeChanged,
        AuditKind::Forbidden,
    ];

    /// The `event` field: `login_failed`, `forbidden`, …
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::LoginSucceeded => "login_succeeded",
            Self::LoginFailed => "login_failed",
            Self::LoginLocked => "login_locked",
            Self::Logout => "logout",
            Self::PasswordChanged => "password_changed",
            Self::UserChanged => "user_changed",
            Self::RoleChanged => "role_changed",
            Self::PrivilegeChanged => "privilege_changed",
            Self::Forbidden => "forbidden",
        }
    }
}

/// Whether the audited action went ahead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// It did.
    Success,
    /// It was refused.
    Failure,
}

impl Outcome {
    /// The `outcome` field: `success` or `failure`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

/// One audited event. Built with [`new`](Self::new) and the builder methods;
/// a field left unset is left out of the log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    /// When, in Unix milliseconds.
    pub time_ms: i64,
    /// What happened.
    pub kind: AuditKind,
    /// Whether it went ahead.
    pub outcome: Outcome,
    /// The user who acted, or who tried to log in.
    pub user: Option<String>,
    /// The client's address (`host:port`).
    pub client: Option<String>,
    /// The protocol: `bolt`, `http`, `grpc`, `flight`.
    pub surface: Option<String>,
    /// What was acted on: the user whose password changed, the role granted.
    pub target: Option<String>,
    /// Why it was refused, or what was done.
    pub reason: Option<String>,
    /// The statement's text, for a refused statement when the server is set
    /// to record it.
    pub query: Option<String>,
}

impl AuditEvent {
    /// An event of `kind` with `outcome`, happening now.
    #[must_use]
    pub fn new(kind: AuditKind, outcome: Outcome) -> Self {
        Self {
            time_ms: now_ms(),
            kind,
            outcome,
            user: None,
            client: None,
            surface: None,
            target: None,
            reason: None,
            query: None,
        }
    }

    /// The same event at `time_ms` (Unix milliseconds).
    #[must_use]
    pub fn at(mut self, time_ms: i64) -> Self {
        self.time_ms = time_ms;
        self
    }

    /// The acting user.
    #[must_use]
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// The client's address.
    #[must_use]
    pub fn client(mut self, client: impl Into<String>) -> Self {
        self.client = Some(client.into());
        self
    }

    /// The protocol.
    #[must_use]
    pub fn surface(mut self, surface: impl Into<String>) -> Self {
        self.surface = Some(surface.into());
        self
    }

    /// What was acted on.
    #[must_use]
    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    /// Why it was refused, or what was done.
    #[must_use]
    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// The statement's text.
    #[must_use]
    pub fn query(mut self, query: impl Into<String>) -> Self {
        self.query = Some(query.into());
        self
    }

    /// The event as one line of JSON (no trailing newline), with `time` in
    /// ISO 8601 UTC: `{"event":"login_failed","outcome":"failure",…}`.
    #[must_use]
    pub fn to_json_line(&self) -> String {
        let mut map = serde_json::Map::new();
        map.insert("time".into(), iso8601_utc(self.time_ms).into());
        map.insert("event".into(), self.kind.name().into());
        map.insert("outcome".into(), self.outcome.name().into());
        for (key, value) in [
            ("user", &self.user),
            ("client", &self.client),
            ("surface", &self.surface),
            ("target", &self.target),
            ("reason", &self.reason),
            ("query", &self.query),
        ] {
            if let Some(v) = value {
                map.insert(key.into(), v.clone().into());
            }
        }
        serde_json::Value::Object(map).to_string()
    }
}

/// Where audit events go. A sink must not fail its caller: an event it
/// cannot write is reported on standard error and dropped.
pub trait AuditSink: Send + Sync {
    /// Record one event.
    fn record(&self, event: &AuditEvent);
}

/// Events kept in memory, in order.
#[derive(Debug, Default)]
pub struct MemorySink {
    events: Mutex<Vec<AuditEvent>>,
}

impl MemorySink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every event recorded so far.
    #[must_use]
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

impl AuditSink for MemorySink {
    fn record(&self, event: &AuditEvent) {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(event.clone());
    }
}

/// JSON lines on standard output.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stdout;

impl AuditSink for Stdout {
    fn record(&self, event: &AuditEvent) {
        let line = event.to_json_line();
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
    }
}

/// JSON lines appended to a file, rotated by size.
///
/// When writing the next line would take the file past `max_bytes`, the file
/// becomes `<path>.1`, the old `<path>.1` becomes `<path>.2`, and so on; the
/// oldest beyond `keep` is deleted. Each file is readable by its owner only
/// (mode `0600` on Unix).
#[derive(Debug)]
pub struct JsonLinesFile {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    state: Mutex<FileState>,
}

#[derive(Debug)]
struct FileState {
    file: File,
    size: u64,
}

impl JsonLinesFile {
    /// Append to `path` (creating it), rotating past `max_bytes` and keeping
    /// `keep` old files.
    ///
    /// # Errors
    ///
    /// The I/O error when the file cannot be opened.
    pub fn open(path: impl Into<PathBuf>, max_bytes: u64, keep: usize) -> std::io::Result<Self> {
        let path = path.into();
        let file = open_append(&path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path,
            max_bytes: max_bytes.max(1),
            keep,
            state: Mutex::new(FileState { file, size }),
        })
    }

    /// The file events are appended to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn rotated(&self, n: usize) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    fn rotate(&self, state: &mut FileState) -> std::io::Result<()> {
        if self.keep == 0 {
            // Keep nothing: start the file afresh.
            state.file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&self.path)?;
            state.size = 0;
            return Ok(());
        }
        let _ = std::fs::remove_file(self.rotated(self.keep));
        for n in (1..self.keep).rev() {
            let from = self.rotated(n);
            if from.exists() {
                std::fs::rename(&from, self.rotated(n + 1))?;
            }
        }
        std::fs::rename(&self.path, self.rotated(1))?;
        state.file = open_append(&self.path)?;
        state.size = 0;
        Ok(())
    }
}

impl AuditSink for JsonLinesFile {
    fn record(&self, event: &AuditEvent) {
        let mut line = event.to_json_line();
        line.push('\n');
        let len = line.len() as u64;
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.size > 0 && state.size + len > self.max_bytes {
            if let Err(e) = self.rotate(&mut state) {
                eprintln!("audit log: cannot rotate {}: {e}", self.path.display());
            }
        }
        match state.file.write_all(line.as_bytes()) {
            Ok(()) => state.size += len,
            Err(e) => eprintln!("audit log: cannot write {}: {e}", self.path.display()),
        }
    }
}

fn open_append(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// `ms` (Unix milliseconds) as ISO 8601 UTC with milliseconds:
/// `2025-10-17T00:00:00.123Z`.
fn iso8601_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

#[cfg(test)]
mod tests {
    use super::iso8601_utc;

    #[test]
    fn times_are_iso8601_utc_utc() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601_utc(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(iso8601_utc(1_760_659_200_123), "2025-10-17T00:00:00.123Z");
        assert_eq!(iso8601_utc(4_102_444_799_999), "2099-12-31T23:59:59.999Z");
    }
}
