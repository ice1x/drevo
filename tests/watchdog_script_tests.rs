//! Behavioural tests for the local-container ops scripts in `scripts/`.
//!
//! The watchdog ([`scripts/drevo-watchdog.sh`]) is the layer that self-heals an
//! accidental `docker rm -f drevo` — a removal a Docker restart policy cannot
//! cover. These tests pin its decision logic without a real Docker daemon by
//! injecting a stub `docker` (via `DREVO_DOCKER`) and a marker-writing recreate
//! command (via `DREVO_UP_CMD`):
//!
//! * container absent / not running  → the watchdog recreates it,
//! * container running               → the watchdog does nothing,
//! * disable sentinel present        → the watchdog does nothing,
//!
//! plus a `bash -n` syntax gate on both shell scripts. Unix-only: the scripts
//! and the stub are bash.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn watchdog() -> PathBuf {
    repo_root().join("scripts/drevo-watchdog.sh")
}

/// Write an executable stub `docker` whose `inspect` prints `status` (or exits
/// non-zero when `status` is empty, mimicking a container that does not exist).
fn write_stub_docker(dir: &Path, status: &str) -> PathBuf {
    let path = dir.join("docker");
    let body = if status.is_empty() {
        // No such container → `docker inspect` fails; the script maps this to
        // "absent" via its `|| echo absent` fallback.
        "#!/usr/bin/env bash\nif [ \"$1\" = inspect ]; then exit 1; fi\nexit 0\n".to_string()
    } else {
        format!("#!/usr/bin/env bash\nif [ \"$1\" = inspect ]; then echo {status}; exit 0; fi\nexit 0\n")
    };
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Run the watchdog with a stub docker reporting `status`, a recreate command
/// that touches a marker file, and a guaranteed-absent disable sentinel.
/// Returns whether the marker was created (i.e. whether it recreated).
fn run_watchdog_recreates(status: &str) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let docker = write_stub_docker(dir.path(), status);
    let marker = dir.path().join("recreated.marker");
    let disable = dir.path().join("nonexistent.disabled");

    let out = Command::new("bash")
        .arg(watchdog())
        .env("DREVO_DOCKER", &docker)
        .env("DREVO_UP_CMD", format!("touch {}", marker.display()))
        .env("DREVO_WATCHDOG_DISABLE_FILE", &disable)
        .output()
        .expect("run watchdog");
    assert!(
        out.status.success(),
        "watchdog exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    marker.exists()
}

#[test]
fn shell_scripts_have_valid_bash_syntax() {
    for rel in ["scripts/drevo-watchdog.sh", "scripts/drevo-restart.sh"] {
        let out = Command::new("bash")
            .arg("-n")
            .arg(repo_root().join(rel))
            .output()
            .expect("bash -n");
        assert!(
            out.status.success(),
            "{rel} failed `bash -n`: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn the_restart_script_forwards_the_server_settings() {
    let script = std::fs::read_to_string(repo_root().join("scripts/drevo-restart.sh")).unwrap();
    let line = script
        .lines()
        .find(|l| l.starts_with("FORWARD_VARS="))
        .expect("a FORWARD_VARS line");
    for var in [
        "DREVO_AUTH",
        "DREVO_EMBEDDINGS_API_KEY",
        "DREVO_TEXT2CYPHER_UPSTREAM",
        "DREVO_TEXT2CYPHER_MODEL",
        "DREVO_TEXT2CYPHER_API_KEY",
        "DREVO_QUERY_TIMEOUT_MS",
        "DREVO_IDLE_SESSION_TIMEOUT_MS",
    ] {
        assert!(line.contains(var), "{var} is not forwarded: {line}");
    }
    assert!(script.contains("for _v in $FORWARD_VARS; do"));
}

#[test]
fn the_restart_script_forwards_the_extraction_and_ingest_settings() {
    // Graph-RAG settings read by the server (docs/admin-guide.md): a
    // container recreated by the script loses them unless they are forwarded
    // (#821).
    let script = std::fs::read_to_string(repo_root().join("scripts/drevo-restart.sh")).unwrap();
    let line = script
        .lines()
        .find(|l| l.starts_with("FORWARD_VARS="))
        .expect("a FORWARD_VARS line");
    for var in [
        "DREVO_EXTRACT_UPSTREAM",
        "DREVO_EXTRACT_API_KEY",
        "DREVO_EXTRACT_MODEL",
        "DREVO_EXTRACT_TIMEOUT_MS",
        "DREVO_INGEST_WORKERS",
        "DREVO_INGEST_MAX_ATTEMPTS",
        "DREVO_INGEST_RETRY_BASE_MS",
        "DREVO_COMMUNITY_MAX_LLM_SYNC",
    ] {
        assert!(line.contains(var), "#821: {var} is not forwarded: {line}");
    }
}

/// Run `scripts/drevo-restart.sh` with `settings` in its environment and
/// nothing else of the caller's, against a stub `docker` and a stub `curl`
/// (the health check), and return the arguments `docker run` was given.
fn restart_run_arguments(settings: &[(&str, &str)]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("docker.log");
    let stub = |name: &str, body: String| {
        let path = dir.path().join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let docker = stub(
        "docker",
        format!(
            "#!/usr/bin/env bash\nif [ \"$1\" = run ]; then printf '%s\\n' \"$@\" > '{}'; fi\nexit 0\n",
            log.display()
        ),
    );
    stub("curl", "#!/usr/bin/env bash\nexit 0\n".to_string());
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("bash")
        .arg(repo_root().join("scripts/drevo-restart.sh"))
        .env_clear()
        .env("PATH", path)
        .env("HOME", dir.path())
        .env("DREVO_DOCKER", &docker)
        .env("DREVO_DATA_DIR", dir.path().join("data"))
        .env("DREVO_ENV_FILE", dir.path().join("no-such.env"))
        .envs(settings.iter().copied())
        .output()
        .expect("run the restart script");
    assert!(
        out.status.success(),
        "the restart script failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::read_to_string(&log).expect("docker run was called")
}

#[test]
fn a_restart_passes_the_extraction_and_ingest_settings_to_the_container() {
    let settings = [
        (
            "DREVO_EXTRACT_UPSTREAM",
            "http://chat.example/v1/chat/completions",
        ),
        ("DREVO_EXTRACT_API_KEY", "test-key"),
        ("DREVO_EXTRACT_MODEL", "extract-model"),
        ("DREVO_EXTRACT_TIMEOUT_MS", "45000"),
        ("DREVO_INGEST_WORKERS", "4"),
        ("DREVO_INGEST_MAX_ATTEMPTS", "7"),
        ("DREVO_INGEST_RETRY_BASE_MS", "250"),
        ("DREVO_COMMUNITY_MAX_LLM_SYNC", "12"),
    ];
    let args = restart_run_arguments(&settings);
    let args: Vec<&str> = args.lines().collect();
    for (var, value) in settings {
        let wanted = format!("{var}={value}");
        assert!(
            args.windows(2).any(|w| w[0] == "-e" && w[1] == wanted),
            "#821: `-e {wanted}` is not passed to docker run: {args:?}"
        );
    }
}

#[test]
fn a_restart_passes_no_setting_that_is_not_set() {
    let args = restart_run_arguments(&[("DREVO_INGEST_WORKERS", "4")]);
    assert!(args.contains("DREVO_INGEST_WORKERS=4"), "{args}");
    for var in [
        "DREVO_EXTRACT_UPSTREAM",
        "DREVO_COMMUNITY_MAX_LLM_SYNC",
        "DREVO_AUTH",
    ] {
        assert!(!args.contains(var), "{var} was passed though unset: {args}");
    }
}

/// Server settings the restart script does not pass on, each with why.
const NOT_FORWARDED: &[(&str, &str)] = &[
    (
        "DREVO_HOST",
        "set by the script: the server listens on every interface of the container",
    ),
    (
        "DREVO_PORT",
        "set by the script: 8080 inside the container, published as DREVO_HTTP_PORT",
    ),
    (
        "DREVO_BOLT_PORT",
        "set by the script: 7687 inside, published as DREVO_BOLT_PORT_HOST",
    ),
    (
        "DREVO_DATA_DIR",
        "set by the script: /data inside, the host directory is bind-mounted",
    ),
    ("DREVO_VERSION", "build-time: baked into the image"),
    ("DREVO_GIT_SHA", "build-time: baked into the image"),
    ("DREVO_BUILD_DATE", "build-time: baked into the image"),
    (
        "DREVO_ENCRYPTION_KEY_FILE",
        "names a file inside the container: needs a mount as well",
    ),
    (
        "DREVO_TLS_CERT",
        "names a file inside the container: needs a mount as well",
    ),
    (
        "DREVO_TLS_KEY",
        "names a file inside the container: needs a mount as well",
    ),
    (
        "DREVO_BACKUP_DIR",
        "names a directory inside the container: needs a mount as well",
    ),
    ("DREVO_GRPC_PORT", "needs a published port as well"),
    ("DREVO_FLIGHT_PORT", "needs a published port as well"),
];

/// Every `"DREVO_…"` string literal in the server's sources: the settings
/// it reads from the environment.
fn settings_read_by_the_server() -> Vec<String> {
    fn walk(dir: &Path, found: &mut std::collections::BTreeSet<String>) {
        for entry in fs::read_dir(dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, found);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = fs::read_to_string(&path).unwrap();
                for piece in text.split("\"DREVO_").skip(1) {
                    let name: String = piece
                        .chars()
                        .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                        .collect();
                    if piece[name.len()..].starts_with('"') && !name.is_empty() {
                        found.insert(format!("DREVO_{name}"));
                    }
                }
            }
        }
    }
    let mut found = std::collections::BTreeSet::new();
    walk(&repo_root().join("src"), &mut found);
    found.into_iter().collect()
}

#[test]
fn every_server_setting_is_forwarded_or_named_as_not() {
    // A setting in the environment file that the script drops looks applied
    // and is not (#840): each one is either forwarded or listed above with
    // the reason.
    let script = std::fs::read_to_string(repo_root().join("scripts/drevo-restart.sh")).unwrap();
    let line = script
        .lines()
        .find(|l| l.starts_with("FORWARD_VARS="))
        .expect("a FORWARD_VARS line");
    let forwarded: Vec<&str> = line
        .trim_start_matches("FORWARD_VARS=")
        .trim_matches('"')
        .split_whitespace()
        .collect();
    let settings = settings_read_by_the_server();
    assert!(
        settings.len() > 40,
        "the scan found the settings: {settings:?}"
    );
    let mut dropped = Vec::new();
    for setting in &settings {
        let named = NOT_FORWARDED.iter().any(|(name, _)| name == setting);
        match (forwarded.contains(&setting.as_str()), named) {
            (true, false) | (false, true) => {}
            (false, false) => dropped.push(setting.as_str()),
            (true, true) => panic!("{setting} is both forwarded and listed as not forwarded"),
        }
    }
    assert!(
        dropped.is_empty(),
        "#840: scripts/drevo-restart.sh neither forwards these settings nor are they in \
         NOT_FORWARDED with a reason: {dropped:?}"
    );
    // Nothing stale: a forwarded or excepted name is a setting the server reads.
    for name in forwarded
        .iter()
        .copied()
        .chain(NOT_FORWARDED.iter().map(|(name, _)| *name))
    {
        assert!(
            settings.iter().any(|s| s == name),
            "{name} is not a setting the server reads"
        );
    }
    // The script says what else the excepted ones need.
    for (name, _) in NOT_FORWARDED
        .iter()
        .filter(|(_, why)| why.contains("needs"))
    {
        assert!(
            script.contains(name),
            "the script header must mention {name}"
        );
    }
}

#[test]
fn a_restart_passes_the_transaction_and_access_settings_to_the_container() {
    let settings = [
        ("DREVO_READ_ONLY", "1"),
        ("DREVO_TX_ISOLATION", "serializable"),
        ("DREVO_WAL_FORMAT", "cbor"),
        ("DREVO_SLOW_QUERY_MS", "250"),
        ("DREVO_OIDC_ISSUER", "https://id.example/realms/drevo"),
        ("DREVO_RAG_TRACE_RETENTION_DAYS", "30"),
    ];
    let args = restart_run_arguments(&settings);
    let args: Vec<&str> = args.lines().collect();
    for (var, value) in settings {
        let wanted = format!("{var}={value}");
        assert!(
            args.windows(2).any(|w| w[0] == "-e" && w[1] == wanted),
            "#840: `-e {wanted}` is not passed to docker run: {args:?}"
        );
    }
}

#[test]
fn watchdog_recreates_when_container_absent() {
    assert!(
        run_watchdog_recreates(""),
        "a removed container must be recreated"
    );
}

#[test]
fn watchdog_recreates_when_container_exited() {
    assert!(
        run_watchdog_recreates("exited"),
        "a stopped/exited container must be recreated"
    );
}

#[test]
fn watchdog_is_noop_when_container_running() {
    assert!(
        !run_watchdog_recreates("running"),
        "a running container must be left untouched"
    );
}

#[test]
fn watchdog_respects_disable_sentinel() {
    let dir = tempfile::tempdir().unwrap();
    let docker = write_stub_docker(dir.path(), ""); // absent → would recreate…
    let marker = dir.path().join("recreated.marker");
    let disable = dir.path().join("present.disabled");
    fs::write(&disable, b"").unwrap(); // …but the sentinel suppresses it.

    let out = Command::new("bash")
        .arg(watchdog())
        .env("DREVO_DOCKER", &docker)
        .env("DREVO_UP_CMD", format!("touch {}", marker.display()))
        .env("DREVO_WATCHDOG_DISABLE_FILE", &disable)
        .output()
        .expect("run watchdog");
    assert!(out.status.success());
    assert!(
        !marker.exists(),
        "the disable sentinel must suppress all recreation"
    );
}
