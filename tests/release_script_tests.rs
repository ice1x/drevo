//! Tests for the release tooling: `scripts/release.sh` version math and the
//! Makefile wiring that fronts it.
//!
//! Only the DRY-RUN path (`scripts/release.sh next … --from <base>`) is
//! exercised — it is pure arithmetic with no git side effects, so the suite
//! never creates or pushes a tag.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo_root().join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// Run `scripts/release.sh next <part> --from <base>` and return trimmed stdout.
fn next_version(part: &str, from: &str) -> String {
    let out = Command::new("bash")
        .arg(repo_root().join("scripts/release.sh"))
        .args(["next", part, "--from", from])
        .current_dir(repo_root())
        .output()
        .expect("run release.sh");
    assert!(
        out.status.success(),
        "release.sh next {part} --from {from} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn release_script_exists_and_is_executable() {
    let path = repo_root().join("scripts/release.sh");
    assert!(path.exists(), "scripts/release.sh must exist");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "scripts/release.sh must be executable");
    }
}

#[test]
fn bump_minor_resets_patch() {
    assert_eq!(next_version("minor", "0.1.0"), "0.2.0");
    assert_eq!(next_version("minor", "1.4.9"), "1.5.0");
}

#[test]
fn bump_patch_increments_patch_only() {
    assert_eq!(next_version("patch", "0.1.0"), "0.1.1");
    assert_eq!(next_version("patch", "2.3.4"), "2.3.5");
}

#[test]
fn bump_major_resets_minor_and_patch() {
    assert_eq!(next_version("major", "0.1.9"), "1.0.0");
    assert_eq!(next_version("major", "3.7.2"), "4.0.0");
}

#[test]
fn makefile_wires_image_and_release_targets() {
    let makefile = read("Makefile");
    for target in ["image:", "release:", "release-patch:", "release-major:"] {
        assert!(
            makefile.contains(target),
            "Makefile must define a `{target}` target"
        );
    }
    // `release` must delegate to the script, not hand-roll `git tag`.
    assert!(
        makefile.contains("scripts/release.sh"),
        "Makefile release targets must call scripts/release.sh"
    );
}

#[test]
fn release_targets_do_not_push_from_the_makefile() {
    // Guard against a footgun: the tag/push must live behind the script's
    // safety rails (clean tree, on main, confirm), never inline in a target.
    let makefile = read("Makefile");
    assert!(
        !makefile.contains("git push"),
        "the Makefile must not `git push` directly — releasing goes through scripts/release.sh"
    );
    assert!(
        !makefile.contains("docker push"),
        "the Makefile must not `docker push` directly — image release goes through scripts/release.sh"
    );
}

#[test]
fn release_script_supports_one_shot_image_build_and_push() {
    // The `image` subcommand is the missing "one command to ship a deployed
    // image": bump -> docker build -> docker push (to the registry the deploy
    // pulls from) -> git tag. Assert the structure rather than running docker.
    let s = read("scripts/release.sh");
    assert!(
        s.contains(r#""image""#),
        "release.sh must handle an `image` subcommand"
    );
    assert!(
        s.contains("docker build"),
        "release.sh `image` mode must build the container image"
    );
    assert!(
        s.contains("docker push"),
        "release.sh `image` mode must push the container image"
    );
    // Must push to the registry the deploy actually uses (Docker Hub by
    // default), overridable via DREVO_IMAGE — not hard-wired to ghcr.io.
    assert!(
        s.contains("DREVO_IMAGE") && s.contains("ice1x/drevo"),
        "release.sh `image` mode must default to the Docker Hub deploy image, DREVO_IMAGE-overridable"
    );
}

#[test]
fn makefile_wires_release_image_target() {
    let makefile = read("Makefile");
    assert!(
        makefile.contains("release-image:"),
        "Makefile must define a `release-image` target"
    );
    assert!(
        makefile.contains("scripts/release.sh image"),
        "the `release-image` target must delegate to `scripts/release.sh image`"
    );
}
