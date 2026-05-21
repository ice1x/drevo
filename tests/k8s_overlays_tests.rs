//! Kustomize overlay structure and convention tests.
//!
//! Phase 8 task 00050: verify the overlays under `k8s/overlays/<env>/`
//! wrap the base manifests (`k8s/deployment.yaml`, `service.yaml`,
//! `pvc.yaml`, `kustomization.yaml` — locked in by task 00049's
//! `tests/k8s_manifests_tests.rs`) with environment-specific patches
//! that keep the single-writer redb topology intact.
//!
//! The tests parse each overlay manifest as text — no `kubectl`, no
//! `kustomize` binary, no cluster required. The CI K8s job complements
//! this suite by running `kubectl kustomize k8s/overlays/<env>/` for
//! each overlay, which catches structural breakage (broken `../../`
//! base reference, unresolved patch target, kustomize-syntax slip)
//! that pure text matching cannot.
//!
//! Mirrors `tests/k8s_manifests_tests.rs` (task 00049) and
//! `tests/docker_compose_tests.rs` (task 00047).

use std::fs;
use std::path::{Path, PathBuf};

fn k8s_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("k8s")
}

fn overlay_dir(env_name: &str) -> PathBuf {
    k8s_dir().join("overlays").join(env_name)
}

fn read_overlay_file(env_name: &str, name: &str) -> String {
    let path = overlay_dir(env_name).join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read overlay manifest {}: {}", path.display(), e))
}

const ENVS: &[&str] = &["dev", "prod"];

// ------------------------------------------------------------------
// Layout: required files exist
// ------------------------------------------------------------------

#[test]
fn overlays_directory_exists() {
    assert!(
        k8s_dir().join("overlays").is_dir(),
        "k8s/overlays/ directory must exist"
    );
}

#[test]
fn every_env_overlay_has_a_kustomization() {
    for env in ENVS {
        let path = overlay_dir(env).join("kustomization.yaml");
        assert!(
            path.is_file(),
            "k8s/overlays/{env}/kustomization.yaml must exist"
        );
    }
}

#[test]
fn every_env_overlay_has_a_deployment_patch() {
    for env in ENVS {
        let path = overlay_dir(env).join("deployment-patch.yaml");
        assert!(
            path.is_file(),
            "k8s/overlays/{env}/deployment-patch.yaml must exist"
        );
    }
}

#[test]
fn every_env_overlay_has_a_pvc_patch() {
    for env in ENVS {
        let path = overlay_dir(env).join("pvc-patch.yaml");
        assert!(
            path.is_file(),
            "k8s/overlays/{env}/pvc-patch.yaml must exist"
        );
    }
}

// ------------------------------------------------------------------
// Overlay kustomization.yaml — base reference, namespace, image tag
// ------------------------------------------------------------------

#[test]
fn every_overlay_kustomization_uses_the_kustomize_v1beta1_api() {
    for env in ENVS {
        let content = read_overlay_file(env, "kustomization.yaml");
        assert!(
            content.contains("apiVersion: kustomize.config.k8s.io/v1beta1"),
            "k8s/overlays/{env}/kustomization.yaml must declare apiVersion kustomize.config.k8s.io/v1beta1"
        );
        assert!(
            content.contains("kind: Kustomization"),
            "k8s/overlays/{env}/kustomization.yaml must have kind: Kustomization"
        );
    }
}

#[test]
fn every_overlay_references_the_base_via_relative_path() {
    // Overlays live at `k8s/overlays/<env>/` so the relative path to
    // the base `k8s/kustomization.yaml` is `../../`. Kustomize accepts
    // a path-to-directory reference; the base name is implicit.
    for env in ENVS {
        let content = read_overlay_file(env, "kustomization.yaml");
        assert!(
            content.contains("../../"),
            "k8s/overlays/{env}/kustomization.yaml must reference the base with ../../"
        );
    }
}

#[test]
fn every_overlay_pins_the_namespace_to_a_distinct_value() {
    // Kustomize's top-level `namespace:` field stamps every resource
    // in the build output. Each environment gets its own namespace so
    // a `kubectl apply -k overlays/prod/` cannot collide with
    // `kubectl apply -k overlays/dev/` on the same cluster.
    let mut seen = Vec::new();
    for env in ENVS {
        let content = read_overlay_file(env, "kustomization.yaml");
        let ns_line = content
            .lines()
            .find(|l| l.trim_start().starts_with("namespace:"))
            .unwrap_or_else(|| {
                panic!("k8s/overlays/{env}/kustomization.yaml must declare `namespace:`")
            });
        let ns = ns_line
            .split_once("namespace:")
            .map(|(_, rest)| rest.trim().trim_matches('"').to_string())
            .unwrap_or_default();
        assert!(
            !ns.is_empty(),
            "k8s/overlays/{env}/kustomization.yaml namespace must not be empty (was: {ns_line:?})"
        );
        assert!(
            ns.starts_with("drevo-"),
            "k8s/overlays/{env}/kustomization.yaml namespace must start with `drevo-` (got {ns:?})"
        );
        assert!(
            !seen.contains(&ns),
            "namespace {ns:?} reused across overlays (already in {seen:?})"
        );
        seen.push(ns);
    }
}

#[test]
fn every_overlay_pins_the_image_tag_via_images_block() {
    // Kustomize's `images:` field rewrites every container that uses
    // the named image. Each overlay must rewrite the base image
    // (`ghcr.io/ice1x/drevo`) so `kubectl kustomize overlays/<env>/`
    // emits an env-specific tag rather than the base `v0.1.0`.
    for env in ENVS {
        let content = read_overlay_file(env, "kustomization.yaml");
        assert!(
            content.contains("images:"),
            "k8s/overlays/{env}/kustomization.yaml must declare an `images:` block"
        );
        assert!(
            content.contains("ghcr.io/ice1x/drevo"),
            "k8s/overlays/{env}/kustomization.yaml `images:` block must target ghcr.io/ice1x/drevo"
        );
        assert!(
            content.contains("newTag:"),
            "k8s/overlays/{env}/kustomization.yaml must override the image tag via `newTag:`"
        );
        // No bare :latest — the base manifest forbids it (task 00049)
        // and overlays must hold the same line.
        let images_section = section_after(&content, "images:");
        assert!(
            !images_section.contains("newTag: latest"),
            "k8s/overlays/{env}/kustomization.yaml must not pin the image to :latest, got:\n{images_section}"
        );
    }
}

#[test]
fn every_overlay_lists_both_patches() {
    // The patches block must list the deployment + pvc patches so a
    // kustomize build picks them up. We accept either of the modern
    // `patches:` or the legacy `patchesStrategicMerge:` field names —
    // both work and either is fine in v1beta1.
    for env in ENVS {
        let content = read_overlay_file(env, "kustomization.yaml");
        assert!(
            content.contains("patches:") || content.contains("patchesStrategicMerge:"),
            "k8s/overlays/{env}/kustomization.yaml must declare a `patches:` block"
        );
        for patch_file in ["deployment-patch.yaml", "pvc-patch.yaml"] {
            assert!(
                content.contains(patch_file),
                "k8s/overlays/{env}/kustomization.yaml must reference {patch_file} in its patches block"
            );
        }
    }
}

// ------------------------------------------------------------------
// Deployment patch — keeps single-writer invariants, adjusts resources
// ------------------------------------------------------------------

#[test]
fn every_deployment_patch_targets_the_drevo_deployment() {
    for env in ENVS {
        let content = read_overlay_file(env, "deployment-patch.yaml");
        assert!(
            content.contains("apiVersion: apps/v1"),
            "k8s/overlays/{env}/deployment-patch.yaml must declare apiVersion apps/v1"
        );
        assert!(
            content.contains("kind: Deployment"),
            "k8s/overlays/{env}/deployment-patch.yaml must declare kind: Deployment"
        );
        assert!(
            content.contains("name: drevo"),
            "k8s/overlays/{env}/deployment-patch.yaml must target name: drevo"
        );
    }
}

#[test]
fn no_deployment_patch_bumps_replicas_above_one() {
    // The single-writer redb invariant lives in the base; an overlay
    // must NOT silently raise replicas to >1 because that corrupts the
    // database file. MVCC arrives in Phase 13 / task 00080.
    for env in ENVS {
        let content = read_overlay_file(env, "deployment-patch.yaml");
        for forbidden in ["replicas: 2", "replicas: 3", "replicas: 4", "replicas: 5"] {
            assert!(
                !content.contains(forbidden),
                "k8s/overlays/{env}/deployment-patch.yaml must not set `{forbidden}` (single-writer redb invariant)"
            );
        }
    }
}

#[test]
fn no_deployment_patch_relaxes_the_runasnonroot_security_context() {
    // A future overlay must not silently re-grant root by overriding
    // securityContext.runAsNonRoot to false. The base sets it to
    // `true`; we forbid patches that flip it.
    for env in ENVS {
        let content = read_overlay_file(env, "deployment-patch.yaml");
        assert!(
            !content.contains("runAsNonRoot: false"),
            "k8s/overlays/{env}/deployment-patch.yaml must not set runAsNonRoot: false"
        );
    }
}

#[test]
fn every_deployment_patch_adjusts_container_resources() {
    // The whole point of having per-env overlays is letting dev run
    // cheaper than prod. Each patch must touch the `resources:` block
    // so the kustomize build emits an env-specific request/limit pair.
    for env in ENVS {
        let content = read_overlay_file(env, "deployment-patch.yaml");
        assert!(
            content.contains("resources:"),
            "k8s/overlays/{env}/deployment-patch.yaml must override container resources"
        );
        assert!(
            content.contains("requests:"),
            "k8s/overlays/{env}/deployment-patch.yaml must specify resource requests"
        );
        assert!(
            content.contains("limits:"),
            "k8s/overlays/{env}/deployment-patch.yaml must specify resource limits"
        );
    }
}

#[test]
fn prod_requests_more_cpu_and_memory_than_dev() {
    // Sanity check: production should not be sized below development.
    // Comparing the requests block is enough — limits track requests
    // in practice and any future regression flips the same string.
    let dev = read_overlay_file("dev", "deployment-patch.yaml");
    let prod = read_overlay_file("prod", "deployment-patch.yaml");

    let dev_cpu = parse_request_value(&dev, "cpu:");
    let prod_cpu = parse_request_value(&prod, "cpu:");
    let dev_mem = parse_request_value(&dev, "memory:");
    let prod_mem = parse_request_value(&prod, "memory:");

    assert!(
        prod_cpu > dev_cpu,
        "prod cpu request ({prod_cpu}m) must exceed dev ({dev_cpu}m)"
    );
    assert!(
        prod_mem > dev_mem,
        "prod memory request ({prod_mem}Mi) must exceed dev ({dev_mem}Mi)"
    );
}

// ------------------------------------------------------------------
// PVC patch — storage size only; ReadWriteOnce stays locked at the base
// ------------------------------------------------------------------

#[test]
fn every_pvc_patch_targets_the_drevo_data_claim() {
    for env in ENVS {
        let content = read_overlay_file(env, "pvc-patch.yaml");
        assert!(
            content.contains("apiVersion: v1"),
            "k8s/overlays/{env}/pvc-patch.yaml must declare apiVersion v1"
        );
        assert!(
            content.contains("kind: PersistentVolumeClaim"),
            "k8s/overlays/{env}/pvc-patch.yaml must declare kind: PersistentVolumeClaim"
        );
        assert!(
            content.contains("name: drevo-data"),
            "k8s/overlays/{env}/pvc-patch.yaml must target name: drevo-data"
        );
    }
}

#[test]
fn every_pvc_patch_sets_an_explicit_storage_size() {
    for env in ENVS {
        let content = read_overlay_file(env, "pvc-patch.yaml");
        assert!(
            content.contains("storage:"),
            "k8s/overlays/{env}/pvc-patch.yaml must declare a `storage:` request"
        );
    }
}

#[test]
fn no_pvc_patch_relaxes_the_readwriteonce_access_mode() {
    // ReadWriteOnce is the single-writer invariant at the storage
    // layer. Allowing ReadWriteMany would imply two pods can write
    // the redb file at once. Comments that *mention* the forbidden
    // mode (to explain why it is forbidden) are allowed; only
    // non-comment YAML payload is checked.
    for env in ENVS {
        let content = read_overlay_file(env, "pvc-patch.yaml");
        let payload: String = content
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !payload.contains("ReadWriteMany"),
            "k8s/overlays/{env}/pvc-patch.yaml must not relax accessMode to ReadWriteMany"
        );
    }
}

#[test]
fn prod_pvc_requests_more_storage_than_dev() {
    let dev = parse_storage_gi(&read_overlay_file("dev", "pvc-patch.yaml"));
    let prod = parse_storage_gi(&read_overlay_file("prod", "pvc-patch.yaml"));
    assert!(
        prod > dev,
        "prod PVC ({prod}Gi) must request more storage than dev ({dev}Gi)"
    );
}

// ------------------------------------------------------------------
// k8s/README.md documents the overlays
// ------------------------------------------------------------------

#[test]
fn k8s_readme_documents_overlays_section() {
    let readme = fs::read_to_string(k8s_dir().join("README.md"))
        .expect("k8s/README.md must exist (task 00049)");
    let lower = readme.to_lowercase();
    assert!(
        lower.contains("overlay"),
        "k8s/README.md must document the kustomize overlays (task 00050)"
    );
    for env in ENVS {
        assert!(
            readme.contains(&format!("overlays/{env}")),
            "k8s/README.md must reference k8s/overlays/{env} in its overlay docs"
        );
    }
}

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

/// Parse the first `cpu:` / `memory:` line under the `requests:` block.
/// Returns the numeric value normalised to milli-cpu (e.g. "200m" → 200,
/// "1" → 1000) or mebibytes (e.g. "256Mi" → 256, "1Gi" → 1024, "512M" → 512).
/// Crude but enough to lock the ordering invariant between dev and prod
/// without depending on a YAML parser.
fn parse_request_value(content: &str, key: &str) -> u64 {
    let requests = section_after(content, "requests:");
    let line = requests
        .lines()
        .find(|l| l.trim_start().starts_with(key))
        .unwrap_or_else(|| panic!("no `{key}` line under `requests:` in:\n{requests}"));
    let raw = line
        .split_once(key)
        .map(|(_, rest)| rest.trim().trim_matches('"').to_string())
        .unwrap_or_default();
    if let Some(num) = raw.strip_suffix("m") {
        num.parse::<u64>()
            .unwrap_or_else(|e| panic!("failed to parse milli-cpu value {raw:?}: {e}"))
    } else if let Some(num) = raw.strip_suffix("Mi") {
        num.parse::<u64>()
            .unwrap_or_else(|e| panic!("failed to parse Mi value {raw:?}: {e}"))
    } else if let Some(num) = raw.strip_suffix("Gi") {
        let n: u64 = num
            .parse()
            .unwrap_or_else(|e| panic!("failed to parse Gi value {raw:?}: {e}"));
        n * 1024
    } else if let Some(num) = raw.strip_suffix("M") {
        num.parse::<u64>()
            .unwrap_or_else(|e| panic!("failed to parse M value {raw:?}: {e}"))
    } else if let Some(num) = raw.strip_suffix("G") {
        let n: u64 = num
            .parse()
            .unwrap_or_else(|e| panic!("failed to parse G value {raw:?}: {e}"));
        n * 1000
    } else {
        // Plain integer cpu (e.g. "1" → 1 core → 1000m).
        let n: u64 = raw
            .parse()
            .unwrap_or_else(|e| panic!("failed to parse plain value {raw:?}: {e}"));
        n * 1000
    }
}

/// Parse the storage request from a PVC patch, normalising the unit to
/// gibibytes. Accepts `1Gi`, `20Gi`, `2048Mi`, etc.
fn parse_storage_gi(content: &str) -> u64 {
    let line = content
        .lines()
        .find(|l| l.trim_start().starts_with("storage:"))
        .unwrap_or_else(|| panic!("no `storage:` line in PVC patch:\n{content}"));
    let raw = line
        .split_once("storage:")
        .map(|(_, rest)| rest.trim().trim_matches('"').to_string())
        .unwrap_or_default();
    if let Some(num) = raw.strip_suffix("Gi") {
        num.parse::<u64>()
            .unwrap_or_else(|e| panic!("failed to parse Gi value {raw:?}: {e}"))
    } else if let Some(num) = raw.strip_suffix("Mi") {
        let n: u64 = num
            .parse()
            .unwrap_or_else(|e| panic!("failed to parse Mi value {raw:?}: {e}"));
        // Round down to whole Gi for comparison purposes.
        n / 1024
    } else {
        panic!("unsupported storage unit in {raw:?}")
    }
}

/// Return the contiguous block of text that begins at the line where
/// `marker` first appears (inclusive) and ends at the next line whose
/// indent is ≤ the marker line's indent. Crude but enough to scope
/// `requests:` / `images:` sub-trees without depending on a YAML parser.
/// Same shape as the helper in `tests/k8s_manifests_tests.rs`.
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
