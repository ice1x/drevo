//! Phase 16 task `00122` — text-level scaffolding tests for the
//! cibuildwheel-driven Python CI matrix.
//!
//! Same pattern as `python_package_wheels_tests.rs` (`00116`) and
//! `python_e2e_test_suite_tests.rs` (`00120`): the existing Rust CI
//! runners do not provision a Python interpreter, so these tests do
//! NOT invoke `pytest`, `pip`, `maturin`, or `cibuildwheel`. They
//! lock the *shape* of `.github/workflows/python.yml` by grepping it
//! on disk.
//!
//! The workflow itself is the definition-of-done gate for Phase 16:
//! a 2-cell cibuildwheel matrix (`platform: [macos, linux]`) that
//! runs the three drevo-py test layers (00118 unit, 00119 integration,
//! 00120 e2e) plus `mypy --strict`, `ruff check`, and `black --check`
//! inside cibuildwheel's per-platform sandbox.
//!
//! **Why cibuildwheel and not GHA `container:`?** An earlier iteration
//! tried `container: python:3.13-bookworm` to route the ubuntu cell
//! to the self-hosted macOS runner. The runner-agent refused with
//! "Container operations are only supported on Linux runners" — GHA's
//! `container:` directive is implemented only in the Linux runner
//! agent. cibuildwheel sidesteps that by managing `docker run` itself,
//! the same pattern `python-wheels.yml` already uses.
//!
//! **Why `[macos, linux]` and not the full `[ubuntu, macos, windows]`
//! roadmap matrix?** The 4 × 3 = 12-cell roadmap matrix doesn't pay
//! its way under abi3-py310 (same binary covers every CPython 3.10+).
//! `cp313-*` only is enforced via `CIBW_BUILD`. Windows isn't in the
//! matrix because cibuildwheel can't cross-build Windows wheels on a
//! macOS host; that path stays in `python-wheels.yml` until a
//! self-hosted Windows runner exists.
//!
//! See KG decisions:
//!
//!   * `decision_python_ci_matrix_pin_to_latest_only`
//!   * `decision_python_ci_unified_via_cibuildwheel` (this iteration)
//!
//! Both have `do_not_revert: true`.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workflow_path() -> PathBuf {
    repo_root()
        .join(".github")
        .join("workflows")
        .join("python.yml")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

fn read_workflow() -> String {
    let p = workflow_path();
    assert!(
        p.exists(),
        ".github/workflows/python.yml must exist — task 00122 deliverable",
    );
    read(&p)
}

// ── 1. File exists with the right top-level name ───────────────────────

#[test]
fn python_ci_matrix_workflow_exists() {
    let wf = read_workflow();
    assert!(
        wf.lines()
            .any(|line| line.starts_with("name:") && line.to_lowercase().contains("python")),
        "python.yml `name:` must include `Python` — distinguishes \
         it from non-Python workflows in the Actions UI",
    );
}

// ── 2. Triggers ────────────────────────────────────────────────────────

#[test]
fn python_ci_matrix_does_not_run_on_pull_request() {
    // CI runner-contention fix (2026-06-10): the cibuildwheel matrix is
    // the intrinsically heaviest Python gate (a wheel build + test per
    // platform, manylinux container pull for Linux) and on the single
    // self-hosted runner it serialised behind every other PR job while
    // duplicating the lint/test gate the lighter `python-ci.yml` already
    // runs on every PR. So the matrix moved OFF the PR path to
    // `push:main` + `workflow_dispatch`, mirroring the already-de-PR-
    // triggered `python-wheels.yml` / `docker-publish.yml` /
    // `cross-compile.yml`. PR Python coverage is preserved by
    // `python-ci.yml` (locked by
    // `python_package_wheels_tests.rs::python_ci_workflow_runs_full_lint_and_test_gate`).
    let wf = read_workflow();
    assert!(
        !wf.contains("pull_request:"),
        "python.yml must NOT include a `pull_request:` trigger — the heavy \
         cibuildwheel matrix is gated off the PR path to keep the single \
         self-hosted runner free; the lighter `python-ci.yml` is the PR gate",
    );
}

#[test]
fn python_ci_matrix_also_runs_on_push_main_and_workflow_dispatch() {
    let wf = read_workflow();
    assert!(
        wf.contains("push:"),
        "python.yml must include `push:` trigger"
    );
    assert!(
        wf.contains("workflow_dispatch:"),
        "python.yml must include `workflow_dispatch:` trigger",
    );
}

#[test]
fn python_ci_matrix_uses_path_filters() {
    let wf = read_workflow();
    assert!(
        wf.contains("paths:"),
        "python.yml must declare `paths:` filters"
    );
    for needle in ["drevo-py/**", "src/**", "python.yml"] {
        assert!(
            wf.contains(needle),
            "python.yml `paths:` must include `{needle}`",
        );
    }
}

// ── 3. cibuildwheel mechanism ──────────────────────────────────────────

#[test]
fn python_ci_matrix_uses_cibuildwheel_not_gha_container_directive() {
    let wf = read_workflow();
    // The whole point of pivoting this workflow: cibuildwheel
    // manages docker itself, so the Linux cell works on the
    // self-hosted macOS host without hitting GHA's "Container
    // operations are only supported on Linux runners" wall.
    assert!(
        wf.contains("cibuildwheel"),
        "python.yml must invoke cibuildwheel — the GHA `container:` \
         directive is rejected by the macOS runner agent",
    );
    // Negative: no GHA `container:` directive. A bare top-level
    // `container:` key would re-enter the broken regime.
    let has_gha_container_directive = wf.lines().any(|line| {
        let t = line.trim_start();
        // Matches `container:` at job/workflow level (4 or fewer
        // leading spaces of indent — strategy.matrix is deeper).
        // We accept `container: ${{ ... }}` or `container: image`
        // forms; rule it out entirely.
        let indent = line.len() - t.len();
        indent <= 4 && t.starts_with("container:")
    });
    assert!(
        !has_gha_container_directive,
        "python.yml must NOT use the GHA `container:` directive — \
         it fails on macOS runners with 'Container operations are \
         only supported on Linux runners'. Use cibuildwheel + \
         CIBW_PLATFORM instead.",
    );
}

#[test]
fn python_ci_matrix_pins_python_to_latest_under_abi3() {
    let wf = read_workflow();
    // CIBW_BUILD restricted to cp313 only — abi3-py310 wheel works
    // on every CPython 3.10+, so the latest minor covers the
    // supported range. KG: `decision_python_ci_matrix_pin_to_latest_only`.
    assert!(
        wf.contains("CIBW_BUILD: \"cp313-*\"") || wf.contains("CIBW_BUILD: 'cp313-*'"),
        "python.yml must declare `CIBW_BUILD: \"cp313-*\"` — pinning \
         to the latest CPython under abi3-py310",
    );
    // Negative: no other cp31X-* in the build list.
    for forbidden in ["cp310-*", "cp311-*", "cp312-*"] {
        assert!(
            !wf.contains(forbidden),
            "python.yml must NOT include `{forbidden}` in CIBW_BUILD \
             — abi3-py310 makes multi-minor builds redundant. If you \
             genuinely need another minor, relax this test and the \
             pin-to-latest decision in the same PR.",
        );
    }
}

#[test]
fn python_ci_matrix_declares_platform_axis() {
    let wf = read_workflow();
    assert!(
        wf.contains("strategy:") && wf.contains("matrix:"),
        "python.yml must declare a `strategy: matrix:` block",
    );
    // The platform axis drives CIBW_PLATFORM. Two cells: macos +
    // linux. Windows excluded — cibuildwheel can't cross-build it
    // on a macOS host.
    assert!(
        wf.contains("platform: [macos, linux]")
            || wf.contains("- macos\n")
            || wf.contains("- linux\n"),
        "python.yml strategy.matrix.platform must list both `macos` \
         and `linux` cells",
    );
    assert!(
        wf.contains("CIBW_PLATFORM: ${{ matrix.platform }}"),
        "python.yml must thread `matrix.platform` into \
         `CIBW_PLATFORM` — without it cibuildwheel only builds the \
         host's native platform",
    );
}

#[test]
fn python_ci_matrix_all_cells_self_hosted() {
    let wf = read_workflow();
    // Both cells (macos and linux) run on the self-hosted macOS
    // host. cibuildwheel handles the Linux container itself.
    // Without this guard, a future contributor might "split out
    // the linux cell to ubuntu-latest" and re-enter GHA billing.
    assert!(
        wf.contains("runs-on: [self-hosted, macOS]"),
        "python.yml must set `runs-on: [self-hosted, macOS]` for \
         the matrix job — cibuildwheel manages Linux container on \
         the same macOS host",
    );
    // Negative: no GitHub-hosted runner labels.
    for ghd in ["ubuntu-latest", "windows-latest"] {
        let bad = wf.lines().any(|line| {
            let t = line.trim_start();
            t.starts_with("runs-on:") && t.contains(ghd)
        });
        assert!(
            !bad,
            "python.yml must NOT route any cell to `{ghd}` — the \
             whole point of the cibuildwheel pivot is zero \
             GitHub-hosted minutes",
        );
    }
}

// ── 4. CIBW_TEST_COMMAND covers all gates ──────────────────────────────

#[test]
fn python_ci_matrix_test_command_runs_three_pytest_layers() {
    let wf = read_workflow();
    for layer in ["tests/unit", "tests/integration", "tests/e2e"] {
        assert!(
            wf.contains(layer),
            "python.yml CIBW_TEST_COMMAND must invoke pytest against \
             `{layer}/` — task 00122 runs the three layers \
             independently so a failure surfaces in the log without \
             re-reading the bulk pytest output",
        );
    }
}

#[test]
fn python_ci_matrix_test_command_runs_mypy_ruff_black() {
    let wf = read_workflow();
    assert!(
        wf.contains("mypy --strict"),
        "python.yml CIBW_TEST_COMMAND must invoke `mypy --strict` — \
         guards .pyi stub drift against the runtime shim",
    );
    assert!(
        wf.contains("ruff check"),
        "python.yml CIBW_TEST_COMMAND must invoke `ruff check`",
    );
    assert!(
        wf.contains("black --check"),
        "python.yml CIBW_TEST_COMMAND must invoke `black --check`",
    );
}

#[test]
fn python_ci_matrix_test_requires_lists_pytest_and_lint_tooling() {
    let wf = read_workflow();
    // CIBW_TEST_REQUIRES installs pytest/mypy/ruff/black into the
    // wheel's isolated test env. Without it, CIBW_TEST_COMMAND
    // would crash with "command not found".
    assert!(
        wf.contains("CIBW_TEST_REQUIRES"),
        "python.yml must declare `CIBW_TEST_REQUIRES` so the test \
         env has pytest + mypy + ruff + black available",
    );
    for tool in ["pytest", "mypy", "ruff", "black"] {
        assert!(
            wf.contains(tool),
            "python.yml CIBW_TEST_REQUIRES (or CIBW_TEST_COMMAND) \
             must mention `{tool}`",
        );
    }
}

// ── 5. Concurrency / cost / release guards ─────────────────────────────

#[test]
fn python_ci_matrix_uses_concurrency_with_cancel_in_progress() {
    let wf = read_workflow();
    assert!(
        wf.contains("concurrency:") && wf.contains("cancel-in-progress: true"),
        "python.yml must declare concurrency with cancel-in-progress: true",
    );
    assert!(
        wf.contains("python-matrix-") || wf.contains("python-ci-matrix-"),
        "python.yml concurrency group must be distinct from \
         `python-ci-` and `python-wheels-`",
    );
}

#[test]
fn python_ci_matrix_does_not_publish_to_pypi() {
    let wf = read_workflow();
    // The cibuildwheel pivot is a TEST gate, not a release pipeline.
    // python-wheels.yml is the release-staging workflow.
    assert!(
        !wf.contains("twine upload") && !wf.contains("pypi-publish"),
        "python.yml MUST NOT publish to PyPI — it is a PR gate; \
         release flow stays in python-wheels.yml",
    );
}

// ── 6. README + CHANGELOG bookkeeping ──────────────────────────────────

#[test]
fn readme_ticks_task_00122() {
    let readme = read(&repo_root().join("README.md"));
    assert!(
        readme.contains("[x] `00122`"),
        "README.md must tick `[x] 00122` after the cibuildwheel-\
         driven matrix lands",
    );
}

#[test]
fn readme_progress_note_for_00122_landed() {
    let readme = read(&repo_root().join("README.md"));
    assert!(
        readme.contains("after task 00122"),
        "README.md must include a `Progress (YYYY-MM-DD, after task \
         00122)` note",
    );
}

#[test]
fn changelog_records_task_00122() {
    let body = read(&repo_root().join("drevo-py").join("CHANGELOG.md"));
    assert!(
        body.contains("00122"),
        "drevo-py/CHANGELOG.md must reference task `00122`",
    );
}
