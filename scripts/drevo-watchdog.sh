#!/usr/bin/env bash
#
# drevo-watchdog.sh — keep the local drevo container alive, even after removal.
#
# Docker's own `restart: unless-stopped` (see docker-compose.yml) already
# relaunches the container after a crash, an OOM-kill, or a Docker/host reboot.
# What it CANNOT cover is a container that was *removed*: an accidental
# `docker rm -f drevo` — e.g. another project's teardown script hitting the same
# name — deletes the container object outright, so the restart policy has
# nothing left to act on and the API/Bolt ports simply go dark.
#
# This watchdog closes that gap. Run it periodically (launchd on macOS, a
# systemd timer or cron on Linux — templates in scripts/watchdog/) and it
# recreates the container whenever it is missing or not running.
#
# It is intentionally conservative: it only ever (re)creates when the container
# is absent or not `running`, it acts solely on the single container named
# $DREVO_NAME, and it no-ops while a disable sentinel exists so you can stop the
# container on purpose without a fight.
#
# Usage:
#   scripts/drevo-watchdog.sh                 # Compose path (repo default)
#   DREVO_UP_CMD='~/drevo-restart.sh' scripts/drevo-watchdog.sh   # bare docker run
#
# Env:
#   DREVO_NAME                  container name to guard        (default: drevo)
#   DREVO_DOCKER                docker executable              (default: docker)
#   DREVO_UP_CMD                command to (re)create it; when unset, falls back
#                               to `docker compose up -d` run from the repo root
#   DREVO_WATCHDOG_DISABLE_FILE sentinel; while it exists the watchdog no-ops
#                               (default: ~/.drevo-watchdog.disabled)
set -euo pipefail

NAME="${DREVO_NAME:-drevo}"
DOCKER="${DREVO_DOCKER:-docker}"
UP_CMD="${DREVO_UP_CMD:-}"
DISABLE="${DREVO_WATCHDOG_DISABLE_FILE:-$HOME/.drevo-watchdog.disabled}"

# Opt out without a fight: `touch "$DISABLE"` before an intentional
# `docker stop` / `compose down`, delete it to resume self-healing.
if [ -e "$DISABLE" ]; then
  echo "drevo-watchdog: disabled ($DISABLE present) — no action"
  exit 0
fi

# `inspect` exits non-zero when the container does not exist → treat as "absent".
status="$("$DOCKER" inspect -f '{{.State.Status}}' "$NAME" 2>/dev/null || echo absent)"
if [ "$status" = "running" ]; then
  exit 0
fi

echo "drevo-watchdog: container '$NAME' is '$status' — (re)creating"
if [ -n "$UP_CMD" ]; then
  # A bare-`docker run` deployment supplies its own (re)create command.
  eval "$UP_CMD"
else
  # Repo default: Compose from the repository root (one level up from scripts/),
  # which honours the same DREVO_* env the compose file reads. `up -d` recreates
  # the container from the existing image; it is idempotent when already up.
  repo_root="$(cd "$(dirname "$0")/.." && pwd)"
  ( cd "$repo_root" && "$DOCKER" compose up -d )
fi
