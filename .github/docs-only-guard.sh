#!/usr/bin/env bash
# Docs-only gate for ci-skip.yml.
#
# ci-skip.yml is the docs-only pass-through that emits the required
# `CI / <job>` status checks (Test, Check, Clippy, …) without running a
# real Rust build, so a documentation-only PR can merge. Its `paths:`
# trigger fires when AT LEAST ONE changed file is a doc path — but GitHub
# path filters cannot express "ALL files are docs". So a MIXED commit
# (code + docs) triggers BOTH ci.yml (real) AND ci-skip.yml. If the skip
# job blindly echoes success, it can post a false-green `CI / Test` that
# masks the real ci.yml Test failing or being cancelled.
#
# Regression that motivated this (2026-06-29): a mixed commit's real Test
# hit the 4h timeout (deps/ bloat), yet the PR showed green because the
# skip workflow's no-op `Test` reported success.
#
# This script makes the skip jobs FAIL-CLOSED: they pass ONLY when the
# change is genuinely docs-only; otherwise they exit non-zero so the real
# ci.yml — not this no-op — is the authority for the required check.
#
# Env: BASE_SHA / HEAD_SHA (the commit range to diff). The repo must be
# checked out with full history (fetch-depth: 0) so BASE_SHA resolves.
set -euo pipefail

base="${BASE_SHA:-}"
head="${HEAD_SHA:-}"

# Fail closed if we cannot prove the range (e.g. a new-branch push whose
# `before` is all-zeros): defer to the real CI rather than risk a false green.
if [ -z "$base" ] || [ -z "$head" ] || printf '%s' "$base" | grep -qE '^0+$'; then
  echo "::error::Cannot resolve a base..head range (base='$base' head='$head') — deferring to the real ci.yml gate."
  exit 1
fi

changed="$(git diff --name-only "$base" "$head")"
echo "changed files ($base..$head):"
echo "$changed"

# Docs globs MUST mirror ci-skip.yml's `paths:` and ci.yml's `paths-ignore:`
# (**/*.md, audit/**, memory/**, LICENSE, .gitignore). Any path outside this
# set makes the change non-docs-only.
nondoc="$(printf '%s\n' "$changed" \
  | grep -vE '(\.md$|^audit/|^memory/|^LICENSE$|^\.gitignore$)' \
  | grep -v '^$' || true)"

if [ -n "$nondoc" ]; then
  echo "::error::Not a docs-only change — the real CI (ci.yml) must gate this. Failing the skip pass-through to avoid a false-green required check."
  echo "non-docs files:"
  echo "$nondoc"
  exit 1
fi

echo "docs-only change confirmed — skip pass-through OK."
