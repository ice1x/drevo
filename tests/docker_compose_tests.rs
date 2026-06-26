//! docker-compose.yml structure and convention tests.
//!
//! Task 00047 / 00163: verify the docker-compose.yml at the project root
//! exposes drevo as a single service with the expected host bind-mount (00163
//! replaced the named volume), port mapping, and environment defaults.
//!
//! These tests parse docker-compose.yml as text so they work without a
//! YAML parser dependency or a running Docker daemon — mirroring the
//! pattern used in `tests/dockerfile_tests.rs`.

use std::fs;
use std::path::Path;

fn read_compose() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docker-compose.yml");
    fs::read_to_string(path).expect("failed to read docker-compose.yml")
}

#[test]
fn docker_compose_exists() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docker-compose.yml");
    assert!(
        path.exists(),
        "docker-compose.yml must exist at project root"
    );
}

#[test]
fn docker_compose_declares_services_section() {
    let content = read_compose();
    assert!(
        content
            .lines()
            .any(|l| l.trim_start().starts_with("services:")),
        "docker-compose.yml must declare a top-level `services:` section"
    );
}

#[test]
fn docker_compose_defines_drevo_service() {
    let content = read_compose();
    // The service key must be indented under `services:` — a non-empty
    // leading indent distinguishes it from a top-level field.
    let has_service = content.lines().any(|l| {
        let trimmed = l.trim_start();
        let indent = l.len() - trimmed.len();
        indent > 0 && (trimmed.starts_with("drevo:") || trimmed.starts_with("drevo-db:"))
    });
    assert!(
        has_service,
        "docker-compose.yml must define a `drevo` (or `drevo-db`) service"
    );
}

#[test]
fn docker_compose_uses_local_build_or_image() {
    let content = read_compose();
    let has_build = content.lines().any(|l| l.trim_start().starts_with("build"));
    let has_image = content
        .lines()
        .any(|l| l.trim_start().starts_with("image:"));
    assert!(
        has_build || has_image,
        "docker-compose.yml must define either `build:` or `image:` for the service"
    );
}

#[test]
fn docker_compose_maps_port_8080() {
    let content = read_compose();
    // Accept either "8080:8080" string in any port form (short or long).
    assert!(
        content.contains("8080:8080") || content.contains("\"8080:8080\""),
        "docker-compose.yml must publish container port 8080 to host 8080"
    );
}

#[test]
fn docker_compose_mounts_data_volume() {
    let content = read_compose();
    // The named volume side and the in-container path must both be present.
    assert!(
        content.contains(":/data"),
        "docker-compose.yml must mount a volume into /data (got no `:/data` mount string)"
    );
    let has_volumes_section = content
        .lines()
        .any(|l| l.trim_start().starts_with("volumes:"));
    assert!(
        has_volumes_section,
        "docker-compose.yml must contain a `volumes:` key (under the service and/or top level)"
    );
}

#[test]
fn docker_compose_bind_mounts_host_data_dir() {
    let content = read_compose();
    // Task 00163: the named volume was replaced with a HOST bind-mount so the
    // redb file lives on the host (inspectable, survives `compose down`) and a
    // single drevo-server owns it. The mount maps a configurable host dir to
    // /data, and the service runs as the host user so the non-root image can
    // take redb's write lock on the bind-mounted folder.
    assert!(
        content.contains("${DREVO_DATA_DIR:-./data}:/data"),
        "docker-compose.yml must bind-mount the configurable host data dir into /data"
    );
    assert!(
        content.contains("${DREVO_UID:-1000}:${DREVO_GID:-1000}"),
        "docker-compose.yml must run as the host user (DREVO_UID:DREVO_GID) to write the bind-mounted redb file"
    );
    // The old named volume must be gone — a named volume would shadow the
    // bind-mount and hide the host file.
    assert!(
        !content.contains("drevo-data:"),
        "docker-compose.yml must not declare the old `drevo-data` named volume (use a host bind-mount)"
    );
}

#[test]
fn docker_compose_sets_env_vars() {
    let content = read_compose();
    assert!(
        content.contains("DREVO_HOST"),
        "docker-compose.yml must set DREVO_HOST"
    );
    assert!(
        content.contains("DREVO_PORT"),
        "docker-compose.yml must set DREVO_PORT"
    );
    assert!(
        content.contains("DREVO_DATA_DIR"),
        "docker-compose.yml must set DREVO_DATA_DIR"
    );
}

#[test]
fn docker_compose_data_dir_points_to_volume_mount() {
    let content = read_compose();
    // DREVO_DATA_DIR should align with the /data mount point declared
    // in the Dockerfile so the database file lives in the persistent volume.
    let mentions_data_dir_value = content
        .lines()
        .any(|l| l.contains("DREVO_DATA_DIR") && l.contains("/data"));
    assert!(
        mentions_data_dir_value,
        "DREVO_DATA_DIR in docker-compose.yml must be set to /data (matching the Dockerfile VOLUME)"
    );
}

#[test]
fn docker_compose_has_restart_policy() {
    let content = read_compose();
    let has_restart = content
        .lines()
        .any(|l| l.trim_start().starts_with("restart:"));
    assert!(
        has_restart,
        "docker-compose.yml should declare a `restart:` policy for the service so the DB survives container exits"
    );
}

#[test]
fn docker_compose_declares_healthcheck() {
    let content = read_compose();
    // The HTTP API already exposes GET /health (task 00042). A healthcheck
    // hook keeps Docker / orchestrators aware of liveness status.
    let has_healthcheck = content
        .lines()
        .any(|l| l.trim_start().starts_with("healthcheck:"));
    assert!(
        has_healthcheck,
        "docker-compose.yml must declare a `healthcheck:` block hitting GET /health"
    );
    assert!(
        content.contains("/health"),
        "healthcheck command must hit the /health endpoint"
    );
}

#[test]
fn docker_compose_no_obsolete_version_field() {
    let content = read_compose();
    // Compose Spec (v2+) deprecates the top-level `version:` key. Modern
    // docker-compose / docker compose emits a warning when it is present.
    let has_version_at_top = content.lines().any(|l| l.starts_with("version:"));
    assert!(
        !has_version_at_top,
        "docker-compose.yml must omit the obsolete top-level `version:` key (Compose Spec)"
    );
}
