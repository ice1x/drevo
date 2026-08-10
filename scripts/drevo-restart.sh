#!/usr/bin/env bash
#
# drevo-restart.sh — (re)start the local drevo container in ONE command.
#
# A bare-`docker run` alternative to docker-compose.yml for single-host setups
# that don't use Compose. Force-recreates the container from a LOCAL image (no
# implicit `docker pull`, so a freshly built image is never clobbered by an
# older registry one), on a host-mounted data directory so the redb file lives
# on the host and survives the container swap. The container carries
# `--restart unless-stopped`, so a crash / OOM-kill / Docker or host reboot
# self-heals; pair it with scripts/drevo-watchdog.sh to also survive an
# accidental `docker rm -f` (which a restart policy cannot cover).
#
# Usage:
#   scripts/drevo-restart.sh
#   DREVO_IMAGE=ghcr.io/ice1x/drevo:0.1.0 scripts/drevo-restart.sh
#   DREVO_BOLT_PORT_HOST=7688 scripts/drevo-restart.sh   # avoid a local 7687 clash
#
# Env (all optional):
#   DREVO_NAME            container name                 (default: drevo)
#   DREVO_IMAGE           image ref                      (default: ghcr.io/ice1x/drevo:latest)
#   DREVO_DATA_DIR        host data dir bind-mounted /data (default: ./data)
#   DREVO_HTTP_PORT       host port -> container 8080     (default: 8080)
#   DREVO_BOLT_PORT_HOST  host port -> container 7687     (default: 7687)
#   DREVO_ENV_FILE        extra env file to source        (default: ~/.drevo.env if present)
set -euo pipefail

# Optionally load persistent deploy config (image tag, embeddings proxy vars,
# API key) from one file so a normal restart stays a single command. Anything
# already exported in the shell still wins over the file.
ENV_FILE="${DREVO_ENV_FILE:-$HOME/.drevo.env}"
if [ -f "$ENV_FILE" ]; then
  set -a
  # shellcheck disable=SC1090
  . "$ENV_FILE"
  set +a
fi

NAME="${DREVO_NAME:-drevo}"
IMAGE="${DREVO_IMAGE:-ghcr.io/ice1x/drevo:latest}"
DATA_DIR="${DREVO_DATA_DIR:-./data}"
HTTP_PORT="${DREVO_HTTP_PORT:-8080}"        # host -> container 8080 (HTTP + Web UI)
BOLT_PORT="${DREVO_BOLT_PORT_HOST:-7687}"   # host -> container 7687 (Bolt)

command -v "${DREVO_DOCKER:-docker}" >/dev/null 2>&1 || { echo "docker not found on PATH" >&2; exit 1; }
DOCKER="${DREVO_DOCKER:-docker}"
"$DOCKER" info >/dev/null 2>&1 || { echo "docker daemon unreachable — is Docker running?" >&2; exit 1; }
mkdir -p "$DATA_DIR"
[ -f "${DATA_DIR}/drevo.redb" ] || echo "note: ${DATA_DIR}/drevo.redb not found yet — a fresh DB will be created."

# Forward the optional embeddings-proxy config (issue #217) only when present,
# so the endpoint stays a 503 no-op otherwise. Requires an image built with the
# `embeddings-proxy` feature.
EMB_ENV=()
for _v in DREVO_EMBEDDINGS_UPSTREAM DREVO_EMBEDDINGS_API_KEY DREVO_EMBEDDINGS_MODEL; do
  if [ -n "${!_v:-}" ]; then
    EMB_ENV+=(-e "${_v}=${!_v}")
  fi
done

echo "Recreating '${NAME}' from ${IMAGE}  (HTTP :${HTTP_PORT}, Bolt :${BOLT_PORT}, data ${DATA_DIR}) …"
"$DOCKER" rm -f "${NAME}" >/dev/null 2>&1 || true
# --restart unless-stopped: the daemon relaunches the container after a crash,
# OOM-kill, or Docker/host reboot. It does NOT fight an intentional stop, and it
# cannot resurrect a *removed* container — that is what the watchdog is for.
"$DOCKER" run -d --restart unless-stopped --name "${NAME}" \
  -p "${HTTP_PORT}:8080" -p "${BOLT_PORT}:7687" \
  --user "$(id -u):$(id -g)" \
  -e DREVO_HOST=0.0.0.0 -e DREVO_PORT=8080 -e DREVO_BOLT_PORT=7687 -e DREVO_DATA_DIR=/data \
  ${EMB_ENV[@]+"${EMB_ENV[@]}"} \
  -v "${DATA_DIR}:/data" \
  "${IMAGE}" >/dev/null

healthy=0
for _ in $(seq 1 30); do
  if curl -fsS "http://localhost:${HTTP_PORT}/health" >/dev/null 2>&1; then healthy=1; break; fi
  sleep 1
done
if [ "$healthy" -ne 1 ]; then
  echo "drevo did not become healthy within 30s — check: ${DOCKER} logs ${NAME}" >&2
  exit 1
fi

echo "drevo up:  HTTP http://localhost:${HTTP_PORT}   Bolt bolt://localhost:${BOLT_PORT}   data ${DATA_DIR}"
