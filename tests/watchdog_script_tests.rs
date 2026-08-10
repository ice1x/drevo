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
