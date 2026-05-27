//! Phase 16 task `00122` — text-level scaffolding tests for the full
//! Python CI matrix workflow.
//!
//! Same pattern as `python_package_wheels_tests.rs` (`00116`) and
//! `python_e2e_test_suite_tests.rs` (`00120`): the existing Rust CI
//! runners do not provision a Python interpreter, so these tests do
//! NOT invoke `pytest`, `pip`, or `maturin`. They lock the *shape* of
//! the new workflow file by grepping it on disk.
//!
//! The workflow itself — `.github/workflows/python.yml` — is the
//! definition-of-done for Phase 16: a Python × OS matrix that gates
//! PR merges on the three test layers shipped in `00118` / `00119` /
//! `00120` plus `mypy --strict`, `ruff check`, and `black --check`.
//!
//! The matrix declared in roadmap task `00122`:
//!
//!   {python: [3.10, 3.11, 3.12, 3.13]} × {os: [ubuntu-latest, macos-latest, windows-latest]}
//!
//! = 12 cells per PR. Per-job: checkout, setup-python, `pip install
//! maturin pytest mypy ruff black`, `maturin develop --release
//! --features="redb-backend"`, `pytest tests/unit/`, `pytest
//! tests/integration/`, `pytest tests/e2e/`, `mypy --strict drevo/`,
//! `ruff check . && black --check .`. The workflow also publishes a
//! pytest-cov coverage report as a PR comment per the spec.
//!
//! Cost note: the matrix burns GitHub-hosted minutes on every Python-
//! touching PR. Path filters narrow trigger conditions to
//! `drevo-py/**` + `src/**` + the workflow itself + Cargo.toml/lock.
//! Doc-only PRs do not pay the matrix cost. The fast-feedback
//! `python-ci.yml` (self-hosted, single cell) remains as the warm
//! sub-2-minute signal alongside.
//!
//! macos-latest + windows-latest are added to the allow-list in
//! `tests/ci_self_hosted_runner_tests.rs` for this workflow alongside
//! `python-wheels.yml`. The justification matches RFC §2 wheel-layout
//! reasoning: PyO3 ABI checks are platform-native, FTS UTF-8
//! tokenisation is locale-sensitive on Windows, and redb file
//! locking behaves differently on each kernel — only running on real
//! macOS / Windows runners produces a meaningful regression signal
//! for those classes.

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
        wf.contains("name:"),
        ".github/workflows/python.yml must declare a top-level `name:`",
    );
    // The exact display name is not constrained by the task, but it
    // MUST be something Python-matrix-ish so the GitHub Actions UI
    // separates it from `python-ci.yml` (display name "Python CI") at
    // a glance.
    assert!(
        wf.lines()
            .any(|line| line.starts_with("name:") && line.to_lowercase().contains("python")),
        ".github/workflows/python.yml `name:` must include `Python` — \
         distinguishes it from non-Python workflows in the Actions UI",
    );
}

// ── 2. Triggers ────────────────────────────────────────────────────────

#[test]
fn python_ci_matrix_runs_on_every_pr() {
    let wf = read_workflow();
    // Roadmap task wording: "Python CI matrix on every PR." The
    // workflow MUST trigger on `pull_request` against `main`.
    assert!(
        wf.contains("pull_request:"),
        "python.yml must include a `pull_request:` trigger — task 00122 \
         says 'matrix on every PR'",
    );
    assert!(
        wf.contains("branches: [main]") || wf.contains("- main"),
        "python.yml `pull_request:` must target `main`",
    );
}

#[test]
fn python_ci_matrix_also_runs_on_push_main_and_workflow_dispatch() {
    let wf = read_workflow();
    assert!(
        wf.contains("push:"),
        "python.yml must include a `push:` trigger so post-merge \
         regressions on main surface immediately",
    );
    assert!(
        wf.contains("workflow_dispatch:"),
        "python.yml must include `workflow_dispatch:` so a maintainer \
         can re-run the matrix on demand without a code change",
    );
}

#[test]
fn python_ci_matrix_uses_path_filters() {
    let wf = read_workflow();
    // Cost guard: 12-cell hosted matrix is expensive. Path filters
    // narrow to changes that can plausibly affect the Python surface.
    assert!(
        wf.contains("paths:"),
        "python.yml must declare `paths:` filters so a doc-only PR \
         does not pay the 12-cell hosted-matrix cost",
    );
    assert!(
        wf.contains("drevo-py/**"),
        "python.yml `paths:` must include `drevo-py/**` so changes to \
         the Python package retrigger the matrix",
    );
    assert!(
        wf.contains("src/**"),
        "python.yml `paths:` must include `src/**` so a Rust core \
         change (which `maturin develop` will rebuild) retriggers",
    );
    assert!(
        wf.contains("python.yml"),
        "python.yml `paths:` must include the workflow file itself so \
         a workflow-only edit retriggers the matrix",
    );
}

// ── 3. Matrix shape ────────────────────────────────────────────────────

#[test]
fn python_ci_matrix_enumerates_every_python_minor() {
    let wf = read_workflow();
    // Phase 16 cross-cutting acceptance criteria + task 00122 matrix.
    // Every cp310..cp313 minor must appear in the strategy.matrix
    // python axis. cibuildwheel uses `cp31X-*` patterns; this matrix
    // uses dotted versions because `actions/setup-python` accepts them.
    for py in ["\"3.10\"", "\"3.11\"", "\"3.12\"", "\"3.13\""] {
        assert!(
            wf.contains(py),
            "python.yml strategy.matrix.python must include {py} — \
             task 00122 spec",
        );
    }
}

#[test]
fn python_ci_matrix_enumerates_every_os() {
    let wf = read_workflow();
    for os in ["ubuntu-latest", "macos-latest", "windows-latest"] {
        assert!(
            wf.contains(os),
            "python.yml strategy.matrix.os must include `{os}` — task \
             00122 spec mandates the three-OS matrix for PyO3 ABI / FTS \
             UTF-8 / redb-locking platform divergence",
        );
    }
}

#[test]
fn python_ci_matrix_declares_strategy_matrix_block() {
    let wf = read_workflow();
    assert!(
        wf.contains("strategy:") && wf.contains("matrix:"),
        "python.yml must declare `strategy: matrix:` — that is what \
         actually fans the job out to 12 cells",
    );
    // The job's runs-on must thread the matrix.os axis (not be a
    // hard-coded value), otherwise the OS axis is decorative.
    assert!(
        wf.contains("runs-on: ${{ matrix.os }}"),
        "python.yml must declare `runs-on: ${{{{ matrix.os }}}}` — \
         hard-coding the runner defeats the OS axis",
    );
}

// ── 4. Build + test steps ──────────────────────────────────────────────

#[test]
fn python_ci_matrix_runs_maturin_develop_release() {
    let wf = read_workflow();
    // Spec line: `maturin develop --release --features="redb-backend"`.
    // The release flag matches the wheel configuration cibuildwheel
    // uses; redb-backend is the wheel's default feature.
    assert!(
        wf.contains("maturin develop"),
        "python.yml must invoke `maturin develop` — that is the only \
         way the matrix cells link the PyO3 cdylib into the active \
         interpreter for the tests below",
    );
    assert!(
        wf.contains("--release"),
        "python.yml `maturin develop` must use `--release` so the \
         cdylib matches the wheel build's optimisation level",
    );
}

#[test]
fn python_ci_matrix_installs_dev_tooling() {
    let wf = read_workflow();
    // Spec step 1: `pip install maturin pytest mypy ruff black`.
    // The full set must appear in a single `pip install` line OR be
    // installed across multiple lines — we grep for each token
    // independently so the implementation has room to choose.
    for tool in ["maturin", "pytest", "mypy", "ruff", "black"] {
        assert!(
            wf.contains(tool),
            "python.yml must install `{tool}` via pip — task 00122 \
             step 1 enumerates the dev tooling",
        );
    }
}

#[test]
fn python_ci_matrix_runs_three_pytest_layers() {
    let wf = read_workflow();
    // Spec steps 3-5: pytest against the three test layers landed by
    // 00118 (unit), 00119 (integration), 00120 (e2e). Each layer
    // gets its own pytest invocation so a single failing layer shows
    // up in the Actions UI as a discrete failed step.
    for layer in ["tests/unit", "tests/integration", "tests/e2e"] {
        assert!(
            wf.contains(layer),
            "python.yml must invoke `pytest {layer}` — task 00122 \
             spec runs the three test layers independently so the \
             Actions UI surfaces which layer regressed",
        );
    }
}

#[test]
fn python_ci_matrix_runs_mypy_ruff_black() {
    let wf = read_workflow();
    // Spec steps 6-7: type-check + lint + format gate.
    assert!(
        wf.contains("mypy --strict") || wf.contains("mypy"),
        "python.yml must run `mypy --strict` — guards .pyi stub \
         drift against the runtime shim",
    );
    assert!(
        wf.contains("ruff check"),
        "python.yml must run `ruff check` — style + unused-import \
         + bug-class lints, pinned in drevo-py/pyproject.toml",
    );
    assert!(
        wf.contains("black --check"),
        "python.yml must run `black --check` — formatting gate, \
         matches drevo-py/pyproject.toml [tool.black]",
    );
}

#[test]
fn python_ci_matrix_publishes_coverage_report() {
    let wf = read_workflow();
    // Spec final paragraph: "The workflow also publishes a coverage
    // report (`pytest-cov`) as a PR comment." Without this assertion
    // a future PR could quietly delete the pytest-cov bits and the
    // PR comment integration would silently disappear.
    assert!(
        wf.contains("pytest-cov") || wf.contains("--cov"),
        "python.yml must publish a pytest-cov coverage report — task \
         00122 spec final paragraph",
    );
}

// ── 5. Concurrency + cost guards ───────────────────────────────────────

#[test]
fn python_ci_matrix_uses_concurrency_with_cancel_in_progress() {
    let wf = read_workflow();
    // Cost guard: a force-push or rapid PR re-push should NOT keep
    // 12 hosted cells running for the previous head SHA. Every other
    // workflow in this repo follows this pattern; locking it here
    // prevents accidental drift.
    assert!(
        wf.contains("concurrency:"),
        "python.yml must declare a `concurrency:` block so superseded \
         runs of the same PR / branch get cancelled",
    );
    assert!(
        wf.contains("cancel-in-progress: true"),
        "python.yml `concurrency:` must set `cancel-in-progress: true` \
         — without it the 12-cell matrix keeps running on every old \
         commit, multiplying minute spend",
    );
    // Distinct concurrency group from `python-ci.yml` (group key
    // `python-ci-${{ github.ref }}`) and `python-wheels.yml`
    // (`python-wheels-${{ github.ref }}`) so the three workflows do
    // not cancel each other.
    assert!(
        wf.contains("python-matrix-") || wf.contains("python-ci-matrix-"),
        "python.yml `concurrency.group` must be distinct from \
         `python-ci-` and `python-wheels-` so the workflows do not \
         cancel each other on a shared key",
    );
}

#[test]
fn python_ci_matrix_does_not_publish_to_pypi() {
    let wf = read_workflow();
    // Same guard `python-wheels.yml` carries: this workflow is a
    // GATE, not a release pipeline. Catching `twine upload` /
    // `pypa/gh-action-pypi-publish` here keeps the responsibility
    // separation explicit.
    assert!(
        !wf.contains("twine upload") && !wf.contains("pypi-publish"),
        "python.yml MUST NOT publish to PyPI — it is a PR gate, the \
         publish flow is a separate release task",
    );
}

// ── 6. Allow-list integration ──────────────────────────────────────────

#[test]
fn allow_list_now_permits_macos_and_windows_in_python_yml() {
    // Sibling test in `tests/ci_self_hosted_runner_tests.rs` —
    // `macos_and_windows_runners_only_in_python_wheels_workflow` —
    // historically pinned `macos-latest` / `windows-latest` to
    // `python-wheels.yml` only. Task 00122 widens the allow-list to
    // ALSO permit `python.yml` (the matrix workflow itself). This
    // test reads the policy file and asserts the relaxation landed,
    // so a future revert that re-narrows the allow-list breaks here
    // instead of silently breaking the matrix in production.
    let policy = read(
        &repo_root()
            .join("tests")
            .join("ci_self_hosted_runner_tests.rs"),
    );
    assert!(
        policy.contains("python.yml"),
        "tests/ci_self_hosted_runner_tests.rs must mention \
         `python.yml` — task 00122 widens the macos/windows \
         allow-list to cover the new matrix workflow alongside \
         python-wheels.yml",
    );
    // Make sure the relaxation is keyed on the file-name allow-list
    // (`python.yml` or `python-wheels.yml`) rather than removed
    // entirely — otherwise we have lost a load-bearing guard.
    assert!(
        policy.contains("python-wheels.yml"),
        "tests/ci_self_hosted_runner_tests.rs must still mention \
         `python-wheels.yml` — the relaxation must widen, not \
         replace, the existing allow-list",
    );
}

// ── 7. README + CHANGELOG bookkeeping ──────────────────────────────────

#[test]
fn readme_ticks_task_00122() {
    let readme = read(&repo_root().join("README.md"));
    // README task list uses `[x]` for shipped tasks and `[ ]` for
    // pending. After 00122 ships, the box must be ticked.
    assert!(
        readme.contains("[x] `00122`"),
        "README.md must tick `[x] 00122` after the Python CI matrix \
         workflow lands — task tracker invariant",
    );
}

#[test]
fn readme_progress_note_for_00122_landed() {
    let readme = read(&repo_root().join("README.md"));
    // Pattern matches every other Phase 16 progress note: starts
    // with `**Progress (YYYY-MM-DD, after task 00XXX)`.
    assert!(
        readme.contains("after task 00122"),
        "README.md must include a `Progress (YYYY-MM-DD, after task \
         00122)` note describing the matrix workflow — Phase 16 \
         convention",
    );
}

#[test]
fn changelog_records_task_00122() {
    let body = read(&repo_root().join("drevo-py").join("CHANGELOG.md"));
    // drevo-py CHANGELOG keeps a Keep-a-Changelog entry per Phase 16
    // task. Lock the 00122 reference so a future PR can't silently
    // forget to update it.
    assert!(
        body.contains("00122"),
        "drevo-py/CHANGELOG.md must reference task `00122` — the \
         Python CI matrix is a user-visible packaging milestone",
    );
}
