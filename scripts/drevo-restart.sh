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
#   DREVO_IMAGE=ice1x/drevo:0.1.0 scripts/drevo-restart.sh
#   DREVO_BOLT_PORT_HOST=7688 scripts/drevo-restart.sh   # avoid a local 7687 clash
#
# Env (all optional):
#   DREVO_NAME            container name                 (default: drevo)
#   DREVO_IMAGE           image ref                      (default: ice1x/drevo:latest — Docker Hub)
#   DREVO_DATA_DIR        host data dir bind-mounted /data (default: ./data)
#   DREVO_HTTP_PORT       host port -> container 8080     (default: 8080)
#   DREVO_BOLT_PORT_HOST  host port -> container 7687     (default: 7687)
#   DREVO_ENV_FILE        extra env file to source        (default: ~/.drevo.env if present)
#
# Server settings passed on to the container when set (see FORWARD_VARS):
#   DREVO_EMBEDDINGS_UPSTREAM / _API_KEY / _MODEL     embeddings proxy
#   DREVO_TEXT2CYPHER_UPSTREAM / _MODEL / _API_KEY    text-to-Cypher
#   DREVO_EXTRACT_UPSTREAM / _API_KEY / _MODEL / _TIMEOUT_MS
#                                                     extraction and LLM community summaries
#   DREVO_INGEST_WORKERS / _MAX_ATTEMPTS / _RETRY_BASE_MS
#                                                     ingest jobs
#   DREVO_COMMUNITY_MAX_LLM_SYNC                      LLM summaries per synchronous call
#   DREVO_ENGINE, DREVO_QUERY_TIMEOUT_MS, DREVO_IDLE_SESSION_TIMEOUT_MS, DREVO_AUTH
#   DREVO_READ_ONLY, DREVO_WAL_FORMAT, DREVO_ENCRYPTION_KEY
#                                                     the store
#   DREVO_TX_ISOLATION, DREVO_TX_CONFLICT_HISTORY, DREVO_MAX_TX_WRITES,
#   DREVO_PREPARED_TX_TIMEOUT_SECS, DREVO_PREPARED_TX_WARN_SECS
#                                                     transactions
#   DREVO_MAX_CONNECTIONS, DREVO_HTTP_MAX_IN_FLIGHT, DREVO_HTTP_FORMATS
#                                                     connection and HTTP limits
#   DREVO_SLOW_QUERY_MS, DREVO_AUDIT_LOG, DREVO_AUDIT_QUERY_TEXT
#                                                     slow-query and audit logs
#   DREVO_EXPAND_MAX_HOPS, DREVO_EXPAND_MAX_NODES, DREVO_RAG_TRACE_RETENTION_DAYS
#                                                     graph expansion and context traces
#   DREVO_OIDC_ISSUER / _AUDIENCE / _USERNAME_CLAIM / _ROLES_CLAIM / _ROLE_MAP
#                                                     OIDC sign-in
#
# Not passed on, because the variable alone is not enough — use
# docker-compose.yml or your own `docker run` for these:
#   DREVO_ENCRYPTION_KEY_FILE, DREVO_TLS_CERT, DREVO_TLS_KEY, DREVO_BACKUP_DIR
#                                                     name a path inside the container: need a mount
#   DREVO_GRPC_PORT, DREVO_FLIGHT_PORT                need a published port
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
# Docker Hub is the registry the release flow (`make release-image`) actually
# pushes to and the one that allows anonymous pulls; ghcr.io mirrors tags via CI
# but is not the deploy default.
IMAGE="${DREVO_IMAGE:-ice1x/drevo:latest}"
DATA_DIR="${DREVO_DATA_DIR:-./data}"
HTTP_PORT="${DREVO_HTTP_PORT:-8080}"        # host -> container 8080 (HTTP + Web UI)
BOLT_PORT="${DREVO_BOLT_PORT_HOST:-7687}"   # host -> container 7687 (Bolt)

command -v "${DREVO_DOCKER:-docker}" >/dev/null 2>&1 || { echo "docker not found on PATH" >&2; exit 1; }
DOCKER="${DREVO_DOCKER:-docker}"
"$DOCKER" info >/dev/null 2>&1 || { echo "docker daemon unreachable — is Docker running?" >&2; exit 1; }
mkdir -p "$DATA_DIR"
[ -f "${DATA_DIR}/drevo.redb" ] || echo "note: ${DATA_DIR}/drevo.redb not found yet — a fresh DB will be created."

# Server settings forwarded into the container when set (in the shell or the
# env file): the embeddings proxy (issue #217; needs the `embeddings-proxy`
# feature, a 503 no-op otherwise), text-to-Cypher, the chat model of
# extraction and LLM community summaries (DREVO_EXTRACT_*, each falling back
# to its DREVO_TEXT2CYPHER_* twin), the ingest job workers and retries
# (DREVO_INGEST_*), DREVO_COMMUNITY_MAX_LLM_SYNC, the engine, the statement
# and idle-session timeouts, and DREVO_AUTH — `none` keeps a server without
# users open to clients that send no credentials, such as an MCP server.
# The rest of the list is every other setting the server reads that needs
# nothing but its value (see the header); tests/watchdog_script_tests.rs
# fails when the server gains a setting that is neither here nor excepted
# there with a reason.
FORWARD_VARS="DREVO_EMBEDDINGS_UPSTREAM DREVO_EMBEDDINGS_API_KEY DREVO_EMBEDDINGS_MODEL DREVO_TEXT2CYPHER_UPSTREAM DREVO_TEXT2CYPHER_MODEL DREVO_TEXT2CYPHER_API_KEY DREVO_EXTRACT_UPSTREAM DREVO_EXTRACT_API_KEY DREVO_EXTRACT_MODEL DREVO_EXTRACT_TIMEOUT_MS DREVO_INGEST_WORKERS DREVO_INGEST_MAX_ATTEMPTS DREVO_INGEST_RETRY_BASE_MS DREVO_COMMUNITY_MAX_LLM_SYNC DREVO_ENGINE DREVO_QUERY_TIMEOUT_MS DREVO_IDLE_SESSION_TIMEOUT_MS DREVO_AUTH DREVO_READ_ONLY DREVO_WAL_FORMAT DREVO_ENCRYPTION_KEY DREVO_TX_ISOLATION DREVO_TX_CONFLICT_HISTORY DREVO_MAX_TX_WRITES DREVO_PREPARED_TX_TIMEOUT_SECS DREVO_PREPARED_TX_WARN_SECS DREVO_MAX_CONNECTIONS DREVO_HTTP_MAX_IN_FLIGHT DREVO_HTTP_FORMATS DREVO_SLOW_QUERY_MS DREVO_AUDIT_LOG DREVO_AUDIT_QUERY_TEXT DREVO_EXPAND_MAX_HOPS DREVO_EXPAND_MAX_NODES DREVO_RAG_TRACE_RETENTION_DAYS DREVO_OIDC_ISSUER DREVO_OIDC_AUDIENCE DREVO_OIDC_USERNAME_CLAIM DREVO_OIDC_ROLES_CLAIM DREVO_OIDC_ROLE_MAP"
EMB_ENV=()
for _v in $FORWARD_VARS; do
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
