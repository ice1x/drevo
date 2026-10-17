//! The security audit log: events of a fixed shape, written as JSON lines to
//! a file that rotates by size, to stdout, or to memory.
#![cfg(feature = "password")]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_auth::audit::{AuditEvent, AuditKind, AuditSink, JsonLinesFile, MemorySink, Outcome};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_auth_audit_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn file(&self) -> PathBuf {
        self.0.join("audit.log")
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn failed_login() -> AuditEvent {
    AuditEvent::new(AuditKind::LoginFailed, Outcome::Failure)
        .at(1_760_659_200_123)
        .user("ann")
        .client("10.0.0.7:51234")
        .surface("bolt")
        .reason("invalid principal or credentials")
}

#[test]
fn an_event_is_one_json_line_with_named_fields() {
    let line = failed_login().to_json_line();
    assert!(!line.contains('\n'));
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["time"], "2025-10-17T00:00:00.123Z");
    assert_eq!(v["event"], "login_failed");
    assert_eq!(v["outcome"], "failure");
    assert_eq!(v["user"], "ann");
    assert_eq!(v["client"], "10.0.0.7:51234");
    assert_eq!(v["surface"], "bolt");
    assert_eq!(v["reason"], "invalid principal or credentials");
    // Fields that were not set are left out.
    assert!(v.get("target").is_none());
    assert!(v.get("query").is_none());
}

#[test]
fn every_kind_has_a_stable_name() {
    let names: Vec<&str> = AuditKind::ALL.iter().map(|k| k.name()).collect();
    assert_eq!(
        names,
        [
            "login_succeeded",
            "login_failed",
            "login_locked",
            "logout",
            "password_changed",
            "user_changed",
            "role_changed",
            "privilege_changed",
            "forbidden",
        ]
    );
}

#[test]
fn text_that_needs_escaping_stays_one_line() {
    let line = AuditEvent::new(AuditKind::Forbidden, Outcome::Failure)
        .user("we\"ird\nname")
        .reason("tab\there, back\\slash, \u{1}control")
        .query("MATCH (n) RETURN n")
        .to_json_line();
    assert!(!line.contains('\n'));
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["user"], "we\"ird\nname");
    assert_eq!(v["reason"], "tab\there, back\\slash, \u{1}control");
    assert_eq!(v["query"], "MATCH (n) RETURN n");
}

#[test]
fn a_memory_sink_keeps_events_in_order() {
    let sink = MemorySink::new();
    sink.record(&failed_login());
    sink.record(&AuditEvent::new(AuditKind::LoginSucceeded, Outcome::Success).user("ann"));
    let events = sink.events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, AuditKind::LoginFailed);
    assert_eq!(events[1].kind, AuditKind::LoginSucceeded);
}

#[test]
fn a_file_sink_appends_json_lines() {
    let dir = TmpDir::new();
    let sink = JsonLinesFile::open(dir.file(), 1 << 20, 3).unwrap();
    sink.record(&failed_login());
    sink.record(&AuditEvent::new(AuditKind::Logout, Outcome::Success).user("ann"));
    let text = fs::read_to_string(dir.file()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    for line in lines {
        serde_json::from_str::<serde_json::Value>(line).unwrap();
    }
    // Reopening appends rather than truncating.
    drop(sink);
    let sink = JsonLinesFile::open(dir.file(), 1 << 20, 3).unwrap();
    sink.record(&failed_login());
    assert_eq!(fs::read_to_string(dir.file()).unwrap().lines().count(), 3);
}

#[test]
fn a_full_file_rotates_and_old_files_are_dropped() {
    let dir = TmpDir::new();
    let line_len = failed_login().to_json_line().len() as u64 + 1;
    // Room for two events per file, three old files kept.
    let sink = JsonLinesFile::open(dir.file(), line_len * 2, 3).unwrap();
    for _ in 0..12 {
        sink.record(&failed_login());
    }
    let count = |p: PathBuf| {
        fs::read_to_string(p)
            .map(|t| t.lines().count())
            .unwrap_or(0)
    };
    assert!(count(dir.file()) <= 2);
    for i in 1..=3 {
        let rotated = dir.0.join(format!("audit.log.{i}"));
        assert_eq!(count(rotated), 2, "audit.log.{i}");
    }
    assert!(
        !dir.0.join("audit.log.4").exists(),
        "only three old files are kept"
    );
}

#[cfg(unix)]
#[test]
fn the_file_is_readable_by_its_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TmpDir::new();
    let sink = JsonLinesFile::open(dir.file(), 1 << 20, 1).unwrap();
    sink.record(&failed_login());
    let mode = fs::metadata(dir.file()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn an_unwritable_location_is_an_error_at_open() {
    let dir = TmpDir::new();
    let missing = dir.0.join("no-such-dir").join("audit.log");
    assert!(JsonLinesFile::open(missing, 1 << 20, 1).is_err());
}

#[test]
fn the_fields_have_no_room_for_a_password() {
    // AuditEvent has no password, token or credentials field: the builder is
    // the whole surface. A reason or query is free text, so the server keeps
    // secrets out of them (tested in drevo's audit tests).
    let line = AuditEvent::new(AuditKind::PasswordChanged, Outcome::Success)
        .user("ann")
        .target("ann")
        .to_json_line();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["event", "outcome", "target", "time", "user"]);
}
