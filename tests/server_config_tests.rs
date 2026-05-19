//! Tests for the server binary's environment-variable configuration
//! parser (audit task `00112`).
//!
//! The pre-`00112` server binary inlined all env-var parsing and
//! validation inside `main()`, which made the four `expect()` calls
//! at startup untestable — the only way to exercise a malformed
//! `DREVO_PORT` was to launch the binary as a subprocess. Task
//! `00112` extracts that logic into [`drevo::server::Config`] so each
//! validation rule lives behind a unit test.
//!
//! Rules verified:
//! - `drevo-rust` §"Error Handling" — _"Never `unwrap()` / `expect()`
//!   in library code"._ The server module replaces every `expect()`
//!   with a typed [`drevo::server::ConfigError`].
//! - `drevo-tdd` §"every public function — at least 1 test".
//! - README task `00112` — _"Env-var parsing: `DREVO_PORT` bounds
//!   (u16, 1024+ recommended in container); `DREVO_DATA_DIR` path
//!   validation"._

#![cfg(feature = "http")]

use std::collections::HashMap;

use drevo::server::{Config, ConfigError};

/// Build a closure compatible with [`Config::from_env`] from a map.
fn getter(map: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
    move |key: &str| map.get(key).map(|v| (*v).to_string())
}

// ---------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------

#[test]
fn from_env_defaults_when_nothing_is_set() {
    let cfg = Config::from_env(getter(HashMap::new())).expect("defaults must parse");
    assert_eq!(cfg.host, "0.0.0.0");
    assert_eq!(cfg.port, 8080);
    assert_eq!(cfg.data_dir.to_string_lossy(), "/data");
}

#[test]
fn db_path_appends_drevo_redb_filename() {
    let cfg = Config::from_env(getter(HashMap::new())).unwrap();
    assert_eq!(cfg.db_path().file_name().unwrap(), "drevo.redb");
    // Parent must be the data_dir.
    assert_eq!(cfg.db_path().parent().unwrap(), cfg.data_dir);
}

// ---------------------------------------------------------------------
// Overrides — happy path
// ---------------------------------------------------------------------

#[test]
fn drevo_host_override_is_honoured() {
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_HOST", "127.0.0.1")]))).unwrap();
    assert_eq!(cfg.host, "127.0.0.1");
}

#[test]
fn drevo_port_override_is_honoured() {
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_PORT", "9090")]))).unwrap();
    assert_eq!(cfg.port, 9090);
}

#[test]
fn drevo_data_dir_override_is_honoured() {
    let cfg = Config::from_env(getter(HashMap::from([(
        "DREVO_DATA_DIR",
        "/var/lib/drevo",
    )])))
    .unwrap();
    assert_eq!(cfg.data_dir.to_string_lossy(), "/var/lib/drevo");
    assert_eq!(cfg.db_path().to_string_lossy(), "/var/lib/drevo/drevo.redb");
}

#[test]
fn config_yields_a_socket_addr_that_parses() {
    let cfg = Config::from_env(getter(HashMap::from([
        ("DREVO_HOST", "127.0.0.1"),
        ("DREVO_PORT", "9090"),
    ])))
    .unwrap();
    let addr = cfg.socket_addr().unwrap();
    assert_eq!(addr.port(), 9090);
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
}

#[test]
fn config_socket_addr_supports_ipv6() {
    let cfg = Config::from_env(getter(HashMap::from([
        ("DREVO_HOST", "::1"),
        ("DREVO_PORT", "8081"),
    ])))
    .unwrap();
    let addr = cfg.socket_addr().unwrap();
    assert!(addr.is_ipv6());
    assert_eq!(addr.port(), 8081);
}

// ---------------------------------------------------------------------
// Port validation
// ---------------------------------------------------------------------

#[test]
fn port_zero_is_rejected() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_PORT", "0")]))).unwrap_err();
    assert!(
        matches!(err, ConfigError::InvalidPort { .. }),
        "expected InvalidPort, got {err:?}"
    );
}

#[test]
fn port_above_u16_is_rejected() {
    // 65536 overflows u16 — parse error.
    let err = Config::from_env(getter(HashMap::from([("DREVO_PORT", "65536")]))).unwrap_err();
    assert!(matches!(err, ConfigError::InvalidPort { .. }));
}

#[test]
fn port_negative_is_rejected() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_PORT", "-1")]))).unwrap_err();
    assert!(matches!(err, ConfigError::InvalidPort { .. }));
}

#[test]
fn port_garbage_string_is_rejected() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_PORT", "abc")]))).unwrap_err();
    assert!(matches!(err, ConfigError::InvalidPort { .. }));
}

#[test]
fn port_empty_string_is_rejected() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_PORT", "")]))).unwrap_err();
    assert!(matches!(err, ConfigError::InvalidPort { .. }));
}

#[test]
fn port_at_max_u16_is_accepted() {
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_PORT", "65535")]))).unwrap();
    assert_eq!(cfg.port, 65535);
}

#[test]
fn port_one_is_accepted_but_privileged() {
    // 1..1024 is "privileged" — accepted (operator's choice) but
    // [`Config::is_privileged_port`] flags it so the binary can warn.
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_PORT", "80")]))).unwrap();
    assert_eq!(cfg.port, 80);
    assert!(cfg.is_privileged_port());
}

#[test]
fn port_1024_is_not_privileged() {
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_PORT", "1024")]))).unwrap();
    assert!(!cfg.is_privileged_port());
}

#[test]
fn port_default_8080_is_not_privileged() {
    let cfg = Config::from_env(getter(HashMap::new())).unwrap();
    assert!(!cfg.is_privileged_port());
}

// ---------------------------------------------------------------------
// Host validation
// ---------------------------------------------------------------------

#[test]
fn host_empty_string_is_rejected() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_HOST", "")]))).unwrap_err();
    assert!(
        matches!(err, ConfigError::InvalidHost { .. }),
        "expected InvalidHost, got {err:?}"
    );
}

#[test]
fn host_garbage_is_rejected_when_building_socket_addr() {
    // A non-IP host is accepted at Config construction (DNS names are
    // valid for `bind` in some platforms) but socket_addr() must fail
    // so the binary can report a precise error.
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_HOST", "not_an_ip")]))).unwrap();
    let err = cfg.socket_addr().unwrap_err();
    assert!(matches!(err, ConfigError::InvalidHost { .. }));
}

// ---------------------------------------------------------------------
// data_dir validation
// ---------------------------------------------------------------------

#[test]
fn data_dir_empty_string_is_rejected() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_DATA_DIR", "")]))).unwrap_err();
    assert!(
        matches!(err, ConfigError::InvalidDataDir { .. }),
        "expected InvalidDataDir, got {err:?}"
    );
}

#[test]
fn data_dir_absolute_path_is_accepted() {
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_DATA_DIR", "/srv/drevo")]))).unwrap();
    assert!(cfg.data_dir.is_absolute());
}

#[test]
fn data_dir_relative_path_is_accepted() {
    // Relative paths are permitted (useful for local dev) — the
    // validation only rejects the empty string. Container deployments
    // rely on the absolute `/data` default.
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_DATA_DIR", "./var/data")]))).unwrap();
    assert_eq!(cfg.data_dir.to_string_lossy(), "./var/data");
}

// ---------------------------------------------------------------------
// Error formatting (operator-facing UX)
// ---------------------------------------------------------------------

#[test]
fn invalid_port_error_includes_the_offending_value() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_PORT", "abc")]))).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("abc"),
        "error message should echo the value: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("port"),
        "error message should mention port: {msg}"
    );
}

#[test]
fn invalid_data_dir_error_mentions_data_dir() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_DATA_DIR", "")]))).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("data_dir") || msg.to_lowercase().contains("data dir"),
        "error message should mention data_dir: {msg}"
    );
}
