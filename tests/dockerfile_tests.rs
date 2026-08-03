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
fn dockerfile_enables_bolt_by_default() {
    // Task 00163: the container serves the Neo4j-compatible Bolt listener by
    // default (DREVO_BOLT_PORT set) and exposes its port.
    let content = read_dockerfile();
    assert!(
        content.lines().any(|l| l.contains("DREVO_BOLT_PORT")),
        "Dockerfile must set DREVO_BOLT_PORT so the container serves Bolt"
    );
    assert!(
        content
            .lines()
            .any(|l| l.trim_start().starts_with("EXPOSE") && l.contains("7687")),
        "Dockerfile must EXPOSE the Bolt port 7687"
    );
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
fn dockerfile_features_are_build_arg_overridable() {
    // The compiled Cargo feature set is a build ARG (`CARGO_FEATURES`) so a
    // deployment can override it via `docker build --build-arg` without editing
    // the Dockerfile. The deploy image's DEFAULT ships the full server —
    // http + redb-backend + embeddings-proxy — so `/v1/embeddings` (issue #217)
    // is available out of the box (runtime-gated to 503 until configured); the
    // build must consume the ARG rather than a hardcoded feature list.
    let content = read_dockerfile();
    let arg_line = content
        .lines()
        .find(|l| l.trim_start().starts_with("ARG CARGO_FEATURES"))
        .expect("Dockerfile must declare `ARG CARGO_FEATURES` for an overridable feature set");
    assert!(
        arg_line.contains("http") && arg_line.contains("redb-backend"),
        "the default CARGO_FEATURES must keep http + redb-backend: {arg_line}"
    );
    assert!(
        arg_line.contains("embeddings-proxy"),
        "the deploy image must ship embeddings-proxy so /v1/embeddings is available \
         (runtime-gated to 503 until configured); override to lean via --build-arg: {arg_line}"
    );
    assert!(
        content.contains("${CARGO_FEATURES}"),
        "the cargo build must take features from ${{CARGO_FEATURES}}, not a hardcoded list"
    );
    assert!(
        content.contains("--no-default-features"),
        "the build must stay --no-default-features so only the ARG features compile"
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
fn dockerfile_copies_static_web_assets() {
    // The embedded Web UI (task 00092) is compiled INTO the binary via
    // `include_str!("../static/web/…")` in src/web_ui.rs, so the builder stage
    // must COPY `static/` into the build context or the release build fails
    // with "couldn't read static/web/styles.css". Regression source: task
    // 00163 — the container had never been built end-to-end and silently
    // lacked this COPY.
    let content = read_dockerfile();
    let has_copy = content
        .lines()
        .any(|l| l.trim_start().starts_with("COPY") && l.contains("static/"));
    assert!(
        has_copy,
        "Dockerfile must COPY static/ into the builder (web_ui include_str! assets)"
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

#[test]
fn dockerfile_threads_the_version_build_arg() {
    // `.git` is excluded from the build context, so `build.rs` can't
    // `git describe` inside the image and would fall back to CARGO_PKG_VERSION
    // (0.0.0). The version must arrive as a build ARG and be exported to the
    // build env so `build.rs` picks it up. `scripts/release.sh` supplies it.
    let content = read_dockerfile();
    assert!(
        content.contains("ARG DREVO_VERSION"),
        "Dockerfile must declare `ARG DREVO_VERSION` so the release version can be injected"
    );
    assert!(
        content.contains("ENV DREVO_VERSION"),
        "Dockerfile must export DREVO_VERSION to the build env for build.rs to read"
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

/// Read all top-level `[[bench]]` / `[[bin]]` / `[[test]]` / `[[example]]`
/// blocks from `Cargo.toml` and return the directory each block points at
/// (e.g. `benches`, `src/bin`, `tests`, `examples`). The Dockerfile must COPY
/// every directory in the returned set into the builder stage, or manifest
/// parsing fails inside the container — cargo validates every declared
/// target's path when it parses the manifest, even for `cargo build --bin`,
/// and `required-features` gates *building* a target, not the path check.
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
        } else if trimmed.starts_with("[[example]]") {
            current_section = Some("example");
        } else if trimmed.starts_with('[') {
            current_section = None;
        } else if let Some(section) = current_section {
            // Either an explicit `path = "..."` or rely on the
            // convention: benches/<name>.rs, src/bin/<name>.rs,
            // tests/<name>.rs, examples/<name>.rs.
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
                    "example" => "examples",
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
    assert!(
        dirs.iter().any(|d| d == "examples"),
        "helper failed to discover the `examples/` directory from `[[example]]` declarations \
         (the #241 load-harness targets) — the gap that broke `make release-image`. Got: {dirs:?}"
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

/// Read every directory listed in `[workspace] members = [...]` of the
/// root `Cargo.toml`. The Dockerfile must COPY each member directory
/// into the builder stage — otherwise cargo refuses to load the
/// workspace manifest *even when the target being built does not depend
/// on the member* (the regression task `00115` first surfaced — Docker
/// Publish failed with "failed to load manifest for workspace member
/// `/build/drevo-py`" after `drevo-py` joined the workspace but did not
/// land in the Dockerfile).
fn cargo_workspace_member_dirs() -> Vec<String> {
    let cargo_toml = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("failed to read Cargo.toml");
    let mut dirs: Vec<String> = Vec::new();
    let mut in_workspace = false;
    let mut buffer = String::new();
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if trimmed == "[workspace]" {
            in_workspace = true;
            continue;
        }
        if in_workspace && trimmed.starts_with('[') {
            break;
        }
        if !in_workspace {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("members") {
            // Collect everything until we hit a `]` so we can handle
            // both single-line `members = [".", "drevo-py"]` and
            // multi-line forms.
            buffer.clear();
            buffer.push_str(rest);
        } else if !buffer.is_empty() {
            buffer.push(' ');
            buffer.push_str(trimmed);
        }
        if buffer.contains(']') {
            // Extract every quoted string between '[' and ']'.
            let start = buffer.find('[').unwrap_or(0);
            let end = buffer.find(']').unwrap_or(buffer.len());
            for part in buffer[start + 1..end].split(',') {
                let entry = part.trim().trim_matches('"').to_string();
                // Skip the root crate (".") and entries with glob chars.
                if entry.is_empty() || entry == "." || entry.contains('*') {
                    continue;
                }
                dirs.push(entry);
            }
            buffer.clear();
            // members = [...] only appears once; stop scanning.
            break;
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

#[test]
fn cargo_workspace_member_collector_finds_drevo_py() {
    // Phase 16 task 00115 added `drevo-py` as the first non-root
    // workspace member. If the collector silently regresses, the
    // downstream COPY-coverage test below becomes a tautology.
    let dirs = cargo_workspace_member_dirs();
    assert!(
        dirs.iter().any(|d| d == "drevo-py"),
        "helper failed to discover `drevo-py` from the [workspace] members list. Got: {dirs:?}"
    );
}

#[test]
fn dockerfile_copies_every_workspace_member_dir() {
    // For every non-root entry in `[workspace] members = [...]`, the
    // Dockerfile must COPY that directory into the builder stage. Cargo
    // refuses to load the workspace manifest if a declared member's
    // Cargo.toml is missing — even when `cargo build --bin <name>`
    // targets a binary that does not depend on the missing member.
    //
    // Regression source: Phase 16 task 00115 promoted the repo to a
    // workspace with `drevo-py` as the second member but the
    // accompanying Dockerfile patch was missed on the first commit.
    // The CI Docker Publish job failed on the next PR push with:
    //   error: failed to load manifest for workspace member `/build/drevo-py`
    // This test would have caught it at `cargo test` time before push.
    let dockerfile = read_dockerfile();
    let copy_lines: Vec<&str> = dockerfile
        .lines()
        .filter(|l| l.trim_start().starts_with("COPY") && !l.contains("--from=builder"))
        .collect();
    for dir in cargo_workspace_member_dirs() {
        let covered = copy_lines.iter().any(|l| {
            l.contains(&format!("{dir}/"))
                || l.contains(&format!("{dir} "))
                || l.contains(&format!(" ./{dir}/"))
        });
        assert!(
            covered,
            "[workspace] members declares `{dir}` but no COPY line in the Dockerfile pulls it \
             into the builder stage. Add e.g. `COPY {dir}/ {dir}/` before the `cargo build` step, \
             or the container build will fail at manifest-parse time with:\n  \
             error: failed to load manifest for workspace member `/build/{dir}`\n\
             Current non-builder COPY lines:\n  {}",
            copy_lines.join("\n  ")
        );
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
