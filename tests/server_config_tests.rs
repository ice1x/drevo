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
fn port_0_set_in_code_is_not_privileged() {
    // `DREVO_PORT=0` is refused, but a caller may set port 0 to have the
    // system pick one (see `server::run_announcing`); it is no privileged port.
    let mut cfg = Config::from_env(getter(HashMap::new())).unwrap();
    cfg.port = 0;
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

// ---------------------------------------------------------------------
// DREVO_HTTP_FORMATS (issue #581)
// ---------------------------------------------------------------------

#[test]
fn http_formats_default_to_json_only() {
    let cfg = Config::from_env(getter(HashMap::new())).expect("defaults");
    assert_eq!(
        cfg.http_formats.enabled(),
        &[drevo::wire_format::WireFormat::Json]
    );
}

#[test]
fn http_formats_reject_unknown_names() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_HTTP_FORMATS", "json,xml")])))
        .expect_err("xml is not a format");
    assert!(matches!(err, ConfigError::InvalidHttpFormats(_)), "{err}");
    assert!(err.to_string().contains("xml"), "{err}");
}

#[cfg(all(feature = "format-cbor", feature = "format-msgpack"))]
#[test]
fn http_formats_enable_compiled_formats() {
    use drevo::wire_format::WireFormat;
    let cfg = Config::from_env(getter(HashMap::from([(
        "DREVO_HTTP_FORMATS",
        "cbor, msgpack",
    )])))
    .expect("both compiled in");
    assert_eq!(
        cfg.http_formats.enabled(),
        &[WireFormat::Json, WireFormat::Cbor, WireFormat::MsgPack]
    );
}

#[cfg(not(feature = "format-cbor"))]
#[test]
fn http_formats_reject_a_format_built_without() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_HTTP_FORMATS", "cbor")])))
        .expect_err("not compiled in");
    assert!(err.to_string().contains("format-cbor"), "{err}");
}

// ---------------------------------------------------------------------
// DREVO_WAL_FORMAT (issue #582)
// ---------------------------------------------------------------------

#[test]
fn wal_format_defaults_to_json() {
    let cfg = Config::from_env(getter(HashMap::new())).expect("defaults");
    assert_eq!(cfg.wal_format, drevo::wal_format::WalFormat::Json);
}

#[test]
fn wal_format_rejects_unknown_names() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_WAL_FORMAT", "protobuf")])))
        .expect_err("not a WAL format");
    assert!(matches!(err, ConfigError::InvalidWalFormat(_)), "{err}");
}

#[cfg(all(feature = "format-cbor", feature = "format-msgpack"))]
#[test]
fn wal_format_accepts_compiled_formats() {
    use drevo::wal_format::WalFormat;
    for (raw, want) in [("cbor", WalFormat::Cbor), (" MsgPack ", WalFormat::MsgPack)] {
        let cfg = Config::from_env(getter(HashMap::from([("DREVO_WAL_FORMAT", raw)])))
            .expect("compiled in");
        assert_eq!(cfg.wal_format, want);
    }
}

#[cfg(not(feature = "format-cbor"))]
#[test]
fn wal_format_rejects_a_format_built_without() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_WAL_FORMAT", "cbor")])))
        .expect_err("not compiled in");
    assert!(err.to_string().contains("not compiled in"), "{err}");
}

// ---------------------------------------------------------------------
// DREVO_GRPC_PORT (issue #583)
// ---------------------------------------------------------------------

#[test]
fn grpc_is_off_unless_a_port_is_set() {
    let cfg = Config::from_env(getter(HashMap::new())).expect("defaults");
    assert_eq!(cfg.grpc_port, None);
}

#[test]
fn grpc_port_must_be_a_port() {
    for bad in ["0", "70000", "grpc"] {
        let err =
            Config::from_env(getter(HashMap::from([("DREVO_GRPC_PORT", bad)]))).expect_err(bad);
        assert!(
            matches!(err, ConfigError::InvalidGrpcPort { .. }),
            "{bad}: {err}"
        );
    }
}

#[cfg(feature = "grpc")]
#[test]
fn grpc_port_is_taken_from_the_environment() {
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_GRPC_PORT", "50051")])))
        .expect("grpc compiled in");
    assert_eq!(cfg.grpc_port, Some(50051));
}

#[cfg(not(feature = "grpc"))]
#[test]
fn grpc_port_without_the_feature_is_a_startup_error() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_GRPC_PORT", "50051")])))
        .expect_err("not compiled in");
    assert!(err.to_string().contains("`grpc` feature"), "{err}");
}

// ---------------------------------------------------------------------
// DREVO_FLIGHT_PORT (issue #584)
// ---------------------------------------------------------------------

#[test]
fn flight_is_off_unless_a_port_is_set() {
    let cfg = Config::from_env(getter(HashMap::new())).expect("defaults");
    assert_eq!(cfg.flight_port, None);
}

#[test]
fn flight_port_must_be_a_port() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_FLIGHT_PORT", "flight")])))
        .expect_err("not a port");
    assert!(
        matches!(err, ConfigError::InvalidFlightPort { .. }),
        "{err}"
    );
}

#[cfg(feature = "arrow-flight")]
#[test]
fn flight_port_is_taken_from_the_environment() {
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_FLIGHT_PORT", "8815")])))
        .expect("compiled in");
    assert_eq!(cfg.flight_port, Some(8815));
}

#[cfg(not(feature = "arrow-flight"))]
#[test]
fn flight_port_without_the_feature_is_a_startup_error() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_FLIGHT_PORT", "8815")])))
        .expect_err("not compiled in");
    assert!(err.to_string().contains("`arrow-flight` feature"), "{err}");
}

// ── DREVO_AUTH: the first user (NEO4J_AUTH style) ─────────────────────

#[cfg(feature = "auth")]
#[test]
fn auth_bootstrap_is_read_like_neo4j_auth() {
    use drevo_auth::users::Bootstrap;
    let cfg = Config::from_env(getter(HashMap::new())).expect("defaults");
    assert_eq!(cfg.auth, Bootstrap::Generated);
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_AUTH", "none")]))).unwrap();
    assert_eq!(cfg.auth, Bootstrap::Disabled);
    let cfg =
        Config::from_env(getter(HashMap::from([("DREVO_AUTH", "admin/s3cret-pass")]))).unwrap();
    assert_eq!(
        cfg.auth,
        Bootstrap::Admin {
            user: "admin".into(),
            password: "s3cret-pass".into()
        }
    );
}

#[cfg(feature = "auth")]
#[test]
fn a_malformed_auth_setting_is_a_startup_error_without_the_password() {
    for bad in ["admin", "admin/", "bad name/s3cret-pass"] {
        let err = Config::from_env(getter(HashMap::from([("DREVO_AUTH", bad)])))
            .expect_err("malformed DREVO_AUTH");
        assert!(matches!(err, ConfigError::InvalidAuth { .. }), "{err}");
        assert!(err.to_string().contains("DREVO_AUTH"), "{err}");
        assert!(!err.to_string().contains("s3cret"), "{err}");
    }
}

#[cfg(feature = "auth")]
#[test]
fn the_config_debug_output_hides_the_password() {
    let cfg =
        Config::from_env(getter(HashMap::from([("DREVO_AUTH", "admin/s3cret-pass")]))).unwrap();
    let shown = format!("{cfg:?}");
    assert!(shown.contains("admin"), "{shown}");
    assert!(!shown.contains("s3cret-pass"), "{shown}");
}

#[cfg(not(feature = "auth"))]
#[test]
fn auth_without_the_feature_is_a_startup_error_unless_none() {
    Config::from_env(getter(HashMap::from([("DREVO_AUTH", "none")]))).expect("none needs nothing");
    let err = Config::from_env(getter(HashMap::from([("DREVO_AUTH", "admin/s3cret-pass")])))
        .expect_err("not compiled in");
    assert!(err.to_string().contains("`auth` feature"), "{err}");
    assert!(!err.to_string().contains("s3cret"), "{err}");
}

// ---------------------------------------------------------------------
// DREVO_BOLT_PORT, DREVO_TLS_CERT / DREVO_TLS_KEY
// ---------------------------------------------------------------------

#[test]
fn bolt_is_off_unless_a_port_is_set() {
    let cfg = Config::from_env(getter(HashMap::new())).expect("defaults");
    assert_eq!(cfg.bolt_port, None);
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_BOLT_PORT", "7687")]))).unwrap();
    assert_eq!(cfg.bolt_port, Some(7687));
}

#[test]
fn tls_is_off_unless_a_certificate_is_set() {
    let cfg = Config::from_env(getter(HashMap::new())).expect("defaults");
    assert!(cfg.tls.is_none());
}

#[test]
fn a_tls_certificate_needs_its_key_and_the_key_its_certificate() {
    for one in ["DREVO_TLS_CERT", "DREVO_TLS_KEY"] {
        let err = Config::from_env(getter(HashMap::from([(one, "/etc/drevo/x.pem")])))
            .expect_err("only one of the pair");
        assert!(
            matches!(err, ConfigError::InvalidTls { .. }),
            "{one}: {err}"
        );
        assert!(
            err.to_string().contains("DREVO_TLS_CERT") && err.to_string().contains("DREVO_TLS_KEY"),
            "{err}"
        );
    }
}

#[cfg(feature = "bolt-tls")]
#[test]
fn tls_files_are_taken_from_the_environment() {
    let cfg = Config::from_env(getter(HashMap::from([
        ("DREVO_TLS_CERT", "/etc/drevo/cert.pem"),
        ("DREVO_TLS_KEY", "/etc/drevo/key.pem"),
    ])))
    .expect("bolt-tls compiled in");
    let tls = cfg.tls.expect("tls on");
    assert_eq!(tls.cert.to_string_lossy(), "/etc/drevo/cert.pem");
    assert_eq!(tls.key.to_string_lossy(), "/etc/drevo/key.pem");
}

#[cfg(not(feature = "bolt-tls"))]
#[test]
fn tls_without_the_feature_is_a_startup_error() {
    let err = Config::from_env(getter(HashMap::from([
        ("DREVO_TLS_CERT", "/etc/drevo/cert.pem"),
        ("DREVO_TLS_KEY", "/etc/drevo/key.pem"),
    ])))
    .expect_err("not compiled in");
    assert!(matches!(err, ConfigError::InvalidTls { .. }), "{err}");
    assert!(err.to_string().contains("`bolt-tls` feature"), "{err}");
}

// ---------------------------------------------------------------------
// DREVO_AUDIT_LOG / DREVO_AUDIT_QUERY_TEXT
// ---------------------------------------------------------------------

#[test]
fn the_audit_log_is_off_unless_set() {
    let cfg = Config::from_env(getter(HashMap::new())).expect("defaults");
    assert_eq!(cfg.audit_log, drevo::server::AuditLog::Off);
    assert!(!cfg.audit_query_text);
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_AUDIT_LOG", "off")]))).unwrap();
    assert_eq!(cfg.audit_log, drevo::server::AuditLog::Off);
}

#[cfg(feature = "auth")]
#[test]
fn the_audit_log_goes_to_stdout_or_a_file() {
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_AUDIT_LOG", "stdout")]))).unwrap();
    assert_eq!(cfg.audit_log, drevo::server::AuditLog::Stdout);
    let cfg = Config::from_env(getter(HashMap::from([
        ("DREVO_AUDIT_LOG", "/var/log/drevo/audit.log"),
        ("DREVO_AUDIT_QUERY_TEXT", "true"),
    ])))
    .unwrap();
    assert_eq!(
        cfg.audit_log,
        drevo::server::AuditLog::File("/var/log/drevo/audit.log".into())
    );
    assert!(cfg.audit_query_text);
}

#[test]
fn audit_query_text_must_be_a_switch() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_AUDIT_QUERY_TEXT", "maybe")])))
        .expect_err("not a switch");
    assert!(matches!(err, ConfigError::InvalidAudit { .. }), "{err}");
}

#[cfg(not(feature = "auth"))]
#[test]
fn an_audit_log_without_the_auth_feature_is_a_startup_error() {
    let err = Config::from_env(getter(HashMap::from([("DREVO_AUDIT_LOG", "stdout")])))
        .expect_err("not compiled in");
    assert!(err.to_string().contains("`auth` feature"), "{err}");
}

// ---------------------------------------------------------------------
// DREVO_OIDC_*
// ---------------------------------------------------------------------

#[test]
fn no_identity_provider_unless_an_issuer_is_set() {
    let cfg = Config::from_env(getter(HashMap::new())).expect("defaults");
    assert_eq!(cfg.oidc, None);
    let cfg = Config::from_env(getter(HashMap::from([("DREVO_OIDC_ISSUER", " ")]))).unwrap();
    assert_eq!(cfg.oidc, None);
}

#[cfg(feature = "oidc")]
#[test]
fn an_identity_provider_takes_an_issuer_audience_and_claim_mapping() {
    let cfg = Config::from_env(getter(HashMap::from([
        (
            "DREVO_OIDC_ISSUER",
            "https://login.example.com/realms/drevo",
        ),
        ("DREVO_OIDC_AUDIENCE", "drevo"),
    ])))
    .unwrap();
    let oidc = cfg.oidc.expect("configured");
    assert_eq!(oidc.issuer, "https://login.example.com/realms/drevo");
    assert_eq!(oidc.audience, "drevo");
    assert_eq!(oidc.username_claim, "sub");
    assert_eq!(oidc.roles_claim.as_deref(), Some("groups"));
    assert!(oidc.role_map.is_empty());

    let cfg = Config::from_env(getter(HashMap::from([
        (
            "DREVO_OIDC_ISSUER",
            "https://login.example.com/realms/drevo",
        ),
        ("DREVO_OIDC_AUDIENCE", "drevo"),
        ("DREVO_OIDC_USERNAME_CLAIM", "preferred_username"),
        ("DREVO_OIDC_ROLES_CLAIM", "realm_roles"),
        (
            "DREVO_OIDC_ROLE_MAP",
            "admins=admin, devs=editor,devs=publisher",
        ),
    ])))
    .unwrap();
    let oidc = cfg.oidc.unwrap();
    assert_eq!(oidc.username_claim, "preferred_username");
    assert_eq!(oidc.roles_claim.as_deref(), Some("realm_roles"));
    assert_eq!(oidc.role_map["admins"], ["admin"]);
    assert_eq!(oidc.role_map["devs"], ["editor", "publisher"]);
}

#[cfg(feature = "oidc")]
#[test]
fn a_bad_identity_provider_setting_is_a_startup_error() {
    for (vars, needle) in [
        (
            vec![("DREVO_OIDC_ISSUER", "https://login.example.com")],
            "DREVO_OIDC_AUDIENCE",
        ),
        (
            vec![
                ("DREVO_OIDC_ISSUER", "http://login.example.com"),
                ("DREVO_OIDC_AUDIENCE", "drevo"),
            ],
            "https",
        ),
        (
            vec![
                ("DREVO_OIDC_ISSUER", "https://login.example.com"),
                ("DREVO_OIDC_AUDIENCE", "drevo"),
                ("DREVO_OIDC_ROLE_MAP", "admins"),
            ],
            "group=role",
        ),
        (
            vec![
                ("DREVO_OIDC_ISSUER", "https://login.example.com"),
                ("DREVO_OIDC_AUDIENCE", "drevo"),
                ("DREVO_OIDC_USERNAME_CLAIM", " "),
            ],
            "DREVO_OIDC_USERNAME_CLAIM",
        ),
    ] {
        let err = Config::from_env(getter(vars.into_iter().collect())).expect_err(needle);
        assert!(matches!(err, ConfigError::InvalidOidc { .. }), "{err}");
        assert!(err.to_string().contains(needle), "{needle}: {err}");
    }
}

#[cfg(not(feature = "oidc"))]
#[test]
fn an_identity_provider_without_the_oidc_feature_is_a_startup_error() {
    let err = Config::from_env(getter(HashMap::from([
        ("DREVO_OIDC_ISSUER", "https://login.example.com"),
        ("DREVO_OIDC_AUDIENCE", "drevo"),
    ])))
    .expect_err("not compiled in");
    assert!(err.to_string().contains("`oidc` feature"), "{err}");
}
