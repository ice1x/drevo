#!/usr/bin/env bash
#
# release.sh — cut a new drevo container-image version.
#
# The image itself is built and published by .github/workflows/docker-publish.yml
# ("Docker Publish"): pushing a `vX.Y.Z` git tag makes it publish
# `ghcr.io/ice1x/drevo:X.Y.Z`, `:X.Y`, and `:sha-<short>`. This script computes
# the next version (bumping the MINOR component by default), creates the
# annotated tag, and pushes it — so "new image version" is one command.
#
# Usage:
#   scripts/release.sh                 # bump minor, tag, push  (asks to confirm)
#   scripts/release.sh minor|patch|major
#   scripts/release.sh --yes minor     # skip the confirmation prompt
#   scripts/release.sh next [minor|patch|major]   # DRY RUN: print next version
#   scripts/release.sh next minor --from 0.1.0    # DRY RUN from an explicit base
#
# The current version is read from the latest `vX.Y.Z` git tag, falling back to
# the `version = "…"` in Cargo.toml when no tags exist yet.
set -euo pipefail

repo_root() { git rev-parse --show-toplevel; }

# Latest semver from git tags (vX.Y.Z), else the Cargo.toml version, else 0.0.0.
current_version() {
  local tag
  tag=$(git tag --list 'v[0-9]*.[0-9]*.[0-9]*' --sort=-v:refname | head -n1 || true)
  if [ -n "$tag" ]; then
    printf '%s' "${tag#v}"
    return
  fi
  local cargo
  cargo=$(awk -F'"' '/^version[[:space:]]*=/ {print $2; exit}' "$(repo_root)/Cargo.toml" || true)
  printf '%s' "${cargo:-0.0.0}"
}

# bump_version <current X.Y.Z> <minor|patch|major> -> prints next X.Y.Z
bump_version() {
  local cur="$1" part="${2:-minor}"
  local major minor patch
  IFS='.' read -r major minor patch <<<"$cur"
  # Guard against a malformed base so we never emit garbage tags.
  case "$major.$minor.$patch" in
    [0-9]*.[0-9]*.[0-9]*) : ;;
    *) echo "release.sh: cannot parse version '$cur'" >&2; return 1 ;;
  esac
  case "$part" in
    major) major=$((major + 1)); minor=0; patch=0 ;;
    minor) minor=$((minor + 1)); patch=0 ;;
    patch) patch=$((patch + 1)) ;;
    *) echo "release.sh: unknown bump '$part' (want major|minor|patch)" >&2; return 1 ;;
  esac
  printf '%s.%s.%s' "$major" "$minor" "$patch"
}

# ── DRY RUN: `next [part] [--from X.Y.Z]` prints the next version and exits ──
if [ "${1:-}" = "next" ]; then
  shift
  part="minor"; from=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --from) from="${2:-}"; shift 2 ;;
      minor|patch|major) part="$1"; shift ;;
      *) echo "release.sh next: unexpected arg '$1'" >&2; exit 2 ;;
    esac
  done
  base="${from:-$(current_version)}"
  bump_version "$base" "$part"
  echo
  exit 0
fi

# ── Real release: parse flags ──
assume_yes=0
part="minor"
while [ $# -gt 0 ]; do
  case "$1" in
    -y|--yes) assume_yes=1; shift ;;
    minor|patch|major) part="$1"; shift ;;
    -h|--help) sed -n '3,20p' "$0"; exit 0 ;;
    *) echo "release.sh: unexpected arg '$1' (see --help)" >&2; exit 2 ;;
  esac
done

cd "$(repo_root)"

# Safety rails: release from a clean main only.
branch=$(git symbolic-ref --quiet --short HEAD || echo DETACHED)
if [ "$branch" != "main" ]; then
  echo "release.sh: refusing to release from '$branch' — switch to main first." >&2
  exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
  echo "release.sh: working tree is dirty — commit or stash before releasing." >&2
  exit 1
fi

cur=$(current_version)
next=$(bump_version "$cur" "$part")
tag="v$next"

if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  echo "release.sh: tag $tag already exists." >&2
  exit 1
fi

echo "Current version : $cur"
echo "Next version    : $next   (bump: $part)"
echo "Will create+push: $tag  → triggers Docker Publish → ghcr.io/ice1x/drevo:$next"
if [ "$assume_yes" -ne 1 ]; then
  printf 'Proceed? [y/N] '
  read -r reply
  case "$reply" in
    y|Y|yes|YES) : ;;
    *) echo "Aborted."; exit 0 ;;
  esac
fi

git tag -a "$tag" -m "drevo $next"
git push origin "$tag"
echo "Pushed $tag. Watch the image build at:"
echo "  https://github.com/ice1x/drevo/actions/workflows/docker-publish.yml"
