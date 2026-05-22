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

// ------------------------------------------------------------------
// Manifest-parse correctness — every Cargo.toml `[[bench]]`,
// `[[bin]]`, and `[[test]]` declaration must be reachable in the
// Docker build context. A declaration without a matching file (or a
// COPY that misses the directory) makes `cargo build` fail at
// manifest-parse time inside the container — which is exactly how
// Phase 8 task 00051 (GHCR publish) caught the missing `COPY benches/`
// on its first run. These tests lock the invariant going forward so
// a future bench / bin / test addition cannot regress the container
// build without a corresponding Dockerfile update.
// ------------------------------------------------------------------

/// Read all top-level `[[bench]]` / `[[bin]]` / `[[test]]` blocks
/// from `Cargo.toml` and return the directory each block points at
/// (e.g. `benches`, `src/bin`, `tests`). The Dockerfile must COPY
/// every directory in the returned set into the builder stage, or
/// manifest parsing fails inside the container.
fn cargo_manifest_target_dirs() -> Vec<String> {
    let cargo_toml = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("failed to read Cargo.toml");
    let mut dirs: Vec<String> = Vec::new();
    let mut current_section: Option<&'static str> = None;
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[[bench]]") {
            current_section = Some("bench");
        } else if trimmed.starts_with("[[bin]]") {
            current_section = Some("bin");
        } else if trimmed.starts_with("[[test]]") {
            current_section = Some("test");
        } else if trimmed.starts_with('[') {
            current_section = None;
        } else if let Some(section) = current_section {
            // Either an explicit `path = "..."` or rely on the
            // convention: benches/<name>.rs, src/bin/<name>.rs,
            // tests/<name>.rs.
            if let Some(rest) = trimmed.strip_prefix("path") {
                if let Some(value) = rest.split('=').nth(1) {
                    let path = value.trim().trim_matches('"').to_string();
                    if let Some((dir, _)) = path.rsplit_once('/') {
                        dirs.push(dir.to_string());
                    }
                }
            } else if trimmed.starts_with("name") {
                let dir = match section {
                    "bench" => "benches",
                    "bin" => "src/bin",
                    "test" => "tests",
                    _ => continue,
                };
                dirs.push(dir.to_string());
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

#[test]
fn cargo_manifest_target_dirs_collector_finds_benches() {
    // Sanity check on the helper itself: the project ships four
    // benches under `benches/` and one bin under `src/bin/`. If the
    // helper silently regresses, every downstream test below
    // becomes a tautology.
    let dirs = cargo_manifest_target_dirs();
    assert!(
        dirs.iter().any(|d| d == "benches"),
        "helper failed to discover the `benches/` directory from `[[bench]]` declarations \
         — without it the COPY-coverage test below would pass trivially. Got: {dirs:?}"
    );
    assert!(
        dirs.iter().any(|d| d == "src/bin"),
        "helper failed to discover the `src/bin/` directory from `[[bin]]` declarations. Got: {dirs:?}"
    );
}

#[test]
fn dockerignore_does_not_shadow_cargo_targets() {
    // A `.dockerignore` exclusion that matches a directory the
    // Dockerfile then tries to COPY produces:
    //   ERROR: failed to compute cache key: ".../<dir>": not found
    // — i.e. the COPY can't find the directory because dockerignore
    // hid it from the build context. This is the second half of the
    // bug Phase 8 task 00051 surfaced (the first half was Cargo.toml
    // declaring benches with no COPY; the fix added the COPY but
    // dockerignore was still masking `benches/`).
    let dockerignore =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(".dockerignore"))
            .expect("failed to read .dockerignore");
    let excluded: Vec<&str> = dockerignore
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('!'))
        .collect();
    for dir in cargo_manifest_target_dirs() {
        let prefix = dir.split('/').next().unwrap_or(&dir);
        for pattern in &excluded {
            let is_match = *pattern == dir
                || *pattern == format!("{dir}/")
                || *pattern == prefix
                || *pattern == format!("{prefix}/");
            assert!(
                !is_match,
                ".dockerignore excludes `{pattern}` but `Cargo.toml` declares a target rooted \
                 at `{dir}/` ([[bench]] / [[bin]] / [[test]]) that the Dockerfile must COPY. \
                 The exclusion hides the directory from the build context — the COPY would \
                 fail with `not found`. Remove the exclusion (or re-include with `!{dir}/`)."
            );
        }
    }
}

#[test]
fn dockerfile_copies_every_cargo_manifest_target_dir() {
    // For every directory declared by a `[[bench]]` / `[[bin]]` /
    // `[[test]]` block in Cargo.toml, the Dockerfile must either
    // COPY that exact directory or COPY a parent directory that
    // contains it. Otherwise `cargo build` inside the container
    // fails at manifest-parse time with:
    //   error: can't find `<name>` bench at `benches/<name>.rs`
    // — which is the bug Phase 8 task 00051 (GHCR publish) caught
    // on its first CI run.
    let dockerfile = read_dockerfile();
    let copy_lines: Vec<&str> = dockerfile
        .lines()
        .filter(|l| l.trim_start().starts_with("COPY") && !l.contains("--from=builder"))
        .collect();
    for dir in cargo_manifest_target_dirs() {
        // A directory `foo/bar` is "covered" if any COPY line
        // mentions `foo/bar` OR a prefix `foo/` OR a single
        // leading `./` variant. We don't try to parse the whole
        // COPY grammar — substring checks are enough for the
        // simple `COPY src/ src/` / `COPY benches/ benches/`
        // shape this Dockerfile uses, and the dockerignore test
        // already locks the file layout.
        let prefix = dir.split('/').next().unwrap_or(&dir);
        let covered = copy_lines.iter().any(|l| {
            l.contains(&format!("{dir}/"))
                || l.contains(&format!("{dir} "))
                || l.contains(&format!(" {prefix}/"))
                || l.contains(&format!(" ./{prefix}/"))
        });
        assert!(
            covered,
            "Cargo.toml declares a target rooted at `{dir}/` (via [[bench]] / [[bin]] / [[test]]) \
             but no COPY line in the Dockerfile pulls it into the builder stage. Add e.g. \
             `COPY {dir}/ {dir}/` before the `cargo build` step, or the container build will \
             fail at manifest-parse time. Current non-builder COPY lines:\n  {}",
            copy_lines.join("\n  ")
        );
    }
}
