//! Dockerfile structure and convention tests.
//!
//! Task 00045: verify the Dockerfile follows the expected multi-stage
//! build pattern and uses the correct base images.
//!
//! These tests parse the Dockerfile as text to validate structure
//! without requiring Docker to be installed.

use std::fs;
use std::path::Path;

#[test]
fn dockerfile_exists() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Dockerfile");
    assert!(path.exists(), "Dockerfile must exist at project root");
}

#[test]
fn dockerfile_has_multi_stage_build() {
    let content = read_dockerfile();
    let from_count = content
        .lines()
        .filter(|l| l.trim_start().to_uppercase().starts_with("FROM"))
        .count();
    assert!(
        from_count >= 2,
        "Dockerfile must have at least 2 FROM stages (multi-stage build), found {from_count}"
    );
}

#[test]
fn builder_stage_uses_rust_slim() {
    let content = read_dockerfile();
    let builder_line = content
        .lines()
        .find(|l| l.contains("AS builder") || l.contains("as builder"))
        .expect("Dockerfile must have a builder stage (FROM ... AS builder)");
    assert!(
        builder_line.contains("rust:") && builder_line.contains("slim"),
        "builder stage should use rust:*-slim image, got: {builder_line}"
    );
}

#[test]
fn runtime_stage_uses_debian_bookworm_slim() {
    let content = read_dockerfile();
    let from_lines: Vec<&str> = content
        .lines()
        .filter(|l| l.trim_start().to_uppercase().starts_with("FROM"))
        .collect();
    // The last FROM is the runtime stage
    let runtime_line = from_lines
        .last()
        .expect("Dockerfile must have at least one FROM");
    assert!(
        runtime_line.contains("debian:bookworm-slim"),
        "runtime stage should use debian:bookworm-slim, got: {runtime_line}"
    );
}

#[test]
fn dockerfile_exposes_port_8080() {
    let content = read_dockerfile();
    let has_expose = content
        .lines()
        .any(|l| l.trim_start().starts_with("EXPOSE") && l.contains("8080"));
    assert!(has_expose, "Dockerfile must EXPOSE 8080");
}

#[test]
fn dockerfile_has_volume_data() {
    let content = read_dockerfile();
    let has_volume = content
        .lines()
        .any(|l| l.trim_start().starts_with("VOLUME") && l.contains("/data"));
    assert!(has_volume, "Dockerfile must declare VOLUME /data");
}

#[test]
fn dockerfile_runs_as_non_root() {
    let content = read_dockerfile();
    let has_user = content
        .lines()
        .any(|l| l.trim_start().starts_with("USER") && !l.contains("root"));
    assert!(
        has_user,
        "Dockerfile must switch to a non-root USER for security"
    );
}

#[test]
fn dockerfile_uses_exec_form_entrypoint() {
    let content = read_dockerfile();
    let entrypoint_line = content
        .lines()
        .find(|l| l.trim_start().starts_with("ENTRYPOINT"))
        .expect("Dockerfile must have an ENTRYPOINT");
    assert!(
        entrypoint_line.contains('['),
        "ENTRYPOINT must use exec form (JSON array), got: {entrypoint_line}"
    );
}

#[test]
fn dockerfile_sets_env_defaults() {
    let content = read_dockerfile();
    let envs: Vec<&str> = content
        .lines()
        .filter(|l| l.trim_start().starts_with("ENV"))
        .collect();
    let env_str = envs.join("\n");
    assert!(
        env_str.contains("DREVO_PORT"),
        "Dockerfile must set DREVO_PORT env default"
    );
    assert!(
        env_str.contains("DREVO_DATA_DIR"),
        "Dockerfile must set DREVO_DATA_DIR env default"
    );
    assert!(
        env_str.contains("DREVO_HOST"),
        "Dockerfile must set DREVO_HOST env default"
    );
}

#[test]
fn dockerfile_copies_binary_from_builder() {
    let content = read_dockerfile();
    let has_copy = content
        .lines()
        .any(|l| l.contains("COPY --from=builder") && l.contains("drevo-server"));
    assert!(
        has_copy,
        "Dockerfile must COPY --from=builder the drevo-server binary"
    );
}

#[test]
fn dockerignore_exists() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".dockerignore");
    assert!(path.exists(), ".dockerignore must exist at project root");
}

#[test]
fn dockerignore_excludes_target_and_git() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".dockerignore");
    let content = fs::read_to_string(path).unwrap();
    assert!(
        content.contains("target/"),
        ".dockerignore must exclude target/"
    );
    assert!(
        content.contains(".git/"),
        ".dockerignore must exclude .git/"
    );
}

fn read_dockerfile() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Dockerfile");
    fs::read_to_string(path).expect("failed to read Dockerfile")
}
