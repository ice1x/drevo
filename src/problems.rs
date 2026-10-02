//! Recent server problems, for the Web UI's notifications and problem reports
//! (#552 slice A).
//!
//! A `ProblemLayer` installed on the server's `tracing` subscriber copies
//! every WARN and ERROR event into a bounded in-memory
//! `ProblemLog` ring
//! buffer. `GET /problems?since=<seq>` serves it incrementally, so the UI can
//! poll for anything new. Entries are redacted on the way in: fields whose
//! name looks like a credential, and `sk-…` API-key-shaped substrings
//! anywhere, never reach the buffer.
//!
//! Statement failures that are the server's problem rather than the
//! client's — a statement timeout (#547), a storage error — are reported
//! through `note_exec_error`. It logs them at ERROR under the
//! `drevo::query` target with the query text, so they land both in the
//! server log and in the feed, and it counts timeouts for
//! `drevo_statement_timeouts_total`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

use crate::cypher::executor::ExecError;

/// How many problems the process-wide feed keeps.
const GLOBAL_CAPACITY: usize = 200;

/// Query text longer than this is cut (with a trailing `…`) before logging.
const MAX_QUERY_CHARS: usize = 2000;

/// One captured WARN / ERROR event.
#[derive(Debug, Clone, Serialize)]
pub struct Problem {
    /// Monotonic sequence number, unique within the process.
    pub seq: u64,
    /// When it happened, ISO-8601 UTC with milliseconds.
    pub at: String,
    /// `"ERROR"` or `"WARN"`.
    pub level: String,
    /// The `tracing` target (module path, or e.g. `drevo::query`).
    pub target: String,
    /// The event's message.
    pub message: String,
    /// The event's other fields, rendered as strings.
    pub fields: BTreeMap<String, String>,
}

/// A bounded, thread-safe ring buffer of recent [`Problem`]s.
#[derive(Debug)]
pub struct ProblemLog {
    capacity: usize,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    entries: VecDeque<Problem>,
    next_seq: u64,
}

impl ProblemLog {
    /// An empty log keeping at most `capacity` problems (at least one).
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The process-wide feed that the server's tracing layer fills and
    /// `GET /problems` serves.
    pub fn global() -> Arc<ProblemLog> {
        static GLOBAL: OnceLock<Arc<ProblemLog>> = OnceLock::new();
        Arc::clone(GLOBAL.get_or_init(|| Arc::new(ProblemLog::new(GLOBAL_CAPACITY))))
    }

    /// Append a problem, redacting secrets, and return its sequence number.
    /// The oldest entry is dropped once the log is full.
    pub fn record(
        &self,
        level: &str,
        target: &str,
        message: &str,
        fields: impl IntoIterator<Item = (String, String)>,
    ) -> u64 {
        let fields = fields
            .into_iter()
            .map(|(name, value)| {
                let value = if is_secret_field(&name) {
                    "<redacted>".to_string()
                } else {
                    redact_keys(&value)
                };
                (name, value)
            })
            .collect();
        let at = crate::cypher::executor::iso8601_utc(now_epoch_ms());
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.entries.len() == self.capacity {
            inner.entries.pop_front();
        }
        inner.entries.push_back(Problem {
            seq,
            at,
            level: level.to_string(),
            target: target.to_string(),
            message: redact_keys(message),
            fields,
        });
        seq
    }

    /// Every retained problem with `seq >= since`, oldest first, plus the
    /// cursor to pass next time.
    pub fn since(&self, since: u64) -> (Vec<Problem>, u64) {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let newer = inner
            .entries
            .iter()
            .filter(|p| p.seq >= since)
            .cloned()
            .collect();
        (newer, inner.next_seq)
    }
}

/// A `tracing` layer copying WARN and ERROR events into a [`ProblemLog`].
#[derive(Debug, Clone)]
pub struct ProblemLayer {
    log: Arc<ProblemLog>,
}

impl ProblemLayer {
    /// A layer feeding `log`.
    pub fn new(log: Arc<ProblemLog>) -> Self {
        Self { log }
    }

    /// A layer feeding the process-wide [`ProblemLog::global`] feed.
    pub fn global() -> Self {
        Self::new(ProblemLog::global())
    }
}

impl<S: Subscriber> Layer<S> for ProblemLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        // `Level` orders by verbosity: everything above WARN is chatter.
        if *meta.level() > Level::WARN {
            return;
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.log.record(
            meta.level().as_str(),
            meta.target(),
            &visitor.message,
            visitor.fields,
        );
    }
}

#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.push(field, format!("{value:?}"));
    }
}

impl FieldVisitor {
    fn push(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else {
            self.fields.push((field.name().to_string(), value));
        }
    }
}

static STATEMENT_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

/// How many statements have hit the statement timeout in this process.
pub fn statement_timeouts() -> u64 {
    STATEMENT_TIMEOUTS.load(Ordering::Relaxed)
}

/// Report a failed statement if it is the server's problem. A statement
/// timeout or a storage failure is logged at ERROR (target `drevo::query`)
/// with the protocol, database, and truncated query text, and a timeout is
/// counted. Client mistakes such as syntax errors, unknown variables or
/// missing parameters are the caller's business and are not logged.
pub fn note_exec_error(protocol: &str, database: &str, query: &str, err: &ExecError) {
    match err {
        ExecError::Timeout { limit_ms } => {
            STATEMENT_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                target: "drevo::query",
                protocol,
                database,
                limit_ms,
                query = %truncate(query),
                "statement exceeded the statement timeout"
            );
        }
        ExecError::Storage(e) => {
            tracing::error!(
                target: "drevo::query",
                protocol,
                database,
                error = %e,
                query = %truncate(query),
                "statement failed in storage"
            );
        }
        _ => {}
    }
}

fn truncate(query: &str) -> String {
    if query.chars().count() <= MAX_QUERY_CHARS {
        return query.to_string();
    }
    let mut cut: String = query.chars().take(MAX_QUERY_CHARS).collect();
    cut.push('…');
    cut
}

/// Field names that carry credentials and are never stored.
fn is_secret_field(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "key",
        "token",
        "secret",
        "password",
        "authorization",
        "credential",
    ]
    .iter()
    .any(|needle| name.contains(needle))
}

/// Mask `sk-…` API-key-shaped substrings (OpenAI style: `sk-` then at least
/// eight `[A-Za-z0-9_-]`).
fn redact_keys(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("sk-") {
        let tail = &rest[at + 3..];
        let run = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(tail.len());
        out.push_str(&rest[..at]);
        if run >= 8 {
            out.push_str("sk-<redacted>");
        } else {
            out.push_str(&rest[at..at + 3 + run]);
        }
        rest = &tail[run..];
    }
    out.push_str(rest);
    out
}

fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_keys_masks_only_key_shaped_runs() {
        assert_eq!(redact_keys("sk-abcdefgh1"), "sk-<redacted>");
        assert_eq!(redact_keys("a sk-abc b"), "a sk-abc b");
        assert_eq!(
            redact_keys("x sk-12345678 y sk-q"),
            "x sk-<redacted> y sk-q"
        );
        assert_eq!(redact_keys("no keys"), "no keys");
    }

    #[test]
    fn truncate_counts_chars_not_bytes() {
        let s = "ё".repeat(MAX_QUERY_CHARS + 5);
        let t = truncate(&s);
        assert_eq!(t.chars().count(), MAX_QUERY_CHARS + 1);
        assert!(t.ends_with('…'));
        assert_eq!(truncate("short"), "short");
    }
}
