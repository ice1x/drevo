//! Phase 8 task 00052 — container integration test.
//!
//! Closes Phase 8: builds the production Docker image (the same one
//! [`docker-publish.yml`](../.github/workflows/docker-publish.yml) ships
//! to `ghcr.io/ice1x/drevo`), starts a container against a Docker
//! **named volume**, exercises a full Create / Read / Update / Delete
//! cycle over HTTP, then stops the container and starts a *fresh*
//! container against the **same** named volume to prove data survives
//! a restart — the production scenario K8s `strategy.type: Recreate`
//! (from task 00049's `k8s/base/deployment.yaml`) replays on every
//! rolling update.
//!
//! ## Test layers
//!
//! 1. **Structural (always-on, runs on every `cargo test`):**
//!    - the test file exists at the expected path,
//!    - every `live_container_*` function is `#[ignore]`-gated so a
//!      developer / CI without Docker still gets a clean run,
//!    - the README documents task 00052 and the opt-in command,
//!    - the test file pins port 8080 — the same port the Dockerfile
//!      (`EXPOSE 8080`), `docker-compose.yml`, and
//!      `k8s/base/service.yaml` already declare,
//!    - the test uses a **named volume** mounted at `/data` — the
//!      Dockerfile's `VOLUME ["/data"]` + `DREVO_DATA_DIR=/data`
//!      contract,
//!    - the test image tag namespace is `drevo:` (local-only — the
//!      ghcr.io tag lives in the publish workflow, not in the test).
//!
//! 2. **Live container (`#[ignore]`-by-default, opt-in via
//!    `cargo test --test container_integration_tests -- --include-ignored`):**
//!    - [`live_container_serves_health_and_status_endpoints`]
//!    - [`live_container_supports_node_crud_via_http`]
//!    - [`live_container_persists_node_across_restart`]
//!
//! The live tests assume `docker info` succeeds; they panic with a
//! pointer to the Docker daemon if it does not. They never run in
//! regular CI because the publish workflow already covers
//! image-buildability and the structural tests cover the wire-format
//! invariants — these tests are the manual "did the image really come
//! up?" gate for a maintainer cutting a release.

use std::fs;
use std::path::{Path, PathBuf};

// =====================================================================
// Path helpers
// =====================================================================

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn test_file_path() -> PathBuf {
    workspace_root().join("tests/container_integration_tests.rs")
}

fn read_test_file() -> String {
    fs::read_to_string(test_file_path()).expect("read tests/container_integration_tests.rs")
}

fn read_readme() -> String {
    fs::read_to_string(workspace_root().join("README.md")).expect("read README.md")
}

// =====================================================================
// Structural tests — always on
// =====================================================================

#[test]
fn test_file_exists_at_expected_path() {
    let path = test_file_path();
    assert!(
        path.exists(),
        "expected the container integration test file at {}",
        path.display()
    );
}

#[test]
fn every_live_test_is_ignored() {
    // Every `fn live_container_*` MUST be preceded by both `#[test]`
    // and `#[ignore]` so a default `cargo test` invocation never spins
    // up Docker. The CI matrix in `.github/workflows/ci.yml` would
    // otherwise need a Docker daemon, which it does not have.
    let src = read_test_file();
    let lines: Vec<&str> = src.lines().collect();
    let mut found = 0;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("fn live_container_") {
            continue;
        }
        // Skip references to the helpers inside other functions /
        // docstring lines.
        if !trimmed.contains('(') {
            continue;
        }
        let window_start = i.saturating_sub(8);
        let window: String = lines[window_start..i].join("\n");
        assert!(
            window.contains("#[ignore"),
            "{} (line {}) must be preceded by `#[ignore]`",
            trimmed,
            i + 1
        );
        assert!(
            window.contains("#[test]"),
            "{} (line {}) must be preceded by `#[test]`",
            trimmed,
            i + 1
        );
        found += 1;
    }
    assert!(
        found >= 3,
        "expected at least 3 `fn live_container_*` tests (health, CRUD, persistence); found {found}"
    );
}

#[test]
fn live_tests_pin_port_8080_inside_the_container() {
    // The container ALWAYS listens on 8080 — the Dockerfile sets
    // `ENV DREVO_PORT=8080` + `EXPOSE 8080`, `docker-compose.yml`
    // forwards `8080:8080`, and `k8s/base/service.yaml` targets
    // `8080`. A live test that mapped the host port to anything other
    // than 8080 inside the container would be a silent contract drift.
    let src = read_test_file();
    assert!(
        src.contains(":8080"),
        "live tests must publish the container's port 8080 (the documented EXPOSE port)"
    );
}

#[test]
fn live_tests_mount_data_dir_inside_the_container() {
    // `Dockerfile` declares `VOLUME [\"/data\"]` and the server defaults
    // `DREVO_DATA_DIR=/data`. The persistence test only works if the
    // host's named volume is mounted at exactly `/data` — otherwise the
    // redb file would land somewhere ephemeral and the restart half of
    // the test would silently pass against an empty database.
    let src = read_test_file();
    assert!(
        src.contains(":/data"),
        "live tests must bind the named volume to /data — the path declared by the Dockerfile"
    );
}

#[test]
fn live_tests_use_named_volume_not_bind_mount() {
    // A bind mount onto a host directory does not work because the
    // container's `drevo` user (UID 999, set by the Dockerfile) would
    // not own the host path. A Docker **named volume** initialises by
    // copying the chown'd /data contents from the image, so the
    // container's non-root user can write. This is a load-bearing
    // detail — pin it.
    let src = read_test_file();
    assert!(
        src.contains("docker_volume_create") || src.contains("[\"volume\", \"create\""),
        "live tests must create a Docker named volume (not bind-mount a host dir)"
    );
}

#[test]
fn live_tests_perform_full_crud_cycle() {
    // The task definition is "spin up container, run CRUD via HTTP".
    // Pin the four verbs so a future refactor cannot quietly drop one.
    let src = read_test_file();
    for verb in ["http_post", "http_get", "http_patch", "http_delete"] {
        assert!(
            src.contains(verb),
            "CRUD coverage missing — `{verb}` is not called in the live tests"
        );
    }
}

#[test]
fn live_tests_verify_persistence_with_a_restart() {
    // The persistence test must (a) create state in container A,
    // (b) tear container A down, (c) start a *different* container B
    // against the same named volume, (d) read the state back.
    // Locked here so a future refactor cannot collapse it into a
    // single-container test that silently misses the restart edge.
    let src = read_test_file();
    let func_marker = "fn live_container_persists_node_across_restart";
    let idx = src
        .find(func_marker)
        .expect("expected `fn live_container_persists_node_across_restart` in this file");
    let body = &src[idx..];
    // Two distinct LiveContainer::start sites inside the body.
    let starts = body.matches("LiveContainer::start").count();
    assert!(
        starts >= 2,
        "persistence test must start two containers against the same volume; found {starts}"
    );
}

#[test]
fn readme_marks_task_00052_complete() {
    // The README's Phase 8 checkbox for 00052 must flip to `[x]` once
    // this task lands.
    let readme = read_readme();
    assert!(
        readme.contains("- [x] `00052`"),
        "README must mark `00052` as done with `- [x] `00052``"
    );
}

#[test]
fn readme_documents_opt_in_command_for_live_container_tests() {
    // Live tests are `#[ignore]`-by-default; the README must explain
    // how a developer or release manager runs them on demand.
    let readme = read_readme();
    assert!(
        readme.contains("--include-ignored"),
        "README must document `cargo test ... -- --include-ignored` for the live container tests"
    );
    assert!(
        readme.contains("container_integration_tests"),
        "README must reference the test file by name so the opt-in command is greppable"
    );
}

#[test]
fn readme_documents_named_volume_and_restart_semantics() {
    let readme = read_readme();
    let lower = readme.to_lowercase();
    assert!(
        lower.contains("named volume") && lower.contains("restart"),
        "README must explain the named-volume + restart semantics of task 00052"
    );
}

#[test]
fn ci_workflow_does_not_run_live_container_tests() {
    // The `ci.yml` workflow runs `cargo test --all-features` without
    // `--include-ignored`. Adding `--include-ignored` to CI would
    // require a Docker daemon on the runner and would slow CI by
    // minutes per push — explicitly out of scope. Lock the
    // non-regression: ci.yml must NOT reference --include-ignored.
    let ci = fs::read_to_string(workspace_root().join(".github/workflows/ci.yml"))
        .expect("read .github/workflows/ci.yml");
    assert!(
        !ci.contains("--include-ignored"),
        "ci.yml must NOT enable --include-ignored — live container tests are opt-in only"
    );
}

// =====================================================================
// Live container harness
// =====================================================================
//
// The harness below is exercised only by the `#[ignore]`-gated tests
// further down. It is `cfg(test)`-only by virtue of being in a test
// crate; no production code depends on it.

#[allow(dead_code)] // helpers are only called by #[ignore] tests
const IMAGE_TAG: &str = "drevo:test-00052";

#[allow(dead_code)]
fn require_docker_daemon() {
    // The live tests are opt-in — if a developer runs them with
    // `--include-ignored` they must already have Docker installed and
    // the daemon running. Surface a clear panic instead of a cryptic
    // ECONNREFUSED 30s later.
    let output = std::process::Command::new("docker").arg("info").output();
    match output {
        Ok(out) if out.status.success() => {}
        Ok(out) => panic!(
            "`docker info` exited non-zero — start Docker before running the live container tests.\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(e) => panic!(
            "failed to spawn `docker` (is Docker installed and on PATH?): {e}"
        ),
    }
}

#[allow(dead_code)]
fn docker_build_image() {
    require_docker_daemon();
    let status = std::process::Command::new("docker")
        .args(["build", "-t", IMAGE_TAG, "."])
        .current_dir(workspace_root())
        .status()
        .expect("spawn `docker build`");
    assert!(
        status.success(),
        "`docker build -t {IMAGE_TAG} .` failed — see the build output above"
    );
}

#[allow(dead_code)]
fn unique_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock pre-epoch")
        .as_nanos();
    let pid = std::process::id();
    format!("{pid}-{nanos}")
}

#[allow(dead_code)]
fn pick_free_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0 for free-port probe");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

#[allow(dead_code)]
fn docker_volume_create(name: &str) {
    let status = std::process::Command::new("docker")
        .args(["volume", "create", name])
        .stdout(std::process::Stdio::null())
        .status()
        .expect("spawn `docker volume create`");
    assert!(status.success(), "`docker volume create {name}` failed");
}

#[allow(dead_code)]
struct VolumeGuard(String);

#[allow(dead_code)]
impl Drop for VolumeGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["volume", "rm", "-f", &self.0])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[allow(dead_code)]
struct LiveContainer {
    name: String,
    host_port: u16,
}

#[allow(dead_code)]
impl LiveContainer {
    fn start(suffix: &str, host_port: u16, volume: &str) -> Self {
        let name = format!("drevo-test-00052-{suffix}");
        // Remove a leftover container of the same name from a prior
        // failed run (defensive — Drop normally handles it).
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let port_arg = format!("{host_port}:8080");
        let vol_arg = format!("{volume}:/data");
        let status = std::process::Command::new("docker")
            .args([
                "run", "-d", "--name", &name, "-p", &port_arg, "-v", &vol_arg, IMAGE_TAG,
            ])
            .stdout(std::process::Stdio::null())
            .status()
            .expect("spawn `docker run`");
        assert!(
            status.success(),
            "`docker run` failed for container {name} on host port {host_port}"
        );
        let me = Self { name, host_port };
        me.wait_for_ready();
        me
    }

    fn wait_for_ready(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut last_err = String::new();
        while std::time::Instant::now() < deadline {
            match http_get(self.host_port, "/ready") {
                Ok((200, _)) => return,
                Ok((code, body)) => last_err = format!("HTTP {code}: {body}"),
                Err(e) => last_err = format!("connect error: {e}"),
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        let logs = std::process::Command::new("docker")
            .args(["logs", "--tail", "100", &self.name])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        panic!(
            "container {} did not become ready within 60s (host port {}). last probe: {}\n--- docker logs ---\n{}",
            self.name, self.host_port, last_err, logs
        );
    }
}

#[allow(dead_code)]
impl Drop for LiveContainer {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &self.name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

// =====================================================================
// Minimal HTTP/1.1 client — keeps dev-deps lean (no reqwest / ureq).
// =====================================================================

#[allow(dead_code)]
fn http_request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> std::io::Result<(u16, String)> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;

    let body_bytes = body.unwrap_or("").as_bytes();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        body_bytes.len()
    );
    if body.is_some() {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes())?;
    stream.write_all(body_bytes)?;
    stream.flush()?;

    let mut buf = Vec::with_capacity(4096);
    stream.read_to_end(&mut buf)?;

    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "no CRLFCRLF in HTTP response",
            )
        })?;
    let header_str = std::str::from_utf8(&buf[..split])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let status: u16 = header_str
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "no HTTP status code")
        })?;
    let body_str = String::from_utf8_lossy(&buf[split + 4..]).into_owned();
    Ok((status, body_str))
}

#[allow(dead_code)]
fn http_get(port: u16, path: &str) -> std::io::Result<(u16, String)> {
    http_request(port, "GET", path, None)
}

#[allow(dead_code)]
fn http_post(port: u16, path: &str, body: &str) -> std::io::Result<(u16, String)> {
    http_request(port, "POST", path, Some(body))
}

#[allow(dead_code)]
fn http_patch(port: u16, path: &str, body: &str) -> std::io::Result<(u16, String)> {
    http_request(port, "PATCH", path, Some(body))
}

#[allow(dead_code)]
fn http_delete(port: u16, path: &str) -> std::io::Result<(u16, String)> {
    http_request(port, "DELETE", path, None)
}

#[allow(dead_code)]
fn parse_id_from_response(json: &str) -> u64 {
    // The Node response is JSON like
    //   {"id":1,"uuid":"...","kind":"note",...}
    // The leading `"id":` is unique enough for the small payloads we
    // create here — we don't need a full JSON parser.
    let key = "\"id\":";
    let start = json
        .find(key)
        .unwrap_or_else(|| panic!("response has no `id` field: {json}"))
        + key.len();
    let rest = &json[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end]
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("`id` is not a u64: {}", &rest[..end]))
}

// =====================================================================
// Live tests (opt-in via `cargo test -- --include-ignored`)
// =====================================================================

#[test]
#[ignore = "requires Docker daemon; opt-in via `cargo test --test container_integration_tests -- --include-ignored`"]
fn live_container_serves_health_and_status_endpoints() {
    docker_build_image();
    let suffix = unique_suffix();
    let volume = format!("drevo-test-vol-{suffix}");
    docker_volume_create(&volume);
    let _vol = VolumeGuard(volume.clone());
    let port = pick_free_port();
    let _container = LiveContainer::start(&suffix, port, &volume);

    let (status, body) = http_get(port, "/health").expect("GET /health");
    assert_eq!(status, 200, "/health body = {body}");
    assert!(body.contains("\"ok\""), "/health body = {body}");

    let (status, body) = http_get(port, "/ready").expect("GET /ready");
    assert_eq!(status, 200, "/ready body = {body}");
    assert!(body.contains("\"ready\""), "/ready body = {body}");

    let (status, body) = http_get(port, "/status").expect("GET /status");
    assert_eq!(status, 200, "/status body = {body}");
    assert!(body.contains("\"drevo\""), "/status body = {body}");
}

#[test]
#[ignore = "requires Docker daemon; opt-in via `cargo test --test container_integration_tests -- --include-ignored`"]
fn live_container_supports_node_crud_via_http() {
    docker_build_image();
    let suffix = unique_suffix();
    let volume = format!("drevo-test-vol-{suffix}");
    docker_volume_create(&volume);
    let _vol = VolumeGuard(volume.clone());
    let port = pick_free_port();
    let _container = LiveContainer::start(&suffix, port, &volume);

    // CREATE — POST /nodes
    let create_body = r#"{"kind":"note","title":"hello","body":"world","body_html":"<p>world</p>","properties":{}}"#;
    let (status, body) = http_post(port, "/nodes", create_body).expect("POST /nodes");
    assert_eq!(status, 201, "POST /nodes body = {body}");
    let id = parse_id_from_response(&body);

    // READ — GET /nodes/{id}
    let (status, body) = http_get(port, &format!("/nodes/{id}")).expect("GET /nodes/{id}");
    assert_eq!(status, 200, "GET /nodes/{id} body = {body}");
    assert!(body.contains("\"title\":\"hello\""), "body = {body}");
    assert!(body.contains("\"kind\":\"note\""), "body = {body}");

    // UPDATE — PATCH /nodes/{id}
    let patch = r#"{"title":"renamed"}"#;
    let (status, body) =
        http_patch(port, &format!("/nodes/{id}"), patch).expect("PATCH /nodes/{id}");
    assert_eq!(status, 200, "PATCH /nodes/{id} body = {body}");
    assert!(body.contains("\"title\":\"renamed\""), "body = {body}");

    // LIST — GET /nodes?kind=note (the `kind` filter is mandatory; the
    // API does not expose an unfiltered list to avoid accidental
    // table-scans against large graphs).
    let (status, body) = http_get(port, "/nodes?kind=note").expect("GET /nodes?kind=note");
    assert_eq!(status, 200, "GET /nodes?kind=note body = {body}");
    assert!(body.contains("\"renamed\""), "list missing renamed: {body}");

    // DELETE — DELETE /nodes/{id}
    let (status, body) = http_delete(port, &format!("/nodes/{id}")).expect("DELETE /nodes/{id}");
    assert_eq!(status, 204, "DELETE /nodes/{id} body = {body}");

    // Confirm the delete — subsequent GET must 404.
    let (status, _) = http_get(port, &format!("/nodes/{id}")).expect("GET /nodes/{id}");
    assert_eq!(status, 404, "node should be gone after DELETE");
}

#[test]
#[ignore = "requires Docker daemon; opt-in via `cargo test --test container_integration_tests -- --include-ignored`"]
fn live_container_persists_node_across_restart() {
    docker_build_image();
    let suffix = unique_suffix();
    let volume = format!("drevo-test-vol-{suffix}");
    docker_volume_create(&volume);
    let _vol = VolumeGuard(volume.clone());

    // ----- Container A: create state -----
    let port_a = pick_free_port();
    let container_a = LiveContainer::start(&suffix, port_a, &volume);
    let create_body = r#"{"kind":"persistent","title":"survives_restart","body":"hello after restart","body_html":"","properties":{"durable":"yes"}}"#;
    let (status, body) =
        http_post(port_a, "/nodes", create_body).expect("POST /nodes (container A)");
    assert_eq!(status, 201, "POST /nodes (A) body = {body}");
    let id = parse_id_from_response(&body);
    drop(container_a); // `docker rm -f` — fresh start next.

    // ----- Container B: same named volume, must see the node -----
    let port_b = pick_free_port();
    let suffix_b = format!("{suffix}-b");
    let _container_b = LiveContainer::start(&suffix_b, port_b, &volume);
    let (status, body) =
        http_get(port_b, &format!("/nodes/{id}")).expect("GET /nodes/{id} after restart");
    assert_eq!(
        status, 200,
        "node {id} did not survive container restart — GET returned {status}: {body}"
    );
    assert!(
        body.contains("\"survives_restart\""),
        "restored node missing title: {body}"
    );
    assert!(
        body.contains("\"durable\":\"yes\""),
        "restored node missing properties: {body}"
    );
}
