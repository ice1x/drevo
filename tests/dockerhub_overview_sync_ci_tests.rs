//! CI invariant: the Docker Hub repository **overview** is synced from the
//! committed `DOCKERHUB.md`.
//!
//! Docker Hub's overview (the long description on the repo's page) is
//! populated only through the web form or the Docker Hub API — a
//! `docker push` never touches it. So without automation the published
//! overview silently drifts from, or simply never matches, the source of
//! truth in the repo (the page showed "No overview available" until this
//! was set up). `.github/workflows/dockerhub-overview.yml` closes that gap:
//! on every push to `main` that changes `DOCKERHUB.md`, it PATCHes the
//! Docker Hub API with the file's contents.
//!
//! These pure-text invariants keep the file and the workflow from drifting
//! apart — e.g. a rename of `DOCKERHUB.md` that forgets the workflow, or a
//! workflow edit that drops the sync. Mirrors the pattern of
//! `tests/ci_self_hosted_runner_tests.rs` /
//! `tests/cbindgen_header_sync_ci_tests.rs`.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn workflow_path() -> PathBuf {
    repo_root()
        .join(".github")
        .join("workflows")
        .join("dockerhub-overview.yml")
}

fn workflow() -> String {
    let path = workflow_path();
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

#[test]
fn dockerhub_overview_source_file_exists() {
    let path = repo_root().join("DOCKERHUB.md");
    assert!(
        path.is_file(),
        "DOCKERHUB.md (the Docker Hub overview source of truth) must exist at the repo root"
    );
    let body = fs::read_to_string(&path).expect("read DOCKERHUB.md");
    assert!(
        body.contains("drevo"),
        "DOCKERHUB.md must actually describe drevo"
    );
}

#[test]
fn overview_sync_workflow_reads_the_committed_file() {
    let wf = workflow();
    assert!(
        wf.contains("DOCKERHUB.md"),
        "the overview-sync workflow must read DOCKERHUB.md as its source"
    );
}

#[test]
fn overview_sync_workflow_targets_docker_hub_api_for_this_repo() {
    let wf = workflow();
    assert!(
        wf.contains("hub.docker.com"),
        "the workflow must call the Docker Hub API (hub.docker.com)"
    );
    assert!(
        wf.contains("ice1x/drevo"),
        "the workflow must target the ice1x/drevo Docker Hub repository"
    );
}

#[test]
fn overview_sync_workflow_triggers_on_main_changes_to_the_file() {
    let wf = workflow();
    // Path-filtered push to main so the sync fires exactly when the source
    // changes (and a manual `workflow_dispatch` escape hatch).
    assert!(
        wf.contains("workflow_dispatch"),
        "the workflow should offer a manual trigger"
    );
    assert!(
        wf.contains("branches:") && wf.contains("main"),
        "the workflow must trigger on pushes to main"
    );
}
