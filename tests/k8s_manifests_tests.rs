//! Kubernetes manifest structure and convention tests.
//!
//! Phase 8 task 00049: verify the manifests under `k8s/` deploy drevo as
//! a single-replica HTTP service with a persistent volume mounted at
//! `/data` and Kubernetes-native liveness/readiness probes wired to the
//! drevo HTTP endpoints `/health` and `/ready`.
//!
//! The tests parse each manifest as text — no `kubectl`, no `kube-rs`
//! crate, no running cluster required. This mirrors the pattern used by
//! `tests/dockerfile_tests.rs` and `tests/docker_compose_tests.rs`.

use std::fs;
use std::path::{Path, PathBuf};

fn k8s_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("k8s")
}

fn read_manifest(name: &str) -> String {
    let path = k8s_dir().join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read k8s manifest {}: {}", path.display(), e))
}

// ------------------------------------------------------------------
// Layout: required files exist
// ------------------------------------------------------------------

#[test]
fn k8s_directory_exists() {
    assert!(
        k8s_dir().is_dir(),
        "k8s/ directory must exist at project root"
    );
}

#[test]
fn k8s_deployment_manifest_exists() {
    assert!(
        k8s_dir().join("deployment.yaml").is_file(),
        "k8s/deployment.yaml must exist"
    );
}

#[test]
fn k8s_service_manifest_exists() {
    assert!(
        k8s_dir().join("service.yaml").is_file(),
        "k8s/service.yaml must exist"
    );
}

#[test]
fn k8s_pvc_manifest_exists() {
    assert!(
        k8s_dir().join("pvc.yaml").is_file(),
        "k8s/pvc.yaml must exist"
    );
}

#[test]
fn k8s_readme_exists() {
    assert!(
        k8s_dir().join("README.md").is_file(),
        "k8s/README.md must document how to apply the manifests"
    );
}

// ------------------------------------------------------------------
// Deployment manifest
// ------------------------------------------------------------------

#[test]
fn deployment_declares_apps_v1_deployment() {
    let content = read_manifest("deployment.yaml");
    assert!(
        content.contains("apiVersion: apps/v1"),
        "deployment.yaml must use apiVersion apps/v1"
    );
    assert!(
        content.contains("kind: Deployment"),
        "deployment.yaml must have kind: Deployment"
    );
}

#[test]
fn deployment_names_the_workload_drevo() {
    let content = read_manifest("deployment.yaml");
    assert!(
        content.contains("name: drevo"),
        "deployment.yaml must name the workload `drevo`"
    );
}

#[test]
fn deployment_uses_ghcr_image() {
    // 00051 will publish to ghcr.io/ice1x/drevo. The manifest pins that
    // canonical name so the K8s deploy and the planned image-publish
    // pipeline cannot drift apart. A tag is required (no `:latest` only
    // because tests below assert a non-latest tag is present, but a
    // `:latest` or `:vX.Y.Z` is accepted here — the next test enforces
    // the latest/version policy).
    let content = read_manifest("deployment.yaml");
    assert!(
        content.contains("image: ghcr.io/ice1x/drevo"),
        "deployment.yaml must reference the ghcr.io/ice1x/drevo container image"
    );
}

#[test]
fn deployment_pins_an_image_tag() {
    // Untagged image references resolve to `:latest`, which is unstable
    // and disallowed by audit 00112 (server ops). Require an explicit
    // colon-tag on the image reference.
    let content = read_manifest("deployment.yaml");
    let image_line = content
        .lines()
        .find(|l| l.trim_start().starts_with("image:"))
        .expect("deployment.yaml must contain an `image:` line");
    let after = image_line.split_once("image:").unwrap().1.trim();
    // Strip surrounding quotes if any
    let stripped = after.trim_matches('"').trim_matches('\'');
    assert!(
        stripped.contains(':'),
        "image reference must include an explicit tag, got `{stripped}`"
    );
}

#[test]
fn deployment_exposes_container_port_8080() {
    let content = read_manifest("deployment.yaml");
    assert!(
        content.contains("containerPort: 8080"),
        "deployment.yaml must expose containerPort 8080"
    );
}

#[test]
fn deployment_sets_drevo_env_vars() {
    let content = read_manifest("deployment.yaml");
    for var in ["DREVO_HOST", "DREVO_PORT", "DREVO_DATA_DIR"] {
        assert!(
            content.contains(var),
            "deployment.yaml must set environment variable {var}"
        );
    }
}

#[test]
fn deployment_data_dir_points_to_pvc_mount() {
    // DREVO_DATA_DIR must equal the in-pod mountPath so the redb file
    // lives on the PVC. The Dockerfile defaults DREVO_DATA_DIR=/data;
    // the K8s manifest keeps that contract.
    //
    // Kubernetes env vars are split across two lines:
    //     - name: DREVO_DATA_DIR
    //       value: "/data"
    // Walk lines pairwise and accept the value on the next non-empty
    // line after the `name:` row.
    let content = read_manifest("deployment.yaml");
    let lines: Vec<&str> = content.lines().collect();
    let aligns = lines.iter().enumerate().any(|(i, l)| {
        if !l.contains("name: DREVO_DATA_DIR") {
            return false;
        }
        lines
            .iter()
            .skip(i + 1)
            .take(3)
            .any(|next| next.contains("value:") && next.contains("/data"))
    });
    assert!(
        aligns,
        "DREVO_DATA_DIR in deployment.yaml must be set to /data (env var name + value within 3 lines)"
    );
    assert!(
        content.contains("mountPath: /data"),
        "deployment.yaml must mount the data volume at /data"
    );
}

#[test]
fn deployment_mounts_persistentvolumeclaim() {
    let content = read_manifest("deployment.yaml");
    // The volumes block must reference the PVC by claimName. The PVC
    // manifest names the claim `drevo-data`.
    assert!(
        content.contains("persistentVolumeClaim:"),
        "deployment.yaml must declare a persistentVolumeClaim volume"
    );
    assert!(
        content.contains("claimName: drevo-data"),
        "deployment.yaml must reference PVC `drevo-data`"
    );
}

#[test]
fn deployment_has_liveness_probe_on_health() {
    // /health is the cheap liveness endpoint (task 00042). It must NOT
    // probe the DB — the deployment manifest wires it as livenessProbe.
    let content = read_manifest("deployment.yaml");
    assert!(
        content.contains("livenessProbe:"),
        "deployment.yaml must declare a livenessProbe"
    );
    let in_liveness = section_after(&content, "livenessProbe:");
    assert!(
        in_liveness.contains("path: /health"),
        "livenessProbe must hit GET /health, section:\n{in_liveness}"
    );
}

#[test]
fn deployment_has_readiness_probe_on_ready() {
    // /ready actively probes redb (task 00048) — it is the readiness
    // probe. Flipping to 503 during shutdown lets Endpoints withdraw
    // traffic before SIGKILL.
    let content = read_manifest("deployment.yaml");
    assert!(
        content.contains("readinessProbe:"),
        "deployment.yaml must declare a readinessProbe"
    );
    let in_readiness = section_after(&content, "readinessProbe:");
    assert!(
        in_readiness.contains("path: /ready"),
        "readinessProbe must hit GET /ready, section:\n{in_readiness}"
    );
}

#[test]
fn deployment_declares_termination_grace_period() {
    // SIGTERM-triggered graceful shutdown (task 00048) needs time to
    // drain in-flight requests before SIGKILL. K8s default is 30s; we
    // keep it explicit so a future operator does not lower it blindly.
    let content = read_manifest("deployment.yaml");
    assert!(
        content.contains("terminationGracePeriodSeconds:"),
        "deployment.yaml must set terminationGracePeriodSeconds explicitly"
    );
}

#[test]
fn deployment_pod_runs_as_non_root() {
    // Dockerfile drops to a `drevo` system user. The K8s manifest must
    // not silently re-grant root via securityContext.
    let content = read_manifest("deployment.yaml");
    assert!(
        content.contains("runAsNonRoot: true"),
        "deployment.yaml must set securityContext.runAsNonRoot: true"
    );
}

#[test]
fn deployment_uses_single_replica() {
    // RWO PVC + Deployment is a single-writer topology. Multiple
    // replicas would either fail to schedule (PVC bound to one node)
    // or corrupt the redb file. Lock to one replica until MVCC lands
    // (Phase 13 / task 00080).
    let content = read_manifest("deployment.yaml");
    assert!(
        content.contains("replicas: 1"),
        "deployment.yaml must declare replicas: 1 (single-writer DB)"
    );
}

#[test]
fn deployment_uses_recreate_strategy() {
    // Default RollingUpdate would briefly spin up a second pod that
    // cannot attach the RWO PVC. Recreate stops the old pod before
    // starting the new one, releasing the volume cleanly.
    let content = read_manifest("deployment.yaml");
    let strategy = section_after(&content, "strategy:");
    assert!(
        strategy.contains("type: Recreate"),
        "deployment.yaml must use strategy.type: Recreate, got:\n{strategy}"
    );
}

// ------------------------------------------------------------------
// Service manifest
// ------------------------------------------------------------------

#[test]
fn service_declares_v1_service() {
    let content = read_manifest("service.yaml");
    assert!(
        content.contains("apiVersion: v1"),
        "service.yaml must use apiVersion v1"
    );
    assert!(
        content.contains("kind: Service"),
        "service.yaml must have kind: Service"
    );
}

#[test]
fn service_targets_port_8080() {
    let content = read_manifest("service.yaml");
    assert!(
        content.contains("port: 8080"),
        "service.yaml must expose port 8080"
    );
    assert!(
        content.contains("targetPort: 8080"),
        "service.yaml must target containerPort 8080"
    );
}

#[test]
fn service_selector_matches_deployment_labels() {
    // The Service must select the Deployment's pods. Both manifests
    // share `app.kubernetes.io/name: drevo` as the selector / pod
    // label so a relabelling in one file is caught here.
    let svc = read_manifest("service.yaml");
    let dep = read_manifest("deployment.yaml");
    let label = "app.kubernetes.io/name: drevo";
    assert!(
        svc.contains(label),
        "service.yaml must select pods with label `{label}`"
    );
    assert!(
        dep.contains(label),
        "deployment.yaml must label pods with `{label}`"
    );
}

// ------------------------------------------------------------------
// PVC manifest
// ------------------------------------------------------------------

#[test]
fn pvc_declares_v1_persistentvolumeclaim() {
    let content = read_manifest("pvc.yaml");
    assert!(
        content.contains("apiVersion: v1"),
        "pvc.yaml must use apiVersion v1"
    );
    assert!(
        content.contains("kind: PersistentVolumeClaim"),
        "pvc.yaml must have kind: PersistentVolumeClaim"
    );
}

#[test]
fn pvc_is_named_drevo_data() {
    let content = read_manifest("pvc.yaml");
    assert!(
        content.contains("name: drevo-data"),
        "pvc.yaml must name the claim `drevo-data` (referenced from deployment.yaml)"
    );
}

#[test]
fn pvc_requests_readwriteonce_storage() {
    let content = read_manifest("pvc.yaml");
    assert!(
        content.contains("ReadWriteOnce"),
        "pvc.yaml must request ReadWriteOnce access mode (single-writer DB)"
    );
    assert!(
        content.contains("storage:"),
        "pvc.yaml must declare a storage request"
    );
}

// ------------------------------------------------------------------
// Kustomize / glue
// ------------------------------------------------------------------

#[test]
fn kustomization_bundles_all_three_manifests() {
    // A kustomization.yaml lets operators `kubectl apply -k k8s/` in
    // one command, matching the docker-compose ergonomics. The file
    // is short — five lines plus three resources — so the cost of
    // including it is trivial vs the operational gain.
    let content = read_manifest("kustomization.yaml");
    for resource in ["deployment.yaml", "service.yaml", "pvc.yaml"] {
        assert!(
            content.contains(resource),
            "kustomization.yaml must list {resource} under resources"
        );
    }
}

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

/// Return the contiguous block of text that begins at the line where
/// `marker` first appears (inclusive) and ends at the next line whose
/// indent is ≤ the marker line's indent. Crude but enough to scope
/// `livenessProbe:` / `readinessProbe:` / `strategy:` sub-trees for the
/// assertions above without depending on a YAML parser.
fn section_after(content: &str, marker: &str) -> String {
    let mut out = String::new();
    let mut found = false;
    let mut marker_indent = 0usize;
    for line in content.lines() {
        if !found {
            if let Some(idx) = line.find(marker) {
                if line[..idx].trim().is_empty() {
                    found = true;
                    marker_indent = idx;
                    out.push_str(line);
                    out.push('\n');
                }
            }
            continue;
        }
        // Stop when we leave the sub-tree (sibling or higher).
        if line.trim().is_empty() {
            out.push('\n');
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent <= marker_indent {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}
