//! GitHub Actions workflow tests — `runs-on` labels.
//!
//! drevo CI is **fully GitHub-hosted** as of the open-source move: the
//! self-hosted runner is retired. Every job runs on an ephemeral GitHub-hosted
//! runner (`ubuntu-latest`, plus `macos-latest` for the platform-native
//! iOS/macOS/wheel jobs), so all validation runs in parallel and there is no
//! persistent host to queue behind.
//!
//! Why this changed: the repo is going public. For public repos GitHub-hosted
//! minutes are free and unlimited, so the only reason for self-hosted (the
//! private-repo minute budget) is gone. More importantly, a self-hosted runner
//! on a PUBLIC repo is a security hole — a malicious fork PR can execute
//! arbitrary code on the host — so self-hosted MUST be gone before the repo is
//! made public. This also fixes the chronic single-runner serialization that
//! made `CI / Test` queue for 1-2 hours per push.
//!
//! These tests pin the policy as text-level invariants over the workflow files
//! in `.github/workflows/`:
//!
//! 1. Every `runs-on:` line references one of the documented allow-listed
//!    GitHub-hosted runners (`ubuntu-latest` is the default; `macos-latest` +
//!    `windows-latest` only where a platform-native reason applies — Python
//!    wheels/CI matrix and the iOS/macOS cross-compile targets).
//! 2. The `fuzz` job in `.github/workflows/ci.yml` runs on `ubuntu-latest`
//!    (cargo-fuzz + nightly + libFuzzer + ASAN all work on Linux GitHub runners;
//!    free on a public repo).
//! 3. The Docker Publish workflow runs on `ubuntu-latest` (its tag-only
//!    multi-arch build must never block CI on a shared runner; the `type=gha`
//!    layer cache bounds the per-build cost).
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
///
/// * `self-hosted` — retained in the allow-list only so the array-form runner
///   machinery below keeps compiling; NO workflow uses it anymore (the runner
///   is retired). fuzz + Docker Publish run on ubuntu-latest — see
///   fuzz_job_in_ci_must_be_github_hosted + docker_publish_job_must_be_github_hosted.
/// * `ubuntu-latest` — the default GitHub-hosted runner for every stable-Rust
///   job (check, test, clippy, fmt, doc, msrv, k8s, fuzz) and Docker Publish.
/// * `macos-latest` + `windows-latest` — Phase 16 tasks `00116` AND
///   `00122` only.
///   PyO3 wheels are platform-native (every wheel is an `.so` / `.dylib` /
///   `.pyd` compiled for the target OS), so the `cibuildwheel` matrix in
///   `.github/workflows/python-wheels.yml` MUST run on real macOS and
///   Windows runners to produce a `macosx_*` / `win_amd64` wheel. There
///   is no cross-compile path that satisfies PyO3's runtime ABI checks.
///   Cited: `audit/RFC-python-api.md` §2 "Wheel layout"; README §"Phase
///   16 cross-cutting acceptance criteria" wheel matrix.
///
///   Task `00122` widens the same allow-list to `.github/workflows/python.yml`
///   — the Python CI matrix that exercises the three test layers (00118
///   unit, 00119 integration, 00120 e2e) across 4 CPython × 3 OS = 12
///   cells. The justification mirrors the wheel-matrix reasoning: PyO3
///   ABI checks are platform-native, FTS UTF-8 tokenisation is locale-
///   sensitive on Windows, redb file locking is kernel-dependent — only
///   running on real macOS / Windows runners gives a meaningful
///   regression signal for those classes.
///
///   Adding any new runner here requires the same kind of citation —
///   a workflow that could run on `ubuntu-latest` is not a justification
///   for adding a new label.
const ALLOWED_RUNS_ON: &[&str] = &[
    "self-hosted",
    "ubuntu-latest",
    "macos-latest",
    "windows-latest",
];

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
        } else if value.starts_with("${{") && value.contains("matrix.") {
            // Matrix-expansion form, e.g. `runs-on: ${{ matrix.os }}`.
            // The literal values come from `strategy.matrix.<name>` and
            // are validated in `matrix_runner_values_are_all_allow_listed`
            // below — accepting the expression here keeps the matrix
            // pattern available without weakening the allowlist.
            true
        } else {
            ALLOWED_RUNS_ON.contains(&value)
        };

        assert!(
            ok,
            "{file}: `runs-on: {value}` is not allow-listed — supported \
             values are {ALLOWED_RUNS_ON:?} (or an array including \
             `self-hosted` plus OS/arch labels, or a `${{{{ matrix.X }}}}` \
             expression backed by an allow-listed matrix). Adding a new \
             value requires updating ALLOWED_RUNS_ON with a comment \
             explaining why.",
        );
    }
}

/// Companion to `every_runs_on_uses_an_allow_listed_runner`. When a
/// workflow uses `runs-on: ${{ matrix.os }}` (or similar), the literal
/// runner labels live under `strategy.matrix.os` — assert that EVERY
/// such literal is on `ALLOWED_RUNS_ON`. Without this guard, a future
/// PR could add `matrix.os: [ubuntu-latest, foo-runner]` and silently
/// schedule jobs on a runner class the policy never approved.
#[test]
fn matrix_runner_values_are_all_allow_listed() {
    for path in workflow_files() {
        let file = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let body = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));

        // Locate `matrix:` blocks and inspect any list-typed value
        // whose name contains "os" (the conventional name for OS axis).
        // The parser is line-based to keep this test free of a YAML
        // dependency, mirroring the rest of the suite.
        let mut in_matrix = false;
        let mut matrix_indent: Option<usize> = None;
        let mut os_axis_seen = false;
        let mut in_os_axis = false;

        for raw in body.lines() {
            let trimmed = raw.trim_start();
            let indent = raw.len() - trimmed.len();

            // Enter a matrix block when we see `matrix:` at the start
            // of a line, exit when indentation collapses back.
            if !in_matrix && trimmed.starts_with("matrix:") {
                in_matrix = true;
                matrix_indent = Some(indent);
                continue;
            }
            if in_matrix {
                if let Some(start_indent) = matrix_indent {
                    // De-indent to or past the matrix line ends the block.
                    if !trimmed.is_empty() && indent <= start_indent {
                        in_matrix = false;
                        in_os_axis = false;
                        matrix_indent = None;
                        continue;
                    }
                }
                // Axis name lines, e.g. `os:` or `os: [...]`.
                if trimmed.starts_with("os:") {
                    os_axis_seen = true;
                    let rest = trimmed.trim_start_matches("os:").trim();
                    if let Some(list) = rest.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                        for label in list.split(',').map(|s| s.trim().trim_matches('"')) {
                            assert!(
                                ALLOWED_RUNS_ON.contains(&label),
                                "{file} strategy.matrix.os contains `{label}` \
                                 which is not allow-listed — see \
                                 ALLOWED_RUNS_ON in this test"
                            );
                        }
                    } else if !rest.is_empty() {
                        // Scalar value, e.g. `os: macos-latest` — common inside a
                        // `matrix.include:` map. Check it directly.
                        let label = rest.trim_matches('"').trim_matches('\'');
                        assert!(
                            ALLOWED_RUNS_ON.contains(&label),
                            "{file} strategy.matrix.os `{label}` is not \
                             allow-listed — see ALLOWED_RUNS_ON in this test"
                        );
                    } else {
                        // Bare `os:` → list items on following lines as `- foo`.
                        in_os_axis = true;
                    }
                    continue;
                }
                if in_os_axis {
                    if let Some(item) = trimmed.strip_prefix("- ") {
                        let label = item.trim().trim_matches('"').trim_matches('\'');
                        assert!(
                            ALLOWED_RUNS_ON.contains(&label),
                            "{file} strategy.matrix.os contains `{label}` \
                             which is not allow-listed — see \
                             ALLOWED_RUNS_ON in this test"
                        );
                        continue;
                    }
                    // Anything that isn't a list item ends the os axis.
                    in_os_axis = false;
                }
            }
        }

        let _ = os_axis_seen; // not every workflow defines a matrix.os
    }
}

/// `macos-latest` and `windows-latest` are allowed ONLY for the Phase
/// 16 Python wheel matrix (`python-wheels.yml`, task `00116`) and the
/// Phase 16 Python CI matrix (`python.yml`, task `00122`). Any other
/// workflow reaching for those runners is almost certainly a slip —
/// `ubuntu-latest` covers every stable-Rust gate and `self-hosted`
/// covers the niche-target jobs. This guard catches the slip at PR
/// time so we don't silently drift back into burning GitHub minutes
/// on macOS / Windows for jobs that have no platform-native reason
/// to be there.
///
/// Both `python-wheels.yml` and `python.yml` carry the same
/// justification: PyO3 ABI checks are platform-native (no cross-
/// compile path), FTS UTF-8 tokenisation is locale-sensitive on
/// Windows, redb file locking is kernel-dependent. See the comment
/// on ALLOWED_RUNS_ON above for the full citation.
/// Workflows allowed to reach for `macos-latest` / `windows-latest`, each for a
/// platform-native reason:
/// * `python-wheels.yml` / `python.yml` — PyO3 wheels + the Python CI matrix
///   (00116 / 00122): ABI checks are platform-native, no cross-compile path.
/// * `cross-compile.yml` — the `aarch64-apple-ios` target needs the Apple SDK,
///   and the macOS platform smoke tests need a real macOS host.
const MACOS_WINDOWS_ALLOWED_WORKFLOWS: &[&str] =
    &["python-wheels.yml", "python.yml", "cross-compile.yml"];

#[test]
fn macos_and_windows_runners_only_in_justified_workflows() {
    let lines = all_runs_on_lines();
    for (file, line) in &lines {
        for restricted in ["macos-latest", "windows-latest"] {
            if line.contains(restricted) {
                assert!(
                    MACOS_WINDOWS_ALLOWED_WORKFLOWS.contains(&file.as_str()),
                    "{file} uses `{restricted}` but only \
                     {MACOS_WINDOWS_ALLOWED_WORKFLOWS:?} are allowed to — \
                     PyO3 wheels (00116) + Python CI matrix (00122) and the \
                     iOS/macOS cross-compile targets are the sole justified \
                     macOS/Windows use cases. See the comment on ALLOWED_RUNS_ON.",
                );
            }
        }
    }
}

#[test]
fn fuzz_job_in_ci_must_be_github_hosted() {
    // The `fuzz` job runs on GitHub-hosted ubuntu-latest like every other job:
    // cargo-fuzz + nightly + libFuzzer + ASAN all install/run fine on Linux
    // GitHub runners, and on a public repo the minutes are free. Keeping ANY job
    // on self-hosted is forbidden — a self-hosted runner on a public repo lets a
    // malicious fork PR execute arbitrary code on the host.
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
        !line.contains("self-hosted"),
        "ci.yml fuzz job must NOT run on self-hosted (found `{line}`) — no job \
         may, now that the repo targets public + GitHub-hosted runners.",
    );
    assert!(
        line.contains("ubuntu-latest"),
        "ci.yml fuzz job must run on ubuntu-latest (found `{line}`).",
    );
}

#[test]
fn fuzz_job_must_pin_nightly_toolchain() {
    // The fuzz smoke steps call bare `cargo fuzz run`, which reads the
    // machine-global `rustup default`. On the single self-hosted runner that
    // default is shared mutable state: a concurrent job running
    // `dtolnay/rust-toolchain@stable` (i.e. `rustup default stable`) can flip
    // it out from under the fuzz job *between two smoke steps*, so cargo-fuzz's
    // `-Zsanitizer=address` build dies with "the option `Z` is only accepted on
    // the nightly compiler". Pinning `RUSTUP_TOOLCHAIN: nightly` at job level
    // makes every step of the job immune to that race regardless of the global
    // default. Regression guard for the flaky fuzz failure diagnosed on PR #244
    // (nightly-OK smoke step immediately followed by a stable-fail one).
    let ci_yml = workflows_dir().join("ci.yml");
    let body = fs::read_to_string(&ci_yml).expect("ci.yml exists");
    let mut in_fuzz_job = false;
    let mut pinned = false;
    for line in body.lines() {
        let trimmed = line.trim_start();
        // Job declarations are indented 2 spaces and end with `:`.
        if line.starts_with("  ") && !line.starts_with("    ") && trimmed.ends_with(':') {
            let job = trimmed.trim_end_matches(':');
            in_fuzz_job = job == "fuzz";
            continue;
        }
        if in_fuzz_job && trimmed.starts_with("RUSTUP_TOOLCHAIN:") && trimmed.contains("nightly") {
            pinned = true;
            break;
        }
    }
    assert!(
        pinned,
        "ci.yml fuzz job must pin `RUSTUP_TOOLCHAIN: nightly` at job level. \
         Without it the bare `cargo fuzz run` steps read the shared global \
         `rustup default`, which a concurrent stable-toolchain job on the \
         single self-hosted runner can flip mid-job, breaking the \
         nightly-only `-Zsanitizer=address` build.",
    );
}

#[test]
fn docker_publish_job_must_be_github_hosted() {
    // Docker Publish MUST stay OFF the self-hosted runner. It is a tag-only
    // release artifact nobody waits on; on the single self-hosted runner its
    // multi-arch build (~40-60 min cold) monopolised the runner and stalled
    // every PR's CI behind each release tag. On ubuntu-latest it runs on an
    // ephemeral GitHub runner in parallel, so it never blocks CI/fuzz. The
    // per-build cost (arm64 under QEMU) is bounded by the `type=gha` layer
    // cache and releases are infrequent. This invariant prevents a regression
    // back to `self-hosted`.
    let docker_yml = workflows_dir().join("docker-publish.yml");
    if !docker_yml.exists() {
        // If the file is renamed, the rename should ship with this test
        // updated; until then the absence is a soft signal.
        return;
    }
    let body = fs::read_to_string(&docker_yml).expect("docker-publish.yml exists");
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("runs-on:") {
            assert!(
                !trimmed.contains("self-hosted"),
                "docker-publish.yml must NOT run on `self-hosted` — the multi-arch \
                 release build monopolises the single self-hosted runner and stalls \
                 every PR's CI behind each release tag. Keep it on `ubuntu-latest`.",
            );
            assert!(
                trimmed.contains("ubuntu-latest"),
                "docker-publish.yml `runs-on:` must be `ubuntu-latest` so the release \
                 build runs on an ephemeral GitHub runner, never blocking CI. Got: {trimmed}",
            );
        }
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

/// Repository root (the drevo crate manifest sits at the workspace root).
fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Parse the integer assigned to `key` on a `key = N` / `key: "N"` line,
/// tolerating optional quotes and surrounding whitespace. Returns the first
/// match found in `body`.
fn parse_capped_int(body: &str, key: &str) -> Option<u32> {
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(key) {
            let rest = rest.trim_start();
            let rest = rest.strip_prefix([':', '=']).unwrap_or(rest);
            let value = rest.trim().trim_matches('"').trim_matches('\'').trim();
            if let Ok(n) = value.parse::<u32>() {
                return Some(n);
            }
        }
    }
    None
}

#[test]
fn nextest_config_caps_test_threads_below_runner_core_count() {
    // The self-hosted runner is also a daily-driver Mac (10 logical cores).
    // Running the full nextest suite at the default `num-cpus` parallelism
    // alongside interactive use has frozen the whole machine. `.config/
    // nextest.toml` must cap `test-threads` to a bounded value that leaves
    // headroom for the OS / UI. This invariant locks the cap in so a future
    // edit can't silently restore unbounded parallelism. See the file's
    // header comment for the full rationale.
    let path = repo_root().join(".config").join("nextest.toml");
    let body = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{} must exist and cap test-threads (CI-runner resource guard): {e}",
            path.display()
        )
    });
    let threads = parse_capped_int(&body, "test-threads").unwrap_or_else(|| {
        panic!(
            "{} must set a numeric `test-threads = N` under [profile.default] \
             to bound test-process parallelism on the self-hosted runner",
            path.display()
        )
    });
    // 10 logical cores on the runner; a cap must leave real headroom and be
    // a sane positive value.
    assert!(
        (1..=8).contains(&threads),
        "nextest `test-threads` must be a bounded cap (1..=8) that leaves \
         CPU/RAM headroom on the 10-core self-hosted runner; found {threads}"
    );
}

#[test]
fn ci_yml_caps_cargo_build_jobs_below_runner_core_count() {
    // Compile-time parallelism is the other half of the freeze: a full
    // `cargo build` at `jobs = num-cpus` spawns up to 10 parallel rustc
    // processes on the 10-core runner. ci.yml must cap `CARGO_BUILD_JOBS`
    // so a build leaves headroom for the desktop. Locked here so the cap
    // can't silently regress.
    let ci_yml = workflows_dir().join("ci.yml");
    let body = fs::read_to_string(&ci_yml).expect("ci.yml exists");
    assert!(
        body.contains("CARGO_BUILD_JOBS"),
        "ci.yml must set `CARGO_BUILD_JOBS` to cap parallel compilation on \
         the self-hosted runner — without it a full `cargo build` saturates \
         every core and can wedge the daily-driver machine into a freeze."
    );
    let jobs = parse_capped_int(&body, "CARGO_BUILD_JOBS")
        .unwrap_or_else(|| panic!("ci.yml `CARGO_BUILD_JOBS` must be set to a numeric value"));
    assert!(
        (1..=8).contains(&jobs),
        "ci.yml `CARGO_BUILD_JOBS` must be a bounded cap (1..=8) that leaves \
         headroom on the 10-core self-hosted runner; found {jobs}"
    );
}

/// The docs-only path set — these glob patterns MUST appear in
/// `paths-ignore:` of every NON-required heavy workflow that runs on
/// branch pushes (cross-compile.yml) so docs-only PRs don't consume
/// runner time or hit upstream flakes there.
///
/// ci.yml is NOT in that list: as the required-checks workflow it must
/// always run (so its `CI / <Job>` checks always report), and it filters
/// docs-only changes PER JOB via the `changes` detector + `if:` gate
/// (skip == success for required checks) rather than per-workflow. See
/// `ci_yml_uses_per_job_docs_gate_not_paths_ignore`.
const DOCS_ONLY_GLOBS: &[&str] = &["**/*.md", "audit/**", "memory/**", "LICENSE", ".gitignore"];

/// Non-required heavy workflows that MUST skip docs-only PRs via
/// `paths-ignore`. A docs-only PR (README, audit/, memory/, …) touches
/// no Rust code / infra, so running these wastes runner time AND risks a
/// PR-blocking upstream flake (e.g. Android NDK download from
/// dl.google.com — PR #81, a README-only PR, was blocked by exactly
/// that on cross-compile.yml before this invariant landed). ci.yml is
/// intentionally absent (it always runs; it gates docs-only per job).
/// docker-publish.yml was removed 2026-07-30: it no longer runs on
/// branch pushes (release-tag + workflow_dispatch only), so it has no
/// `paths-ignore` to keep in sync.
const HEAVY_WORKFLOWS_WITH_DOCS_SKIP: &[&str] = &["cross-compile.yml"];

#[test]
fn heavy_workflows_share_docs_only_paths_ignore() {
    for workflow in HEAVY_WORKFLOWS_WITH_DOCS_SKIP {
        let path = workflows_dir().join(workflow);
        let body =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("{workflow} must exist: {e}"));
        assert!(
            body.contains("paths-ignore:"),
            "{workflow} must declare `paths-ignore:` on its PR / push \
             triggers so docs-only PRs (README, audit/, memory/) do \
             not consume CI minutes or expose the PR to upstream \
             network flakes in unrelated build steps. The path set \
             MUST be byte-identical across all workflows in \
             HEAVY_WORKFLOWS_WITH_DOCS_SKIP."
        );
        for glob in DOCS_ONLY_GLOBS {
            assert!(
                body.contains(glob),
                "{workflow} `paths-ignore:` is missing the docs-only \
                 glob `{glob}` — the set must be {DOCS_ONLY_GLOBS:?} \
                 and stay in sync across {HEAVY_WORKFLOWS_WITH_DOCS_SKIP:?}."
            );
        }
    }
}

#[test]
fn ci_yml_uses_per_job_docs_gate_not_paths_ignore() {
    // ci.yml is the required-checks workflow, so it must ALWAYS trigger:
    // a workflow filtered out by `paths-ignore` never reports its
    // required checks, leaving branch protection waiting forever (the bug
    // the deleted ci-skip.yml existed to paper over). Docs-only changes
    // are filtered PER JOB via the `changes` detector + `if:` gate, where
    // a skipped required job counts as success.
    let body = fs::read_to_string(workflows_dir().join("ci.yml")).expect("ci.yml exists");
    assert!(
        !body.contains("paths-ignore:"),
        "ci.yml must NOT use `paths-ignore:` — it must always run and gate \
         docs-only changes per job via the `changes` job. A workflow-level \
         path filter leaves the required `CI / <Job>` checks unreported on \
         docs-only PRs (the bug ci-skip.yml worked around; it is now gone)."
    );
}

#[test]
fn ci_yml_has_changes_detector_job() {
    // The per-job docs gate depends on a `changes` job exposing a `code`
    // output (`'false'` only for a provably docs-only change).
    let body = fs::read_to_string(workflows_dir().join("ci.yml")).expect("ci.yml exists");
    assert!(
        body.contains("\n  changes:\n"),
        "ci.yml must declare a `changes:` job that detects docs-only \
         changes for the per-job gate."
    );
    assert!(
        body.contains("code: ${{ steps.detect.outputs.code }}"),
        "ci.yml's `changes` job must expose `outputs.code` from its detect \
         step — every gated job's `if:` reads `needs.changes.outputs.code`."
    );
}

#[test]
fn ci_yml_required_jobs_are_gated_on_changes() {
    // Every required build job must (a) `needs: changes` and (b) gate on
    // `needs.changes.outputs.code != 'false'` — the fail-safe form: it
    // runs unless the change is provably docs-only, so a detector failure
    // or any non-docs file runs the real job (never a silent skip).
    let body = fs::read_to_string(workflows_dir().join("ci.yml")).expect("ci.yml exists");
    let all_jobs = [
        "changes", "check", "test", "clippy", "fmt", "k8s", "doc", "msrv", "fuzz",
    ];
    let mut offsets: Vec<usize> = all_jobs
        .iter()
        .filter_map(|j| body.find(&format!("\n  {j}:\n")))
        .collect();
    offsets.sort_unstable();
    // fuzz is continue-on-error (not a required check) but is gated too.
    let required = [
        "check", "test", "clippy", "fmt", "k8s", "doc", "msrv", "fuzz",
    ];
    for id in required {
        let hdr = format!("\n  {id}:\n");
        let start = body
            .find(&hdr)
            .unwrap_or_else(|| panic!("ci.yml must declare a `{id}:` job"));
        let next = offsets
            .iter()
            .copied()
            .find(|&o| o > start)
            .unwrap_or(body.len());
        let block = &body[start..next];
        assert!(
            block.contains("needs: changes"),
            "ci.yml job `{id}` must declare `needs: changes` to gate on the \
             docs-only detector."
        );
        assert!(
            block.contains("needs.changes.outputs.code != 'false'"),
            "ci.yml job `{id}` must gate with `needs.changes.outputs.code != \
             'false'` (fail-safe: runs unless provably docs-only)."
        );
    }
}

#[test]
fn ci_skip_yml_must_not_exist() {
    // The dual-workflow ci-skip pattern was removed: two workflows both
    // named `CI` emitting the same `CI / <Job>` contexts raced on the
    // single self-hosted runner (both a false-green and a false-block were
    // observed). ci.yml now owns every context via per-job gating; a
    // resurrected ci-skip.yml would re-create the race.
    assert!(
        !workflows_dir().join("ci-skip.yml").exists(),
        "`.github/workflows/ci-skip.yml` must NOT exist — docs-only \
         filtering is now per-job in ci.yml. A second `name: CI` workflow \
         re-creates the status-check race."
    );
}

#[test]
fn ci_yml_test_job_does_not_run_benches() {
    // PR-gating CI is for correctness, NOT performance measurement.
    // The drevo benchmark crates declare `harness = false` in
    // `Cargo.toml`, which means their criterion-based `main()` runs
    // when included in a test invocation — and on the heavy ones
    // (`bulk_put_100k/RedbBackend` ≈ 530s, `traversal_bfs/...` ≈ 240s
    // each) a single iteration is enough to push a PR run past 60
    // minutes. PR #76 was observed to do exactly that: 1695 real
    // tests finished in ~5 minutes, the next ~50 entries were all
    // benches and consumed another ~90 minutes. Removing
    // `--all-targets` from the nextest invocation cuts the test job
    // back down to ~5 minutes.
    //
    // This invariant locks the fix in: the `test` job's nextest
    // invocation must NOT pass `--all-targets`, `--benches`, or any
    // other flag that would re-enable bench execution.
    let ci_yml = workflows_dir().join("ci.yml");
    let body = fs::read_to_string(&ci_yml).expect("ci.yml exists");

    // Scope the search to the test job: walk from `^  test:` to the
    // next top-level job header, then check every step's `run:` line.
    let mut in_test_job = false;
    let mut test_job_indent: usize = 0;
    let mut bench_offenders: Vec<String> = Vec::new();
    for line in body.lines() {
        let indent = line.chars().take_while(|c| *c == ' ').count();
        let trimmed = line.trim_start();
        // Top-level job declarations sit at column 2.
        if line.starts_with("  ") && !line.starts_with("    ") && trimmed.ends_with(':') {
            let job = trimmed.trim_end_matches(':');
            if in_test_job && job != "test" {
                break;
            }
            in_test_job = job == "test";
            if in_test_job {
                test_job_indent = indent;
            }
            continue;
        }
        if !in_test_job {
            continue;
        }
        // Defensive: stop if we somehow leave the `test:` block.
        if indent <= test_job_indent && !line.is_empty() && !trimmed.starts_with('-') {
            break;
        }
        if trimmed.starts_with("run:") || trimmed.starts_with("- run:") {
            // Only flag the nextest invocation; `cargo test --doc` and
            // any future step that genuinely needs `--all-targets`
            // (none planned) would be addressed by extending this test.
            if trimmed.contains("nextest") {
                for bad_flag in ["--all-targets", "--benches", "--bench "] {
                    if trimmed.contains(bad_flag) {
                        bench_offenders.push(format!("{bad_flag}: {trimmed}"));
                    }
                }
            }
        }
    }

    assert!(
        bench_offenders.is_empty(),
        "ci.yml `test` job's nextest invocation passes one or more \
         bench-enabling flags. PR-gating CI MUST NOT execute \
         criterion benches — they push the job past 60 minutes. \
         Offenders:\n  {}\n\n\
         Default `cargo nextest run` includes only library unit + \
         integration tests; that is the right scope for PR validation. \
         Benches belong in a separate scheduled workflow.",
        bench_offenders.join("\n  "),
    );
}

// ---- bench.yml invariants ------------------------------------------------
//
// `bench.yml` runs criterion benches nightly on the self-hosted runner.
// It MUST NOT be triggered on PR or push, otherwise we re-introduce the
// four-hour CI debacle the bench-removal fix (the `ci_yml_test_job_does_not_run_benches`
// test above pins it) just resolved.
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
fn bench_yml_runs_on_github_hosted() {
    // bench.yml runs on GitHub-hosted ubuntu-latest like every other workflow
    // (no self-hosted runner exists anymore). Absolute benchmark numbers from
    // ephemeral VMs carry more noise, but the workflow is scheduled/opt-in and
    // criterion reports its own variance — a stable host is not worth keeping a
    // self-hosted runner (a security liability on a public repo).
    let bench_yml = workflows_dir().join("bench.yml");
    let body = fs::read_to_string(&bench_yml).expect("bench.yml exists");
    let bench_runs_on = body
        .lines()
        .find(|line| line.trim_start().starts_with("runs-on:"))
        .map(str::to_string)
        .expect("bench.yml must declare `runs-on:`");
    assert!(
        !bench_runs_on.contains("self-hosted"),
        "bench.yml `runs-on:` must NOT be self-hosted (found `{bench_runs_on}`)."
    );
}

#[test]
fn bench_yml_caps_runtime_so_it_cannot_hog_the_runner() {
    // A full criterion run took ~150-175 min every night (one run hit
    // 359 min, one minute from GitHub's 360-min default kill), squatting
    // the single self-hosted runner from 04:00 into working hours. Two
    // guards keep that bounded and must both stay: an explicit
    // `timeout-minutes` (< 360, so a hang frees the runner) and a reduced
    // criterion `--sample-size` (so a normal run finishes well under the
    // cap). Removing either reintroduces the multi-hour hog.
    let body = fs::read_to_string(workflows_dir().join("bench.yml")).expect("bench.yml exists");
    let timeout = body
        .lines()
        .find(|l| l.trim_start().starts_with("timeout-minutes:"))
        .unwrap_or_else(|| {
            panic!(
                "bench.yml must declare `timeout-minutes:` — without it the job \
                 inherits GitHub's 360-min default and can squat the single \
                 self-hosted runner for hours (it took ~3h nightly before this cap)."
            )
        });
    let value: u32 = timeout
        .split(':')
        .nth(1)
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| {
            panic!("bench.yml has an unparseable timeout-minutes line: {timeout:?}")
        });
    assert!(
        value < 360,
        "bench.yml timeout-minutes={value} must be < 360 (the default 6h ceiling)"
    );
    assert!(
        body.contains("--sample-size"),
        "bench.yml must pass criterion `--sample-size` to keep the nightly run \
         under the timeout — the default (100) samples took ~150-175 min."
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

/// Regression test for the workflow-parse failure that silently broke
/// every PR's CI for ~24 hours after commit `0872954` landed.
///
/// **Background.** GHA's `runner.*` expression context is only resolved
/// *at step execution time* — after a runner has been assigned to a
/// job. Putting `${{ runner.tool_cache }}` (or any other `runner.*`
/// expression) in a workflow-level `env:` block OR in a job-level
/// `env:` block produces a generic "workflow file issue" error: the
/// run appears in the Actions UI under its **file path** instead of
/// its `name:` field, with conclusion `failure`, 0 jobs, no logs.
/// Same failure category as the `runner.os` in a job-level `if:`
/// (already guarded by [`workflow_files_do_not_smuggle_runner_os_at_job_level`]).
///
/// **History.** A first revision of the CARGO_TARGET_DIR caching change
/// put the expression in the *workflow-level* `env:` block, caught and
/// fixed in commit `0872954`. That fix moved the expression to *job-
/// level* `env:` — but GHA rejects it there too, which only became
/// visible after PR #89 merged because no PR landed for the 24h
/// window where the workflow-parse failure swallowed the `pull_request`
/// trigger.  This invariant locks the working pattern: a setup step
/// that exports `CARGO_TARGET_DIR=$RUNNER_TOOL_CACHE/…` into
/// `$GITHUB_ENV` *inside* `steps:` (which IS step-level).
///
/// **What this test checks.** For every workflow file, parse a tiny
/// indentation-based state machine over the YAML to track whether the
/// current line sits inside `steps:`. Forbid any `${{ runner.* }}`
/// expression OUTSIDE `steps:` — that's the failure mode.  Comments
/// are exempt (they're allowed to reference the expression in prose so
/// future contributors know what NOT to do).
#[test]
fn ci_workflows_dont_use_runner_context_outside_steps() {
    let mut violations: Vec<String> = Vec::new();
    for path in workflow_files() {
        let file_label = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let body = fs::read_to_string(&path).unwrap_or_default();
        let mut steps_indent: Option<usize> = None;
        for (lineno, line) in body.lines().enumerate() {
            let trimmed = line.trim_start();
            let indent = line.len() - trimmed.len();
            // Track entry / exit from a `steps:` block.
            if trimmed.starts_with("steps:") {
                steps_indent = Some(indent);
                continue;
            }
            if let Some(si) = steps_indent {
                // Anything at <= the `steps:` indent that's a YAML key
                // means we left the steps block (next job or top-level).
                if !line.trim().is_empty()
                    && indent <= si
                    && !trimmed.starts_with('#')
                    && !trimmed.starts_with('-')
                    && trimmed.contains(':')
                {
                    steps_indent = None;
                }
            }
            // Comments referencing the expression in prose are fine —
            // they're how we educate future contributors about WHY this
            // doesn't work.
            if trimmed.starts_with('#') {
                continue;
            }
            // Inside steps: the expression is valid. Outside it: forbidden.
            if line.contains("${{ runner.") && steps_indent.is_none() {
                violations.push(format!(
                    "{}:{}: `${{{{ runner.* }}}}` used outside of `steps:` — \
                     GHA's runner context is only resolved at step \
                     execution time. Move into a setup step like \
                     `run: echo \"VAR=$RUNNER_TOOL_CACHE/…\" >> $GITHUB_ENV`. \
                     Line: `{}`",
                    file_label,
                    lineno + 1,
                    line.trim_end(),
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "GitHub Actions workflow files use `${{{{ runner.* }}}}` outside \
         of a `steps:` block, which makes GHA reject the workflow YAML \
         (run appears under file path with no jobs, no logs). See the \
         test docstring for the history and the fix pattern. \
         Violations:\n  - {}",
        violations.join("\n  - "),
    );
}

// ============================================================================
// Android NDK download — curl resilience against dl.google.com HTTP/2 flakes
// ============================================================================
//
// The Android job in cross-compile.yml downloads the NDK from
// `dl.google.com/android/repository/android-ndk-rXX-<host>.zip` with a
// single `curl -fsSL` call on a cold cache. dl.google.com terminates
// HTTP/2 streams mid-transfer often enough that we observed it on main
// (bce8ade, 2026-05-26):
//
//   curl: (92) HTTP/2 stream 1 was not closed cleanly: INTERNAL_ERROR (err 2)
//
// One-shot curl with no retry surfaces every upstream flake as a red CI
// build, and the Android job is the only thing in the workflow that
// touches the public internet for a ~1 GiB artifact. The fix is to make
// curl retry transient failures (including mid-stream HTTP/2 resets) and
// to force HTTP/1.1, which sidesteps the HTTP/2 termination class entirely
// — dl.google.com still serves the same archive over HTTP/1.1 with no
// behavioural difference besides the absence of stream multiplexing.
//
// These flags are NON-NEGOTIABLE on every curl-to-dl.google.com call in
// the workflow: dropping any one of them re-opens the failure mode.

const ANDROID_NDK_CURL_FLAGS: &[(&str, &str)] = &[
    (
        "--retry",
        "without `--retry N` (N>=3) the single transient HTTP/2 reset from \
         dl.google.com fails the whole CI run — exactly the failure mode \
         observed on commit bce8ade",
    ),
    (
        "--retry-all-errors",
        "without `--retry-all-errors` curl only retries on a small subset of \
         exit codes; HTTP/2 stream errors (exit 92) and broken pipes (exit \
         18) are NOT in that subset and slip past plain `--retry`",
    ),
    (
        "--retry-delay",
        "without `--retry-delay` curl backs off zero seconds between attempts \
         and hammers dl.google.com with the same in-flight congestion that \
         caused the first reset, so all N retries fail in a fraction of a \
         second",
    ),
    (
        "--connect-timeout",
        "without `--connect-timeout` a hung TCP connect can stall the job \
         for the workflow's full 6-hour budget before timing out — \
         dl.google.com edge nodes occasionally accept-then-stall on warm \
         self-hosted IPs",
    ),
    (
        "--http1.1",
        "without `--http1.1` curl negotiates HTTP/2 by default and inherits \
         dl.google.com's stream-reset behaviour; forcing HTTP/1.1 sidesteps \
         the failure class entirely with no functional downside for a \
         single-file download",
    ),
];

#[test]
fn cross_compile_android_ndk_download_uses_resilient_curl() {
    let cross = fs::read_to_string(workflows_dir().join("cross-compile.yml"))
        .expect("cross-compile.yml must exist");

    // The curl call we care about is the one to dl.google.com inside the
    // `Install Android NDK` step. Locate its surrounding block first so
    // we assert on the right call (rather than picking up any curl
    // anywhere else in the workflow).
    let ndk_block = extract_ndk_install_block(&cross);
    // The curl call can span multiple physical lines via `\` continuation
    // (the readable form is `curl -fsSL \\\n  --retry 5 …`). Walk lines
    // and accumulate continuation lines onto whichever line starts with
    // `curl `, so the flag assertions below see the full invocation as a
    // single string.
    let curl_text = join_continued_curl_lines(&ndk_block);
    assert!(
        !curl_text.is_empty(),
        "the `Install Android NDK` step in cross-compile.yml must invoke \
         `curl` to fetch the NDK archive from dl.google.com — block was:\n{}",
        ndk_block,
    );
    for (flag, why) in ANDROID_NDK_CURL_FLAGS {
        assert!(
            curl_text.contains(flag),
            "the NDK-download `curl` in cross-compile.yml is missing `{flag}` \
             — {why}. Current curl invocation(s):\n{curl_text}"
        );
    }
}

/// Collapse `\`-continued shell command lines that start with `curl `
/// into a single logical line each, then return all such commands
/// joined by newlines. Lines that are pure shell-comment (`#`) inside
/// a continuation chain are dropped — `curl` does not honour comments
/// mid-arguments. This lets the flag assertions above match across
/// physical line boundaries without forcing the workflow to keep curl
/// on a single physical line.
fn join_continued_curl_lines(block: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in block.lines() {
        let trimmed = line.trim_start();
        let is_curl_start = trimmed.starts_with("curl ") || trimmed.contains(" curl ");
        if is_curl_start {
            if let Some(prev) = current.take() {
                out.push(prev);
            }
            current = Some(line.trim_end_matches('\\').trim().to_string());
            // If the curl line itself does NOT end with `\`, it's a
            // single-line invocation — flush it immediately.
            if !line.trim_end().ends_with('\\') {
                if let Some(done) = current.take() {
                    out.push(done);
                }
            }
            continue;
        }
        if let Some(ref mut buf) = current {
            // Inside a continuation chain. Skip pure comment lines —
            // they aren't part of the curl arguments.
            if trimmed.starts_with('#') {
                continue;
            }
            buf.push(' ');
            buf.push_str(line.trim_end_matches('\\').trim());
            if !line.trim_end().ends_with('\\') {
                if let Some(done) = current.take() {
                    out.push(done);
                }
            }
        }
    }
    if let Some(done) = current.take() {
        out.push(done);
    }
    out.join("\n")
}

/// Extract the body of the `Install Android NDK (cached on self-hosted
/// runner)` step from cross-compile.yml so the curl-flag assertions
/// above are scoped to the actual NDK-download script — not any other
/// curl that might appear elsewhere in the workflow.
fn extract_ndk_install_block(workflow: &str) -> String {
    let marker = "Install Android NDK";
    let start = workflow.find(marker).unwrap_or_else(|| {
        panic!(
            "cross-compile.yml must contain a step named \"Install \
             Android NDK\" — the curl-flag invariants below need to \
             find the right block. Workflow body:\n{workflow}"
        )
    });
    // Walk forward to the next step's `- name:` (or end-of-file). Steps
    // in this workflow are indented 6 spaces under `steps:`.
    let after_start = &workflow[start..];
    let rel_next = after_start
        .match_indices("\n      - name:")
        .nth(1)
        .map(|(idx, _)| idx)
        .unwrap_or(after_start.len());
    after_start[..rel_next].to_string()
}

// ============================================================================
// Docker login — avoid macOS Keychain when running on a self-hosted Mac
// ============================================================================
//
// `docker/login-action` shells out to `docker login`, which on macOS
// writes credentials through the `osxkeychain` credsStore by default
// (the system Docker config under `~/.docker/config.json` has
// `"credsStore": "desktop"` / `"osxkeychain"`). Writing to the keychain
// requires an unlocked GUI session and surfaces as:
//
//   Error saving credentials: error storing credentials - err: exit
//   status 1, out: `User interaction is not allowed. (-25308)`
//
// when the runner is invoked headlessly (the project's self-hosted Mac
// runner is). The workflow has been red on every push to main since
// before #90 because of this exact error.
//
// A `{}` / `{"credsStore":""}` config does NOT help: Docker Desktop
// re-injects `credsStore: osxkeychain` on `docker login`. Fix: skip
// `docker login` entirely and pre-bake the base64 `auths` blob into a
// per-run `DOCKER_CONFIG/config.json`. Reading auths from the file uses
// no credential helper, so the Keychain is never touched; the temp
// config is discarded with the workspace.

#[test]
fn docker_publish_overrides_docker_config_to_avoid_macos_keychain() {
    let w = fs::read_to_string(workflows_dir().join("docker-publish.yml"))
        .expect("docker-publish.yml must exist");

    // (a) DOCKER_CONFIG must be exported to a per-run dir so docker / buildx
    // use it instead of the user's ~/.docker/config.json (credsStore=osxkeychain).
    assert!(
        w.contains("DOCKER_CONFIG="),
        "docker-publish.yml must export `DOCKER_CONFIG=<per-run dir>` so docker \
         does not fall back to the user's ~/.docker/config.json (which has \
         `credsStore: osxkeychain` on the self-hosted Mac) and fail -25308."
    );

    // (b) Auth must be PRE-BAKED as an `auths` blob, never via `docker login` /
    // `docker/login-action`: Docker Desktop re-injects `credsStore: osxkeychain`
    // on login even into a `{}` config, so login fails headless with
    // `User interaction is not allowed. (-25308)`. Writing auths to the file
    // invokes no credential helper.
    assert!(
        !w.contains("uses: docker/login-action"),
        "docker-publish.yml must NOT use `docker/login-action` — it triggers the \
         macOS osxkeychain credsStore and fails -25308 on the headless runner. \
         Pre-bake an `auths` blob into DOCKER_CONFIG instead."
    );
    assert!(
        w.contains("\"auths\""),
        "docker-publish.yml must pre-bake an `auths` blob into \
         DOCKER_CONFIG/config.json for GHCR auth (no `docker login`)."
    );
}

// ---- task 00129: slow-test re-enablement contract ------------------------
//
// Task 00129 deleted the temporary `ci-fast` nextest profile so the PR
// `test` job runs every integration-test binary again (proptest fixtures,
// `*_bench_tests`, `wal_crash_recovery_tests`, the light compaction cells).
// The ONLY exception is the two heaviest redb-churn compaction cells, kept
// `#[ignore]`d so they never sit in the serialised self-hosted-runner queue.
//
// The README deliberately did NOT lock the *disable* (so it stayed easy to
// remove). These two tests instead lock the *path-(c) safety net* that
// replaced it: the heavy cells must be (a) marked `#[ignore]` so they are off
// the PR path AND (b) have a nightly home in `slow-tests.yml`. Together they
// prevent the failure mode 00129 was written to avoid — a gated test rotting
// into a permanent skip that no CI job ever runs.

/// The two heaviest redb-churn compaction cells must carry `#[ignore]` so
/// they stay off the PR-gating `test` job.
const IGNORED_COMPACTION_CELLS: [&str; 2] = [
    "redb_backend_size_bytes_returns_file_size",
    "redb_compact_reclaims_after_heavy_churn",
];

#[test]
fn heavy_compaction_cells_are_ignored_for_pr_path() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("compaction_tests.rs");
    let body = fs::read_to_string(&path).expect("tests/compaction_tests.rs exists");
    let lines: Vec<&str> = body.lines().collect();

    for cell in IGNORED_COMPACTION_CELLS {
        let needle = format!("fn {cell}(");
        let fn_idx = lines
            .iter()
            .position(|l| l.trim_start().starts_with(&needle))
            .unwrap_or_else(|| {
                panic!("compaction_tests.rs must still define `fn {cell}` (task 00129 cell)")
            });
        // Walk back over the attribute/comment block immediately above the
        // `fn` and require an `#[ignore` somewhere in it (before the previous
        // item ends — i.e. before a blank line that is not itself part of the
        // attribute block). Scanning the 12 lines above the fn is plenty.
        let start = fn_idx.saturating_sub(12);
        let has_ignore = lines[start..fn_idx]
            .iter()
            .any(|l| l.trim_start().starts_with("#[ignore"));
        assert!(
            has_ignore,
            "compaction_tests.rs::{cell} MUST be `#[ignore]`d (task 00129): it \
             is one of the two heaviest redb-churn cells and is kept off the \
             PR `test` job. If you intend to re-enable it on PRs, also update \
             `slow_tests_yml_runs_the_ignored_compaction_cells` and the 00129 \
             README entry."
        );
    }
}

#[test]
fn slow_tests_yml_runs_the_ignored_compaction_cells() {
    // The ignored compaction cells need a home that actually runs them,
    // otherwise they rot into a permanent skip. `slow-tests.yml` must carry a
    // step that runs the ignored tests of the `compaction_tests` binary via
    // `--run-ignored ignored-only` (NOT `--run-ignored all`, which would drag
    // in the 30-minute agentic-workload soaks and the Docker/Neo4j-live
    // ignored tests).
    let slow_yml = workflows_dir().join("slow-tests.yml");
    let body = fs::read_to_string(&slow_yml).expect("slow-tests.yml exists");

    let has_ignored_run = body.lines().any(|line| {
        let t = line.trim_start();
        t.starts_with("run:")
            && t.contains("nextest")
            && t.contains("--run-ignored ignored-only")
            && t.contains("binary(compaction_tests)")
    });
    assert!(
        has_ignored_run,
        "slow-tests.yml MUST run the `#[ignore]`d compaction cells via a step \
         like `cargo nextest run --run-ignored ignored-only ... -E \
         'binary(compaction_tests)'`. Without it, the cells \
         `heavy_compaction_cells_are_ignored_for_pr_path` keeps off the PR \
         path would never run in any CI job (task 00129 path c)."
    );
}

#[test]
fn every_ci_job_declares_a_timeout() {
    // Incident 2026-06-10 (PR #145 duplicate run): the `test` job hung at
    // "Compiling drevo" — blocked on the cargo build-dir lock held by a
    // concurrent job sharing the same `$CARGO_TARGET_DIR` — and, with no
    // explicit `timeout-minutes`, squatted the single self-hosted runner
    // for GitHub's full 360-minute default before being killed, starving
    // every other PR. Every job in ci.yml must declare an explicit
    // `timeout-minutes` strictly below 360 so a lock-blocked or hung job
    // fails fast and frees the runner.
    let ci_yml = workflows_dir().join("ci.yml");
    let body = fs::read_to_string(&ci_yml).expect("ci.yml exists");
    let lines: Vec<&str> = body.lines().collect();

    // Find the `jobs:` section, then each job is a key indented exactly
    // two spaces (`  check:`). A job block runs until the next two-space
    // key or EOF; it must contain a `timeout-minutes:` line.
    let jobs_start = lines
        .iter()
        .position(|l| l.trim_end() == "jobs:")
        .expect("ci.yml must have a top-level `jobs:` section");

    // Collect (job_name, start_line_index) for every two-space-indented key.
    let mut jobs: Vec<(String, usize)> = Vec::new();
    for (idx, line) in lines.iter().enumerate().skip(jobs_start + 1) {
        let is_two_space_key = line.starts_with("  ")
            && !line.starts_with("   ")
            && line.trim_end().ends_with(':')
            && !line.trim_start().starts_with('#');
        if is_two_space_key {
            jobs.push((line.trim().trim_end_matches(':').to_string(), idx));
        }
    }
    assert!(
        jobs.len() >= 5,
        "expected to parse several jobs from ci.yml, found {} — parser likely \
         broke against a workflow reformat",
        jobs.len()
    );

    for (i, (name, start)) in jobs.iter().enumerate() {
        let end = jobs.get(i + 1).map(|(_, s)| *s).unwrap_or(lines.len());
        let block = &lines[*start..end];
        let timeout_line = block
            .iter()
            .find(|l| l.trim_start().starts_with("timeout-minutes:"));
        let timeout_line = timeout_line.unwrap_or_else(|| {
            panic!(
                "ci.yml job `{name}` MUST declare `timeout-minutes:` — without \
                 it the job inherits GitHub's 360-minute default and can squat \
                 the single self-hosted runner for 6h on a build-lock hang (see \
                 the PR #145 incident in this test's comment)."
            )
        });
        let value: u32 = timeout_line
            .split(':')
            .nth(1)
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_else(|| panic!("ci.yml job `{name}` has an unparseable timeout-minutes"));
        assert!(
            value < 360,
            "ci.yml job `{name}` timeout-minutes={value} must be < 360 (the \
             default) — the whole point is to cap below the 6h ceiling"
        );
    }
}
