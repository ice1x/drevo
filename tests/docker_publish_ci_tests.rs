//! GitHub Actions workflow tests for the GHCR Docker publish pipeline.
//!
//! Phase 8 task 00051: build the production drevo image from
//! `Dockerfile` and push it to GitHub Container Registry
//! (`ghcr.io/ice1x/drevo`) on every semver tag (`v*`) — the image is a
//! release artifact (changed 2026-07-30 from per-push-to-`main`, which
//! blocked the single self-hosted runner with a ~40-min QEMU build every
//! merge). Pull requests build the image but never push, so a forked-PR
//! clone cannot publish under the project's namespace.
//!
//! These tests parse `.github/workflows/docker-publish.yml` as text —
//! no Docker daemon, no GHCR credentials, no `act` runner needed.
//! They mirror the pattern of `tests/dockerfile_tests.rs` and
//! `tests/k8s_manifests_tests.rs`: pin every wire-format invariant
//! that would silently break the publish pipeline if a future edit
//! drifted from the intent of this task.
//!
//! What is locked here (each item is one or more tests below):
//!
//! 1.  The workflow file lives at the canonical path so other tooling
//!     (Renovate, dependabot, branch-protection rules) can find it.
//! 2.  Triggers — push of `v*` tags (release-only; NOT push-to-`main`).
//!     Plus a `workflow_dispatch` so ops can re-publish without a code
//!     change.
//! 3.  Permissions — `contents: read` + `packages: write` only. No
//!     `actions: write`, no `id-token: write`. Smallest possible token
//!     surface for a publish workflow (see the GitHub OIDC hardening
//!     guide).
//! 4.  Registry & image — `ghcr.io` and `ghcr.io/ice1x/drevo`. The
//!     namespace matches the value baked into `k8s/base/deployment.yaml`
//!     (`image: ghcr.io/ice1x/drevo:<tag>`); a drift here would break
//!     the K8s deployment after the very first publish.
//! 5.  Auth — `docker/login-action` against `ghcr.io` with
//!     `${{ github.actor }}` + `${{ secrets.GITHUB_TOKEN }}`. No PAT.
//! 6.  Build / push — `docker/build-push-action` with
//!     `push: ${{ github.event_name != 'pull_request' }}` so PRs build
//!     and exercise the Dockerfile but never push under the project's
//!     namespace.
//! 7.  Multi-arch — `linux/amd64,linux/arm64`. amd64 is the CI runner
//!     default; arm64 matters because the project's developer baseline
//!     is Apple Silicon (the README "Performance Targets" section
//!     calibrates against Apple Silicon).
//! 8.  Metadata / tagging — `docker/metadata-action` emits, at minimum:
//!     a SHA tag (`sha-<short>`), `latest` only on the default branch,
//!     and semver tags `vMAJOR.MINOR.PATCH` / `vMAJOR.MINOR` from git
//!     tags. The first two tags are what the K8s overlays' `newTag`
//!     fields reference (`v0.1.0-dev`, `v0.1.0`); the third unlocks
//!     a future release flow without a workflow edit.
//! 9.  README documents the publish surface so a new operator knows
//!     which tag to pull.

use std::fs;
use std::path::{Path, PathBuf};

fn workflow_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".github")
        .join("workflows")
        .join("docker-publish.yml")
}

fn read_workflow() -> String {
    fs::read_to_string(workflow_path()).unwrap_or_else(|e| {
        panic!(
            "failed to read {}: {} — task 00051 requires this file",
            workflow_path().display(),
            e
        )
    })
}

/// Strip YAML `#`-prefixed comment lines and end-of-line comments
/// before searching for forbidden substrings. Without this, the
/// workflow's own documentation comments (which explain *why* we do
/// NOT grant certain permissions) trip the negative assertions.
fn read_workflow_code_only() -> String {
    let raw = read_workflow();
    raw.lines()
        .map(|line| {
            // Trim a trailing comment if the `#` appears outside a
            // quoted string. The current workflow has no `#` inside
            // strings, so a plain split is sufficient — flag and tighten
            // if a future edit introduces one.
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                ""
            } else if let Some((code, _comment)) = line.split_once(" #") {
                code
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_readme() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
    fs::read_to_string(path).expect("failed to read README.md")
}

fn read_k8s_deployment() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("k8s")
        .join("base")
        .join("deployment.yaml");
    fs::read_to_string(path).expect("failed to read k8s/base/deployment.yaml")
}

// ------------------------------------------------------------------
// 1. Layout
// ------------------------------------------------------------------

#[test]
fn docker_publish_workflow_exists() {
    assert!(
        workflow_path().is_file(),
        ".github/workflows/docker-publish.yml must exist (Phase 8 task 00051)"
    );
}

#[test]
fn docker_publish_workflow_has_name() {
    let w = read_workflow();
    assert!(
        w.lines().any(|l| l.trim_start().starts_with("name:")),
        "workflow must declare a top-level `name:` field for the Actions UI"
    );
}

// ------------------------------------------------------------------
// 2. Triggers
// ------------------------------------------------------------------

#[test]
fn docker_publish_does_not_trigger_on_push_to_main() {
    // Changed 2026-07-30: the image is a RELEASE artifact, published on
    // `v*` tags only — NOT on every push to `main`. On the single
    // self-hosted runner the ~40-min QEMU multi-arch build serialised
    // behind and blocked every merge, so the per-merge trigger was
    // removed. `:latest` tracks the newest release tag; `make release`
    // (which pushes a `vX.Y.Z` tag) is what publishes the image.
    let w = read_workflow();
    // The push trigger must NOT filter by branch (no branch push at all).
    assert!(
        !w.contains("branches:"),
        "workflow must NOT trigger on a branch push (e.g. `main`) — the image publishes on \
         `v*` tags only; a per-merge multi-arch build blocks the single self-hosted runner"
    );
}

#[test]
fn docker_publish_triggers_on_version_tags() {
    let w = read_workflow();
    assert!(
        w.contains("'v*'") || w.contains("\"v*\"") || w.contains("- v*"),
        "workflow must publish on semver tags matching `v*` (e.g. v0.1.0)"
    );
}

#[test]
fn docker_publish_does_not_trigger_on_pull_request() {
    // Inverted from the original "must trigger on pull_request" rule.
    // Reason: on the self-hosted Apple Silicon runner, the `linux/amd64`
    // half of the multi-arch matrix runs under QEMU emulation and the
    // end-to-end build cumulated to ~19 min per PR. That duplicates
    // ci.yml's compilation + test surface and only adds value when the
    // change touches `Dockerfile` itself (rare). The next push-to-main
    // build catches any Dockerfile regression before the image actually
    // ships. Contributors who genuinely need a PR-time multi-arch build
    // can use `workflow_dispatch` from the Actions UI on the PR branch.
    let w = read_workflow();
    let trimmed: String = w
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            !t.starts_with('#') && !t.is_empty()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !trimmed.contains("pull_request:"),
        "workflow MUST NOT declare a `pull_request:` trigger — the PR-time \
         multi-arch build cost ~19 min per PR under QEMU emulation and \
         duplicates ci.yml's compilation. Remove the trigger entirely; \
         do not narrow it with paths."
    );
}

#[test]
fn docker_publish_supports_workflow_dispatch() {
    let w = read_workflow();
    assert!(
        w.contains("workflow_dispatch:"),
        "workflow must expose `workflow_dispatch` so ops can re-publish without a code change"
    );
}

// ------------------------------------------------------------------
// 3. Permissions
// ------------------------------------------------------------------

#[test]
fn docker_publish_grants_packages_write() {
    let w = read_workflow();
    assert!(
        w.contains("packages: write"),
        "workflow must grant `packages: write` so the GITHUB_TOKEN can push to ghcr.io"
    );
}

#[test]
fn docker_publish_grants_contents_read() {
    let w = read_workflow();
    assert!(
        w.contains("contents: read"),
        "workflow must declare `contents: read` (least-privilege — checkout needs it)"
    );
}

#[test]
fn docker_publish_does_not_grant_actions_write() {
    let w = read_workflow_code_only();
    assert!(
        !w.contains("actions: write"),
        "workflow must not grant `actions: write` — a publish job does not need to mutate \
         workflow runs. Keep the token surface minimal."
    );
}

#[test]
fn docker_publish_does_not_grant_id_token_write() {
    let w = read_workflow_code_only();
    assert!(
        !w.contains("id-token: write"),
        "workflow must not request OIDC `id-token: write` — GHCR auth uses the GITHUB_TOKEN, not \
         a cloud-provider OIDC exchange. Re-add only if a future task introduces cosign / SLSA."
    );
}

// ------------------------------------------------------------------
// 4. Registry & image
// ------------------------------------------------------------------

#[test]
fn docker_publish_targets_ghcr() {
    let w = read_workflow();
    assert!(
        w.contains("ghcr.io"),
        "workflow must target `ghcr.io` (the canonical GitHub Container Registry host)"
    );
}

#[test]
fn docker_publish_image_namespace_matches_k8s_deployment() {
    // The K8s Deployment manifest pins
    //   image: ghcr.io/ice1x/drevo:<tag>
    // — a drift in the publish workflow would silently break the
    // first cluster pull after the workflow lands.
    let w = read_workflow();
    let dep = read_k8s_deployment();
    assert!(
        dep.contains("ghcr.io/ice1x/drevo"),
        "precondition: k8s/base/deployment.yaml is expected to reference \
         `ghcr.io/ice1x/drevo` (task 00049). If this fails, the precondition broke — fix that \
         first."
    );
    assert!(
        w.contains("ghcr.io/ice1x/drevo") || w.contains("ice1x/drevo"),
        "workflow must publish to `ghcr.io/ice1x/drevo` — the same image namespace the K8s \
         deployment pulls from"
    );
}

// ------------------------------------------------------------------
// 5. Auth — login-action
// ------------------------------------------------------------------

#[test]
fn docker_publish_authenticates_without_login_action_or_keychain() {
    // Auth is pre-baked as a base64 `auths` blob into DOCKER_CONFIG instead of
    // `docker/login-action` / `docker login`: Docker Desktop on the self-hosted
    // macOS runner re-injects credsStore=osxkeychain on login (even into a `{}`
    // config), which fails headless with `User interaction is not allowed.
    // (-25308)`. Reading auths from the file invokes no credential helper.
    let w = read_workflow();
    assert!(
        !w.contains("uses: docker/login-action"),
        "workflow must NOT use docker/login-action — it triggers the macOS osxkeychain credsStore \
         (the comment block may still reference it by name to explain why)"
    );
    assert!(
        w.contains("\"auths\""),
        "workflow must pre-bake an `auths` blob into DOCKER_CONFIG/config.json for GHCR auth"
    );
}

#[test]
fn docker_publish_login_uses_github_token() {
    let w = read_workflow();
    assert!(
        w.contains("secrets.GITHUB_TOKEN"),
        "workflow must authenticate to ghcr.io with `secrets.GITHUB_TOKEN` — not a personal \
         access token, not a deploy key, not a hardcoded credential"
    );
}

#[test]
fn docker_publish_login_uses_github_actor() {
    let w = read_workflow();
    assert!(
        w.contains("github.actor"),
        "workflow must log in as `${{ github.actor }}` so the package owner audit trail is \
         the user who triggered the run (not a bot account)"
    );
}

// ------------------------------------------------------------------
// 6. Build & push
// ------------------------------------------------------------------

#[test]
fn docker_publish_uses_buildx() {
    let w = read_workflow();
    assert!(
        w.contains("docker/setup-buildx-action"),
        "workflow must `docker/setup-buildx-action` so multi-arch builds and cache export work"
    );
}

#[test]
fn docker_publish_uses_qemu_for_cross_arch() {
    let w = read_workflow();
    assert!(
        w.contains("docker/setup-qemu-action"),
        "workflow must `docker/setup-qemu-action` so the amd64 runner can emulate arm64"
    );
}

#[test]
fn docker_publish_uses_build_push_action() {
    let w = read_workflow();
    assert!(
        w.contains("docker/build-push-action"),
        "workflow must use `docker/build-push-action` (the official action — gives us SBOM, \
         provenance, multi-arch, and cache hooks in one step)"
    );
}

#[test]
fn docker_publish_only_pushes_on_non_pr_events() {
    // PR builds must NOT push — a fork PR clone with no special perms
    // still triggers `pull_request`, and we don't want them publishing
    // under the project namespace.
    let w = read_workflow();
    let has_pr_gated_push = w.contains("push:")
        && (w.contains("github.event_name != 'pull_request'")
            || w.contains("github.event_name != \"pull_request\""));
    assert!(
        has_pr_gated_push,
        "the `docker/build-push-action` step must set `push: ${{{{ github.event_name != \
         'pull_request' }}}}` so pull requests build the image (validating the Dockerfile) but \
         do NOT publish to ghcr.io"
    );
}

#[test]
fn docker_publish_dockerfile_is_default() {
    // We don't want to maintain two Dockerfiles. The repo root has
    // exactly one, and this workflow must use it.
    let w = read_workflow();
    let mentions_other_dockerfile = w.contains("file: ./Dockerfile.")
        || w.contains("file: Dockerfile.")
        || w.contains("file: ./docker/")
        || w.contains("file: docker/");
    assert!(
        !mentions_other_dockerfile,
        "workflow must build from the repo-root `Dockerfile` — there is no secondary \
         Dockerfile in this project (task 00045)"
    );
}

// ------------------------------------------------------------------
// 7. Multi-arch
// ------------------------------------------------------------------

#[test]
fn docker_publish_builds_amd64() {
    let w = read_workflow();
    assert!(
        w.contains("linux/amd64"),
        "workflow must build `linux/amd64` — the canonical x86_64 server target"
    );
}

#[test]
fn docker_publish_builds_arm64() {
    let w = read_workflow();
    assert!(
        w.contains("linux/arm64"),
        "workflow must build `linux/arm64` — the project's developer baseline is Apple Silicon \
         (see README `Performance Targets`), and arm64 K8s nodes (Graviton, Ampere) are \
         increasingly common in production"
    );
}

// ------------------------------------------------------------------
// 8. Metadata / tagging
// ------------------------------------------------------------------

#[test]
fn docker_publish_uses_metadata_action() {
    let w = read_workflow();
    assert!(
        w.contains("docker/metadata-action"),
        "workflow must use `docker/metadata-action` to derive image tags from the git ref — \
         hand-rolled tag logic in shell drifts from semver"
    );
}

#[test]
fn docker_publish_emits_sha_tag() {
    let w = read_workflow();
    // `docker/metadata-action` emits a `sha-<short>` tag when the
    // `type=sha` rule is present. Lock that rule explicitly.
    assert!(
        w.contains("type=sha"),
        "workflow must include `type=sha` in `docker/metadata-action.tags` so every published \
         image carries an immutable `sha-<short>` tag — required for reproducible K8s rollouts"
    );
}

#[test]
fn docker_publish_emits_semver_tags_from_git_tags() {
    let w = read_workflow();
    assert!(
        w.contains("type=semver"),
        "workflow must include `type=semver` rules so a `v0.1.0` git tag produces image tags \
         `0.1.0` and `0.1` — required for the future release flow"
    );
}

#[test]
fn docker_publish_emits_latest_on_release_tags() {
    let w = read_workflow();
    // Changed 2026-07-30: the workflow runs on `v*` tags (not `main`), so
    // `:latest` is gated on a version tag ref rather than the default
    // branch — it tracks the newest release.
    let latest_gated =
        w.contains("latest") && (w.contains("refs/tags/v") || w.contains("startsWith(github.ref"));
    assert!(
        latest_gated,
        "workflow must emit the `latest` tag on `v*` release tags — e.g. \
         `type=raw,value=latest,enable=${{{{ startsWith(github.ref, 'refs/tags/v') }}}}`. \
         `:latest` should track the newest release, not a feature-branch build."
    );
}

// ------------------------------------------------------------------
// 9. README documentation
// ------------------------------------------------------------------

#[test]
fn readme_documents_ghcr_publish_workflow() {
    let r = read_readme();
    assert!(
        r.contains("ghcr.io/ice1x/drevo"),
        "README must reference the published image `ghcr.io/ice1x/drevo` so a new operator \
         knows which image to pull (task 00051)"
    );
}

#[test]
fn readme_marks_00051_done() {
    let r = read_readme();
    let has_done_line = r
        .lines()
        .any(|l| l.contains("`00051`") && l.contains("- [x]"));
    assert!(
        has_done_line,
        "README must tick `- [x] `00051`` once the workflow lands"
    );
}

// ------------------------------------------------------------------
// 10. Sibling-workflow non-regression — the main CI must not learn
// publish duties. Keep concerns separate.
// ------------------------------------------------------------------

#[test]
fn main_ci_workflow_does_not_push_images() {
    let ci_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".github")
        .join("workflows")
        .join("ci.yml");
    let ci = fs::read_to_string(ci_path).expect("failed to read ci.yml");
    assert!(
        !ci.contains("docker/login-action"),
        "the main CI workflow (ci.yml) must NOT log in to a container registry — publish \
         duties belong to docker-publish.yml so a CI test job can never push a \
         half-broken image"
    );
    assert!(
        !ci.contains("docker/build-push-action"),
        "the main CI workflow (ci.yml) must NOT use docker/build-push-action — publish duties \
         belong to docker-publish.yml"
    );
}
