//! GitHub Actions workflow tests — `runs-on` labels.
//!
//! drevo CI is **mixed-runner** by policy as of the Phase 10.5 CI
//! speedup work: stable-Rust PR jobs (check, test, clippy, fmt, doc,
//! msrv, k8s) run on `ubuntu-latest` so that they execute in parallel
//! on free GitHub-hosted runners; jobs that genuinely require a
//! persistent host (cargo-fuzz with nightly + libFuzzer + ASAN; Docker
//! multi-arch builds with QEMU) stay on the `self-hosted` runner.
//!
//! Why this changed: the earlier policy ("every job on self-hosted",
//! introduced by PR #74) optimised for Docker multi-arch + cargo-fuzz
//! but unintentionally serialised all PR validation through a single
//! runner-process. The resulting queue made `CI / Test` take 1-2 hours
//! per push, which broke the development cadence. Restoring
//! ubuntu-latest for stable-Rust gates gives back the parallelism
//! without giving up the niche-target benefits self-hosted provides.
//!
//! These tests pin the new policy as text-level invariants over the
//! workflow files in `.github/workflows/`:
//!
//! 1. Every `runs-on:` line references either `self-hosted` or one of
//!    the documented allow-listed GitHub-hosted runners (`ubuntu-latest`
//!    is the default; `macos-latest` / `windows-latest` are NOT used
//!    today and are also rejected — adding them is a deliberate design
//!    decision, not an accident).
//! 2. The `fuzz` job in `.github/workflows/ci.yml` MUST stay on
//!    `self-hosted` (cargo-fuzz preinstall + nightly Rust + ASAN +
//!    libFuzzer; running this on free runners would inflate the GitHub
//!    minutes budget without benefit).
//! 3. The Docker Publish workflow MUST stay on `self-hosted` — QEMU
//!    multi-arch builds are far slower on GitHub-hosted runners and
//!    consume the entire 6-hour timeout on cold runs.
//! 4. No `runner.os` reference in a *job-level* `if:` — that's
//!    rejected by GitHub's workflow validator (regression test for
//!    commit `03c3909`, reverted in `31ea23a`).
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

/// Allow-listed `runs-on:` scalar values. Anything else fails the test.
/// Keep this list short and intentional: every entry is a deliberate
/// policy decision. New aliases require a code review explaining why
/// the workflow needs them.
const ALLOWED_RUNS_ON: &[&str] = &["self-hosted", "ubuntu-latest"];

#[test]
fn every_runs_on_uses_an_allow_listed_runner() {
    let lines = all_runs_on_lines();
    for (file, line) in &lines {
        let value = line
            .strip_prefix("runs-on:")
            .map(str::trim)
            .unwrap_or_default();

        // Accept array form `[self-hosted, …]` if every comma-separated
        // entry is allow-listed.
        let ok = if value.starts_with('[') && value.ends_with(']') {
            value
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(str::trim)
                .all(|label| {
                    // OS / arch disambiguation labels are allowed in
                    // arrays as long as `self-hosted` is also present —
                    // this is enforced below by `array_form_with_os_label_requires_self_hosted`.
                    ALLOWED_RUNS_ON.contains(&label)
                        || matches!(label, "Linux" | "macOS" | "Windows" | "X64" | "ARM64")
                })
        } else {
            ALLOWED_RUNS_ON.contains(&value)
        };

        assert!(
            ok,
            "{file}: `runs-on: {value}` is not allow-listed — supported \
             values are {ALLOWED_RUNS_ON:?} (or an array including \
             `self-hosted` plus OS/arch labels). Adding a new value \
             requires updating ALLOWED_RUNS_ON with a comment \
             explaining why.",
        );
    }
}

#[test]
fn fuzz_job_in_ci_must_be_self_hosted() {
    // The `fuzz` job in ci.yml requires nightly Rust, libFuzzer, and
    // AddressSanitizer — moving it to ubuntu-latest would mean
    // installing cargo-fuzz cold on every PR (~2 min) plus burning
    // GitHub minutes on 3 × 60s smoke runs. The self-hosted runner
    // has cargo-fuzz cached and consumes no GitHub-hosted minutes.
    let ci_yml = workflows_dir().join("ci.yml");
    let body = fs::read_to_string(&ci_yml).expect("ci.yml exists");
    let mut in_fuzz_job = false;
    let mut fuzz_runs_on: Option<String> = None;
    for line in body.lines() {
        let trimmed = line.trim_start();
        // Job declarations are indented 2 spaces and end with `:`.
        if line.starts_with("  ") && !line.starts_with("    ") && trimmed.ends_with(':') {
            let job = trimmed.trim_end_matches(':');
            in_fuzz_job = job == "fuzz";
        }
        if in_fuzz_job && trimmed.starts_with("runs-on:") {
            fuzz_runs_on = Some(trimmed.to_string());
            break;
        }
    }
    let line = fuzz_runs_on.expect("ci.yml fuzz job must declare `runs-on:`");
    assert!(
        line.contains("self-hosted"),
        "ci.yml fuzz job must run on self-hosted (found `{line}`). \
         Moving it to a GitHub-hosted runner would mean installing \
         cargo-fuzz + nightly Rust cold on every PR and consuming \
         GitHub minutes — see the per-job comment in ci.yml for the \
         full rationale.",
    );
}

#[test]
fn docker_publish_job_must_be_self_hosted() {
    // Docker multi-arch (linux/amd64 + linux/arm64 via QEMU) on
    // ubuntu-latest can exceed 30 minutes per build because arm64
    // emulation is slow. On self-hosted with warm layer cache it
    // settles to ~5-10 min. This invariant prevents accidental
    // downgrades.
    let docker_yml = workflows_dir().join("docker-publish.yml");
    if !docker_yml.exists() {
        // If the file is renamed, the rename should ship with this test
        // updated; until then the absence is a soft signal.
        return;
    }
    let body = fs::read_to_string(&docker_yml).expect("docker-publish.yml exists");
    let mut found_self_hosted = false;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("runs-on:") && trimmed.contains("self-hosted") {
            found_self_hosted = true;
            break;
        }
    }
    assert!(
        found_self_hosted,
        "docker-publish.yml has no `runs-on: self-hosted` — multi-arch \
         Docker builds via QEMU MUST run on the persistent self-hosted \
         runner so warm-layer cache survives between runs and so GitHub \
         minutes don't get exhausted by ARM64 emulation.",
    );
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
fn array_form_with_os_label_requires_self_hosted() {
    // If a job uses the *array* form and includes a `Linux` / `macOS` /
    // `Windows` label, the workflow author is explicitly routing it to
    // a specific OS. That's only meaningful alongside `self-hosted` AND
    // only on jobs that actually need that OS. We don't try to read the
    // steps here (the heuristic is brittle); we just lock in the
    // invariant that an OS label never appears in an array form without
    // `self-hosted` (which would route to GitHub-hosted runners under
    // some org configurations).
    //
    // This test deliberately does NOT fire on bare scalars
    // (`runs-on: ubuntu-latest` or `runs-on: self-hosted`).
    let lines = all_runs_on_lines();
    for (file, line) in &lines {
        if !line.contains('[') {
            continue;
        }
        let labels = runs_on_labels(line);
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

#[test]
fn ci_yml_declares_concurrency_cancel_in_progress() {
    // The single biggest source of CI wait time on rapid-fire PR
    // pushes was the *queue*, not any individual job: every push
    // stacked behind every prior push on the same branch. Adding
    // workflow-level `concurrency: cancel-in-progress: true` cuts
    // the queue to length 1 per branch. This invariant locks that
    // setting in so a future edit doesn't silently revert it.
    let ci_yml = workflows_dir().join("ci.yml");
    let body = fs::read_to_string(&ci_yml).expect("ci.yml exists");
    assert!(
        body.contains("concurrency:"),
        "ci.yml must declare a `concurrency:` block — without it, \
         rapid-fire PR pushes stack into the runner queue and CI \
         latency balloons. See the comment above the block in \
         ci.yml for the full rationale."
    );
    assert!(
        body.contains("cancel-in-progress: true"),
        "ci.yml `concurrency:` block must set \
         `cancel-in-progress: true` — otherwise the cancel does not \
         take effect and stale runs continue blocking the queue."
    );
}

/// The docs-only path set — these glob patterns MUST appear in
/// `ci.yml`'s `paths-ignore:` (so the real CI is skipped for them) AND
/// in `ci-skip.yml`'s `paths:` (so the skip workflow runs and emits
/// passing required checks). The set is duplicated in BOTH workflow
/// files; the tests below assert that every element appears in both
/// places. The list is also re-stated here as the single source of
/// truth so a future edit cannot silently drift one of the files.
const DOCS_ONLY_GLOBS: &[&str] = &["**/*.md", "audit/**", "memory/**", "LICENSE", ".gitignore"];

#[test]
fn ci_yml_skips_docs_only_changes_via_paths_ignore() {
    let ci_yml = workflows_dir().join("ci.yml");
    let body = fs::read_to_string(&ci_yml).expect("ci.yml exists");
    assert!(
        body.contains("paths-ignore:"),
        "ci.yml must declare `paths-ignore:` on its triggers so \
         docs-only PRs (README, audit/, memory/) do not consume CI \
         minutes. Pair with `.github/workflows/ci-skip.yml` for the \
         required-check pass-through."
    );
    for glob in DOCS_ONLY_GLOBS {
        assert!(
            body.contains(glob),
            "ci.yml `paths-ignore:` is missing the docs-only glob \
             `{glob}` — the set must be {DOCS_ONLY_GLOBS:?} and stay \
             in sync with the same list in ci-skip.yml."
        );
    }
}

#[test]
fn ci_skip_yml_exists() {
    let ci_skip_yml = workflows_dir().join("ci-skip.yml");
    assert!(
        ci_skip_yml.exists(),
        "`.github/workflows/ci-skip.yml` must exist — it is the \
         pass-through workflow that emits successful required \
         status checks for docs-only PRs that bypass ci.yml. \
         Without it, docs-only PRs would be blocked by branch \
         protection waiting on checks that never run."
    );
}

#[test]
fn ci_skip_yml_triggers_on_docs_only_paths() {
    let ci_skip_yml = workflows_dir().join("ci-skip.yml");
    let body = fs::read_to_string(&ci_skip_yml).expect("ci-skip.yml exists");
    assert!(
        body.contains("paths:"),
        "ci-skip.yml must declare `paths:` (not paths-ignore) — it \
         must run ONLY when changed files match the docs-only set, \
         otherwise it would duplicate every real CI run."
    );
    for glob in DOCS_ONLY_GLOBS {
        assert!(
            body.contains(glob),
            "ci-skip.yml `paths:` is missing the docs-only glob \
             `{glob}` — the set must be {DOCS_ONLY_GLOBS:?} and stay \
             in sync with the same list in ci.yml's paths-ignore."
        );
    }
}

#[test]
fn ci_skip_yml_workflow_name_matches_ci_yml() {
    // Branch protection groups required status checks by
    // `<workflow name> / <job name>`. If ci-skip.yml renames itself
    // to anything other than `CI`, its jobs become "CI-Skip / Test"
    // instead of "CI / Test" and stop satisfying the required check.
    let ci_skip_yml = workflows_dir().join("ci-skip.yml");
    let body = fs::read_to_string(&ci_skip_yml).expect("ci-skip.yml exists");
    let has_ci_name = body.lines().any(|line| line.trim() == "name: CI");
    assert!(
        has_ci_name,
        "ci-skip.yml must declare exactly `name: CI` at the workflow \
         level — branch protection identifies required checks by \
         workflow name and ci.yml is `name: CI`. Any other value \
         (e.g. `name: CI Skip`) breaks the pass-through pattern."
    );
}

/// Job-name pairs `(id, display_name)` extracted from a workflow body.
/// The display name is the `name:` value on the job, when present;
/// fallback to the id itself if no explicit `name:` is given (GitHub's
/// own default).
fn job_names_in(workflow_body: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut current_id: Option<String> = None;
    let mut current_display: Option<String> = None;
    let mut in_jobs = false;
    for line in workflow_body.lines() {
        let trimmed = line.trim_start();
        // Top-level `jobs:` declaration sits at column 0.
        if trimmed == "jobs:" && line.starts_with("jobs:") {
            in_jobs = true;
            continue;
        }
        if !in_jobs {
            continue;
        }
        // A new top-level key at column 0 ends the jobs section.
        if !line.is_empty()
            && !line.starts_with(' ')
            && !line.starts_with('#')
            && line.ends_with(':')
            && line.trim_end_matches(':') != "jobs"
        {
            break;
        }
        // Job declarations are indented 2 spaces and end with `:`.
        if line.starts_with("  ")
            && !line.starts_with("    ")
            && trimmed.ends_with(':')
            && !trimmed.starts_with('#')
            && !trimmed.starts_with("- ")
            && trimmed != "steps:"
            && trimmed != "env:"
        {
            // Flush previous job.
            if let Some(id) = current_id.take() {
                let display = current_display.take().unwrap_or_else(|| id.clone());
                out.push((id, display));
            }
            current_id = Some(trimmed.trim_end_matches(':').to_string());
            current_display = None;
            continue;
        }
        // Job-level `name:` (4-space indent inside the job).
        if line.starts_with("    name:") && current_display.is_none() {
            let value = line.trim_start().trim_start_matches("name:").trim();
            current_display = Some(value.to_string());
        }
    }
    if let Some(id) = current_id {
        let display = current_display.unwrap_or_else(|| id.clone());
        out.push((id, display));
    }
    out
}

/// Extract the workflow-level `concurrency.group` literal from a
/// workflow file body. Returns `None` if no top-level `concurrency:`
/// block is present.
fn workflow_concurrency_group(body: &str) -> Option<String> {
    let mut in_concurrency = false;
    let mut concurrency_indent: usize = 0;
    for line in body.lines() {
        let indent = line.chars().take_while(|c| *c == ' ').count();
        let trimmed = line.trim_start();
        if !in_concurrency {
            if trimmed.starts_with("concurrency:") && indent == 0 {
                in_concurrency = true;
                concurrency_indent = indent;
            }
            continue;
        }
        // We left the concurrency block once we see a line at the same
        // top-level indent that opens a new key.
        if !line.is_empty()
            && indent <= concurrency_indent
            && trimmed.ends_with(':')
            && !trimmed.starts_with('#')
        {
            return None;
        }
        if let Some(rest) = trimmed.strip_prefix("group:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

#[test]
fn ci_yml_and_ci_skip_yml_have_distinct_concurrency_groups() {
    // Bug recovered from the third commit on PR #76: both ci.yml and
    // ci-skip.yml declared `name: CI`, so `${{ github.workflow }}`
    // resolved to the same string in their concurrency `group:` keys.
    // The two workflows then shared a single concurrency queue, and
    // `cancel-in-progress: true` meant whichever workflow started
    // second (typically the cheap skip workflow on mixed PRs) would
    // CANCEL the real CI before it ran a single test. Branch
    // protection then saw the skip workflow's "Success" status as
    // the latest result for required check names (`CI / Test`,
    // `CI / Check`, …) and let merges through with no real CI
    // validation. This invariant pins the fix: the two workflows
    // MUST use distinct `group:` literals so neither cancels the
    // other.
    let ci_yml = workflows_dir().join("ci.yml");
    let ci_skip_yml = workflows_dir().join("ci-skip.yml");
    let ci_body = fs::read_to_string(&ci_yml).expect("ci.yml exists");
    let skip_body = fs::read_to_string(&ci_skip_yml).expect("ci-skip.yml exists");

    let ci_group = workflow_concurrency_group(&ci_body)
        .expect("ci.yml must declare workflow-level concurrency.group");
    let skip_group = workflow_concurrency_group(&skip_body)
        .expect("ci-skip.yml must declare workflow-level concurrency.group");

    assert_ne!(
        ci_group, skip_group,
        "ci.yml and ci-skip.yml MUST use distinct concurrency `group:` \
         literals. Both currently use `{ci_group}`. With identical \
         groups + `cancel-in-progress: true` the faster skip workflow \
         silently cancels the real CI on mixed PRs and branch \
         protection passes on the skip's `echo` jobs instead of the \
         real test suite."
    );

    // Belt-and-braces: also forbid the specific anti-pattern of using
    // `${{ github.workflow }}` in either group, since both workflows
    // share `name: CI` and that expression resolves identically.
    for (label, group) in [("ci.yml", &ci_group), ("ci-skip.yml", &skip_group)] {
        assert!(
            !group.contains("github.workflow"),
            "{label}: concurrency `group: {group}` uses `github.workflow` \
             — both workflows declare `name: CI`, so this expression \
             resolves to the same string in both files and collapses \
             their queues. Hard-code a distinct literal (e.g. \
             `ci-real-${{{{ github.ref }}}}` for the real workflow, \
             `ci-skip-${{{{ github.ref }}}}` for the skip workflow)."
        );
    }
}

#[test]
fn ci_skip_yml_mirrors_every_required_job_name_from_ci_yml() {
    // The skip workflow MUST mirror every required-check job name
    // from ci.yml. The one allowed exception is `fuzz`, which is
    // `continue-on-error: true` and is not (and must not be) a
    // required branch-protection check.
    let ci_yml = workflows_dir().join("ci.yml");
    let ci_skip_yml = workflows_dir().join("ci-skip.yml");
    let ci_body = fs::read_to_string(&ci_yml).expect("ci.yml exists");
    let skip_body = fs::read_to_string(&ci_skip_yml).expect("ci-skip.yml exists");

    let ci_names = job_names_in(&ci_body);
    let skip_names = job_names_in(&skip_body);

    let ci_display: Vec<&String> = ci_names
        .iter()
        .filter(|(id, _)| id != "fuzz")
        .map(|(_, d)| d)
        .collect();
    let skip_display: Vec<&String> = skip_names.iter().map(|(_, d)| d).collect();

    // Every required ci.yml display name must appear in ci-skip.yml.
    for required in &ci_display {
        assert!(
            skip_display.contains(required),
            "ci-skip.yml is missing a mirror job for ci.yml's \
             `name: {required}` — the required status check \
             `CI / {required}` would not pass on docs-only PRs. \
             Add a matching job to ci-skip.yml (single `echo` \
             step on ubuntu-latest)."
        );
    }

    // ci-skip.yml should not have extra jobs beyond ci.yml's set
    // (minus fuzz) — that would imply branch protection enforces a
    // check that has no real CI counterpart.
    for present in &skip_display {
        let in_ci = ci_display.iter().any(|d| d == present);
        assert!(
            in_ci,
            "ci-skip.yml has a job `name: {present}` that has no \
             counterpart in ci.yml — either remove it from \
             ci-skip.yml or add the real job to ci.yml. Stray skip \
             jobs imply phantom required checks."
        );
    }
}

// ---- bench.yml invariants ------------------------------------------------
//
// `bench.yml` runs criterion benches nightly on the self-hosted runner.
// It MUST NOT be triggered on PR or push, otherwise we re-introduce the
// four-hour CI debacle the bench-removal fix (PR #77) just resolved.
// These tests pin the contract.

#[test]
fn bench_yml_exists() {
    let bench_yml = workflows_dir().join("bench.yml");
    assert!(
        bench_yml.exists(),
        "`.github/workflows/bench.yml` must exist — it is the \
         scheduled nightly criterion bench workflow. Without it, \
         we have no perf-regression signal at all (since PR-gating \
         CI was changed to skip benches)."
    );
}

#[test]
fn bench_yml_is_scheduled_at_4am_utc() {
    // Pin the cron expression. Nightly cadence is non-negotiable
    // (we don't want benches running ad-hoc on the self-hosted
    // runner; they'd block PR jobs that are queued there for
    // fuzz / Docker). 04:00 UTC is the agreed quiet window.
    let bench_yml = workflows_dir().join("bench.yml");
    let body = fs::read_to_string(&bench_yml).expect("bench.yml exists");
    let has_cron = body
        .lines()
        .any(|line| line.trim() == "- cron: '0 4 * * *'");
    assert!(
        has_cron,
        "bench.yml must declare exactly `- cron: '0 4 * * *'` \
         (daily at 04:00 UTC). Changing the time is a deliberate \
         operational decision — update this test together with the \
         cron expression and explain the new time in the workflow \
         comment block."
    );
}

#[test]
fn bench_yml_supports_workflow_dispatch() {
    // Manual trigger is required so a developer can run benches
    // on demand before / after touching perf-sensitive code without
    // waiting for the nightly schedule.
    let bench_yml = workflows_dir().join("bench.yml");
    let body = fs::read_to_string(&bench_yml).expect("bench.yml exists");
    assert!(
        body.contains("workflow_dispatch:"),
        "bench.yml must declare `workflow_dispatch:` so the bench \
         suite can be triggered on demand from the Actions UI."
    );
}

#[test]
fn bench_yml_runs_on_self_hosted() {
    // Benchmark numbers MUST come from a stable host. GitHub-hosted
    // ubuntu-latest VMs have variable noise floors that swamp the
    // signal we're trying to measure.
    let bench_yml = workflows_dir().join("bench.yml");
    let body = fs::read_to_string(&bench_yml).expect("bench.yml exists");
    let bench_runs_on = body
        .lines()
        .find(|line| line.trim_start().starts_with("runs-on:"))
        .map(str::to_string)
        .expect("bench.yml must declare `runs-on:`");
    assert!(
        bench_runs_on.contains("self-hosted"),
        "bench.yml `runs-on:` must include `self-hosted` (found \
         `{bench_runs_on}`). Benchmark numbers from ephemeral \
         GitHub-hosted VMs are dominated by neighbour-VM noise."
    );
}

#[test]
fn bench_yml_does_not_trigger_on_pr_or_push() {
    // The ENTIRE reason for this workflow's existence: it must NOT
    // re-introduce the four-hour CI debacle by accidentally
    // triggering on PR or push. Cron + workflow_dispatch only.
    let bench_yml = workflows_dir().join("bench.yml");
    let body = fs::read_to_string(&bench_yml).expect("bench.yml exists");
    // Walk the `on:` block (top-level, indent 0) until we leave it.
    let mut in_on_block = false;
    let mut on_indent: usize = 0;
    let mut forbidden_hits: Vec<String> = Vec::new();
    for line in body.lines() {
        let indent = line.chars().take_while(|c| *c == ' ').count();
        let trimmed = line.trim_start();
        if !in_on_block {
            if trimmed.starts_with("on:") && indent == 0 {
                in_on_block = true;
                on_indent = indent;
            }
            continue;
        }
        // Leave the on: block on the next top-level key.
        if !line.is_empty()
            && indent <= on_indent
            && !trimmed.starts_with('#')
            && trimmed.ends_with(':')
        {
            break;
        }
        // Top-level trigger keys inside `on:` sit at indent 2 in our
        // workflow style. Forbid `push:` and `pull_request:`.
        if indent == on_indent + 2
            && (trimmed.starts_with("push:") || trimmed.starts_with("pull_request:"))
        {
            forbidden_hits.push(trimmed.to_string());
        }
    }
    assert!(
        forbidden_hits.is_empty(),
        "bench.yml declares a PR/push trigger (`{}`). Benches MUST \
         NOT run on PR or push — that path is what caused the \
         four-hour CI runs on PR #76. Allowed triggers are \
         `schedule:` (nightly cron) and `workflow_dispatch:` \
         (manual) only.",
        forbidden_hits.join("`, `")
    );
}
