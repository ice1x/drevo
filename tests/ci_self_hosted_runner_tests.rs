//! GitHub Actions workflow tests — `runs-on` labels.
//!
//! The drevo CI runs on a self-hosted (local) runner registered with
//! GitHub Actions. Every workflow job must select that runner via the
//! `self-hosted` label group — never the GitHub-hosted ephemeral
//! runners (`ubuntu-latest`, `macos-latest`, `windows-latest`).
//!
//! These tests parse each workflow file as text and assert:
//!
//! 1. Every `runs-on:` line in the workflow either selects a
//!    self-hosted runner (`self-hosted` appears in its label set) OR
//!    is a workflow-level matrix expression that resolves to a
//!    self-hosted runner.
//! 2. No `runs-on:` line points at a GitHub-hosted ephemeral runner
//!    (`ubuntu-latest`, `ubuntu-22.04`, `macos-latest`, `macos-13`,
//!    `windows-latest`, etc.).
//! 3. The standard OS labels (`Linux`, `macOS`) appear alongside
//!    `self-hosted` so a multi-OS self-hosted fleet can target the
//!    right host.
//!
//! Pure-text tests: no `act`, no Docker, no GitHub API. They mirror
//! the pattern of `tests/docker_publish_ci_tests.rs` and
//! `tests/k8s_manifests_tests.rs`.

use std::fs;
use std::path::{Path, PathBuf};

fn workflows_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".github")
        .join("workflows")
}

fn workflow_files() -> Vec<PathBuf> {
    fs::read_dir(workflows_dir())
        .expect("`.github/workflows/` must exist")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let ext = path.extension()?.to_string_lossy().to_lowercase();
            if ext == "yml" || ext == "yaml" {
                Some(path)
            } else {
                None
            }
        })
        .collect()
}

/// All `runs-on:` lines across every workflow file, paired with the
/// originating file's name for diagnostic messages.
fn all_runs_on_lines() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for path in workflow_files() {
        let file_label = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let body = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        for line in body.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("runs-on:") {
                out.push((file_label.clone(), trimmed.to_string()));
            }
        }
    }
    out
}

#[test]
fn workflows_directory_contains_yaml_files() {
    let files = workflow_files();
    assert!(
        !files.is_empty(),
        "`.github/workflows/` must contain at least one workflow"
    );
}

#[test]
fn at_least_one_runs_on_directive_exists() {
    let lines = all_runs_on_lines();
    assert!(
        !lines.is_empty(),
        "no `runs-on:` directives found across workflows — that cannot be right"
    );
}

#[test]
fn every_runs_on_selects_self_hosted_runner() {
    let lines = all_runs_on_lines();
    for (file, line) in &lines {
        assert!(
            line.contains("self-hosted"),
            "{file}: `{line}` does not target the self-hosted runner — \
             every job must include `self-hosted` in its `runs-on` label \
             set so CI runs on the local runner, not on GitHub-hosted \
             ephemeral VMs",
        );
    }
}

#[test]
fn no_github_hosted_ephemeral_runner_is_referenced() {
    // The full set of GitHub-hosted runner aliases we explicitly
    // forbid. New aliases (e.g. `ubuntu-26.04` once it ships) should
    // be added here as they appear.
    const FORBIDDEN: &[&str] = &[
        "ubuntu-latest",
        "ubuntu-22.04",
        "ubuntu-24.04",
        "ubuntu-20.04",
        "macos-latest",
        "macos-13",
        "macos-14",
        "macos-15",
        "windows-latest",
        "windows-2022",
        "windows-2019",
    ];
    let lines = all_runs_on_lines();
    for (file, line) in &lines {
        for forbidden in FORBIDDEN {
            assert!(
                !line.contains(forbidden),
                "{file}: `{line}` references the GitHub-hosted runner \
                 `{forbidden}` — replace it with a `[self-hosted, …]` \
                 label set",
            );
        }
    }
}

#[test]
fn linux_jobs_carry_the_linux_label() {
    // A heuristic: any `runs-on:` line that names a Linux-style label
    // (or omits an OS label entirely) must mention `Linux` explicitly
    // so the self-hosted fleet can route the job to a Linux host.
    // We exempt lines that mention `macOS` / `Windows` — those are
    // tested separately.
    let lines = all_runs_on_lines();
    for (file, line) in &lines {
        let mentions_mac = line.contains("macOS") || line.contains("macos");
        let mentions_win = line.contains("Windows") || line.contains("windows");
        if mentions_mac || mentions_win {
            continue;
        }
        assert!(
            line.contains("Linux"),
            "{file}: `{line}` is a Linux job but does not include the \
             `Linux` label — add it so the self-hosted fleet can route \
             correctly",
        );
    }
}

#[test]
fn macos_jobs_use_self_hosted_macos_label() {
    let lines = all_runs_on_lines();
    for (file, line) in &lines {
        let lower = line.to_ascii_lowercase();
        // We only look at jobs that *previously* targeted macOS —
        // detect by the presence of `macOS` (any case) as a label.
        // If `macos-latest` slipped back in, the previous test caught
        // it; here we just verify that legitimate macOS jobs are
        // self-hosted.
        if lower.contains("macos") {
            assert!(
                line.contains("self-hosted"),
                "{file}: `{line}` is a macOS job but does not include \
                 `self-hosted` — the macOS CI runner must be local too",
            );
        }
    }
}
