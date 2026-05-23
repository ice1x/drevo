//! GitHub Actions workflow tests — `runs-on` labels.
//!
//! The drevo CI runs on a self-hosted (local) runner registered with
//! GitHub Actions. Every workflow job must select that runner via the
//! `self-hosted` label — never the GitHub-hosted ephemeral runners
//! (`ubuntu-latest`, `macos-latest`, `windows-latest`).
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
//!
//! The plain `runs-on: self-hosted` form is intentionally accepted —
//! drevo's CI currently uses a single self-hosted runner, so OS- and
//! arch-disambiguation labels (`Linux` / `macOS` / `X64` / `ARM64`)
//! would be tautological. If the fleet grows to multiple hosts the
//! workflows can be tightened to `[self-hosted, Linux, X64]` etc.
//! without changing these tests — they pin the *minimum* requirement,
//! not the exact label set.
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
                 `{forbidden}` — replace it with `self-hosted` (or a \
                 `[self-hosted, …]` label set)",
            );
        }
    }
}

#[test]
fn runs_on_label_is_either_bare_self_hosted_or_label_array() {
    // We accept two normal forms:
    //   1. `runs-on: self-hosted`               (bare scalar)
    //   2. `runs-on: [self-hosted, …]`          (array including
    //                                            `self-hosted`)
    // and reject anything else (e.g. matrix expressions that bypass
    // these tests, or scalar values other than `self-hosted`).
    let lines = all_runs_on_lines();
    for (file, line) in &lines {
        let value = line
            .strip_prefix("runs-on:")
            .map(str::trim)
            .unwrap_or_default();
        let ok = value == "self-hosted"
            || (value.starts_with('[') && value.ends_with(']') && value.contains("self-hosted"));
        assert!(
            ok,
            "{file}: `runs-on: {value}` is not in a supported form — \
             use either `runs-on: self-hosted` (bare) or \
             `runs-on: [self-hosted, …]` (label array)",
        );
    }
}

/// Helper: strip the `runs-on:` prefix and surrounding brackets,
/// returning the lower-cased label string for easy `contains` checks.
/// `runs-on: [self-hosted, Linux, X64]` → `self-hosted, linux, x64`.
/// `runs-on: self-hosted`                → `self-hosted`.
fn runs_on_labels(line: &str) -> String {
    line.strip_prefix("runs-on:")
        .map(str::trim)
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

#[test]
fn array_form_with_linux_label_implies_linux_routing() {
    // If a job uses the *array* form and includes a `Linux` label,
    // the workflow author is explicitly routing it to a Linux host.
    // That's only meaningful alongside `self-hosted` AND only on
    // jobs that actually need Linux. We don't try to read the steps
    // here (the heuristic is brittle); we just lock in the invariant
    // that `Linux` never appears without `self-hosted`. Symmetric for
    // `macOS` / `Windows`.
    //
    // This test deliberately does NOT fire on the bare-scalar form
    // (`runs-on: self-hosted`) — there, OS routing is the runner's
    // job, not the workflow's. It catches drift if a future array
    // form drops `self-hosted` while keeping `Linux` / `macOS` /
    // `Windows` (which would route to a GitHub-hosted runner under
    // some org configurations).
    let lines = all_runs_on_lines();
    for (file, line) in &lines {
        let labels = runs_on_labels(line);
        if !line.contains('[') {
            continue; // bare scalar — out of scope for this test
        }
        for os in &["linux", "macos", "windows"] {
            if labels.contains(os) {
                assert!(
                    labels.contains("self-hosted"),
                    "{file}: `{line}` includes `{os}` label without \
                     `self-hosted` — under some org configurations \
                     this would route to a GitHub-hosted runner",
                );
            }
        }
    }
}

#[test]
fn workflow_files_do_not_smuggle_runner_os_at_job_level() {
    // Regression test for commit 03c3909, reverted in 31ea23a.
    // `runner.os` is NOT available in job-level `if:` because the
    // runner context is populated only after a runner is assigned
    // to a job — job-level `if:` is evaluated before assignment.
    // GitHub rejects the whole workflow file with the generic
    // "This run likely failed because of a workflow file issue"
    // error, blocking every job in the file, not just the one with
    // the bad expression. This is a silent class of bug — local
    // YAML validation passes, GitHub-side validation fails — so we
    // pin it from the test side.
    //
    // Heuristic: any `if:` line that sits at the same indent as
    // `runs-on:` (i.e. is a JOB-level `if:`, not a step-level one)
    // and references `runner.` is rejected.
    let lines_by_file: Vec<(String, Vec<String>)> = workflow_files()
        .into_iter()
        .map(|p| {
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("<unknown>")
                .to_string();
            let body = fs::read_to_string(&p).unwrap_or_default();
            (name, body.lines().map(str::to_string).collect())
        })
        .collect();

    for (file, lines) in &lines_by_file {
        // Walk the file: track the indent of the most-recent
        // `<job_id>:` declaration and the most-recent `runs-on:`
        // line so we know what counts as "job-level".
        let mut job_indent: Option<usize> = None;
        for line in lines {
            let indent = line.chars().take_while(|c| *c == ' ').count();
            let trimmed = line.trim_start();
            if trimmed.starts_with("runs-on:") {
                // A `runs-on:` line tells us the job-attribute indent
                // (always exactly 4 spaces deep in our workflows, but
                // we derive it dynamically so the test is robust).
                job_indent = Some(indent);
                continue;
            }
            if let Some(ji) = job_indent {
                if indent == ji && trimmed.starts_with("if:") && trimmed.contains("runner.") {
                    panic!(
                        "{file}: job-level `if:` references `runner.` — \
                         this is rejected by GitHub's workflow validator \
                         because the `runner` context is only available \
                         at step level. Move the condition to each \
                         affected step's `if:`. Offending line:\n  {line}",
                    );
                }
            }
        }
    }
}
