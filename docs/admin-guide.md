# Admin Guide

Running drevo in production: deployment, configuration, observability, backups, authentication,
and the replication / streaming substrate. For calling the database see the
[SDK Reference](sdk-reference.md).

---

## 1. Binaries

drevo builds one binary (`Cargo.toml` `[[bin]]`):

| Binary | Source | Features | Role |
|--------|--------|----------|------|
| `drevo-server` | [`src/bin/server.rs`](../src/bin/server.rs) | `http`, `redb-backend` | HTTP API + Web UI + Bolt listener. |

```bash
cargo build --release --bin drevo-server --features http,redb-backend
```

`drevo-server` reads its config from the environment, serves on `${DREVO_HOST}:${DREVO_PORT}`,
and shuts down gracefully on `SIGTERM` / `Ctrl-C` — flipping `/health` and `/ready` to `503`
and draining in-flight requests before exiting.

The MCP server for AI agents is a separate process maintained in its own
repository — [github.com/ice1x/drevo-mcp](https://github.com/ice1x/drevo-mcp) —
which connects to a running `drevo-server` over HTTP / Bolt (it never opens the
redb file, so it never contends for redb's single-process lock).

---

## 2. Configuration

`drevo-server` is configured entirely through environment variables:

| Variable | Default | Meaning |
|----------|---------|---------|
| `DREVO_HOST` | `0.0.0.0` | Bind address. Must be non-empty. |
| `DREVO_PORT` | `8080` | TCP port (1–65535; `0` is rejected). |
| `DREVO_DATA_DIR` | `/data` | Directory holding the single `drevo.redb` file. |
| `RUST_LOG` | `info` | `tracing` env-filter (e.g. `drevo=debug,info`). |

Invalid configuration exits with code `2`; a runtime failure exits with `1`.

### Embeddings proxy (`/v1/embeddings`, opt-in)

drevo can host an **OpenAI-compatible** `POST /v1/embeddings` endpoint so one
instance serves graph, vector search, **and** embedding generation. It is
off by default: the endpoint is always present but answers `503`
(`embeddings backend not configured`) until a backend is compiled **and**
configured. The proxy backend forwards each request to a configured upstream
(OpenAI / Ollama / vLLM / any OpenAI-compatible server).

1. **Build** with the feature (pulls a `reqwest` HTTP client on the pure-Rust
   `rustls`+`ring` stack — no OpenSSL, no C toolchain):

   ```
   cargo build --release --features embeddings-proxy
   ```

2. **Configure** the upstream. These are read only under `embeddings-proxy`:

   | Variable | Default | Meaning |
   |----------|---------|---------|
   | `DREVO_EMBEDDINGS_UPSTREAM` | (unset) | Full upstream URL, e.g. `https://api.openai.com/v1/embeddings` or `http://localhost:11434/v1/embeddings`. Unset ⇒ endpoint stays `503`. Must be `http`/`https`. |
   | `DREVO_EMBEDDINGS_API_KEY` | (unset) | Bearer token forwarded as `Authorization: Bearer …`. |
   | `DREVO_EMBEDDINGS_MODEL` | (unset) | Default model when a request omits `model`. |

   A set-but-invalid `DREVO_EMBEDDINGS_UPSTREAM` (empty or non-http scheme)
   fails startup fast rather than degrading silently.

> **Security (OWASP A10 / SSRF).** The upstream is taken **only** from these
> variables — **never** from the request body. The request type carries no URL
> field, so a caller cannot redirect drevo's outbound call at an internal
> address (e.g. a cloud metadata endpoint). A host allowlist and a request
> timeout knob are tracked as Phase 20 / follow-up hardening.

Request / response shape:

```
POST /v1/embeddings
{ "model": "text-embedding-3-small", "input": ["text a", "text b"] }
-> { "object": "list",
     "data": [ { "object": "embedding", "index": 0, "embedding": [ … ] }, … ],
     "model": "text-embedding-3-small", "usage": { … } }
```

`input` accepts a single string or an array; an empty `input` is a `400`; an
upstream failure surfaces as `502`.

The proxy is a **transparent passthrough**: `model` + `input` are validated,
but any other request field (OpenAI `dimensions` / `encoding_format` / `user`,
Voyage / Anthropic-recommended `input_type` / `output_dimension`, …) is
forwarded verbatim, and the upstream response — including base64 embeddings
(`encoding_format: "base64"`) — is returned unchanged. So any OpenAI-compatible
provider works without drevo needing to know its parameter set. (The outbound
destination is still config-only — a forwarded field never changes it.)

---

## 3. Docker

The [`Dockerfile`](../Dockerfile) is a multi-stage build (Rust builder → `debian:bookworm-slim`
runtime) that compiles `--features http,redb-backend`, runs as a non-root `drevo` user (UID/GID
`999`), exposes port `8080`, and persists to the `/data` volume.

```bash
docker build -t drevo .
docker run -d -p 8080:8080 -v drevo-data:/data drevo
```

[`docker-compose.yml`](../docker-compose.yml) wires the same up with a healthcheck on `/health`:

```bash
docker compose up -d        # build + start
docker compose logs -f      # follow logs
docker compose down         # stop (data volume kept)
docker compose down -v      # stop and wipe data
```

---

## 4. Kubernetes

Manifests live under [`k8s/`](../k8s) as a Kustomize base plus `dev` / `prod` overlays.

```bash
kubectl apply -k k8s/base/            # base
kubectl apply -k k8s/overlays/prod/   # prod overlay (own namespace, larger PVC)
kubectl rollout status deployment/drevo
kubectl port-forward svc/drevo 8080:8080
```

Key constraints — **drevo is single-writer** (redb holds an exclusive file lock):

- `replicas: 1`, `strategy.type: Recreate` (a RollingUpdate would deadlock on the RWO PVC).
- The PersistentVolumeClaim is `ReadWriteOnce`; size it for the graph **plus** FTS indices
  (which can be several times the raw graph size).
- Liveness probe → `GET /health` (cheap, DB-independent); readiness → `GET /ready` (exercises
  redb, returns `503` during a SIGTERM drain).
- `terminationGracePeriodSeconds: 30` matches the server's drain window.
- Pod runs non-root (`runAsUser: 999`, `fsGroup: 999`).

The Service is `ClusterIP` by design — front it with your own Ingress / Gateway for TLS and
external exposure.

---

## 5. Observability

### Prometheus metrics

`GET /metrics` on `drevo-server` returns the Prometheus text exposition format. The standard
bundle (dependency-free, always compiled — see [`src/observability/`](../src/observability)):

| Metric | Type | Labels |
|--------|------|--------|
| `drevo_http_requests_total` | counter | `status` (`2xx`…`5xx`) |
| `drevo_http_request_duration_seconds` | histogram | — |
| `drevo_http_requests_in_flight` | gauge | — |
| `drevo_queries_total` | counter | `status` (`ok`/`error`) |
| `drevo_query_duration_seconds` | histogram | — |
| `drevo_process_uptime_seconds` | gauge | — |
| `drevo_build_info` | gauge | `version` |

Scrape config:

```yaml
scrape_configs:
  - job_name: drevo
    metrics_path: /metrics
    static_configs:
      - targets: ['drevo:8080']
```

### Structured query log

Every query emits a `tracing` event tagged with OpenTelemetry database semantic-convention
fields (`db.system="drevo"`, `db.operation`, `db.statement`, `otel.status_code`,
`duration_seconds`) — `info` on success, `warn` on failure. OTLP **wire** export is
deliberately out of scope (it would drag gRPC/protobuf into the default dependency graph); add
an opt-in `tracing-opentelemetry` layer if you want spans exported.

---

## 6. Storage, backup, and recovery

Data is a **single redb file**: `${DREVO_DATA_DIR}/drevo.redb`. redb is an embedded,
ACID, B-tree key-value store; every write commits in its own transaction (synchronous
durability). There is no separate WAL file to manage.

**Backup** — copy the single file (redb allows concurrent readers, so a live copy is safe):

```bash
cp /data/drevo.redb /backups/drevo-$(date +%Y%m%d-%H%M%S).redb
```

**Recover** — `Drevo::recover(path)` opens the file and runs an integrity scan
(`IntegrityReport`: counter drift, orphaned index entries, dangling edges, corrupt rows),
repairing counter drift automatically. `Drevo::compact()` reclaims space.

Monitor disk growth against node/edge count and FTS index size; pre-size the PVC accordingly.

---

## 7. Authentication

Bolt authentication is built in two layers:

- A dependency-free `Authenticator` trait (always compiled). A session with no authenticator
  accepts any connection; with one, it authenticates the Bolt `HELLO` before going ready.
- An Argon2id-backed `UserStore` behind the **`bolt-auth`** feature: passwords are stored as
  salted Argon2id PHC strings, and successful basic auth issues opaque 256-bit in-memory
  session tokens.

```rust
let mut store = UserStore::new();
store.add_user("neo4j", "secret")?;       // hashes with Argon2id
store.verify_basic("neo4j", "secret");    // -> bool
```

TLS for Bolt is provided by the **`bolt-tls`** feature (pure-Rust rustls, no OpenSSL):

```rust
let tls = TlsConfig::from_pem_files("/etc/drevo/cert.pem", "/etc/drevo/key.pem")?;
```

> Authorization / RBAC primitives exist at the substrate level but are not yet wired into the
> HTTP / Bolt request path — treat the network surface as trusted-network or front it with a
> gateway that enforces authz.

---

## 8. Replication & streaming (substrate)

These Phase 15 subsystems are **always-compiled library substrate** — the event/log/role model
and the seams to plug in transports — but are **not yet wired into the running server CLI**.
A deployment integrates them programmatically. The guide is honest about that so you don't go
looking for a config flag that isn't there.

### Replication ([`src/replication/`](../src/replication))

A Write-Ahead-Log-based MAIN/REPLICA model. A `Primary<B>` wraps a `StorageBackend`, tees every
write into a `WriteAheadLog` stamped with a monotonic `Lsn`, and returns the assigned LSN. A
read-only `Replica<B>` replays the log in LSN order, enforcing strict ordering and gap
detection. It ships no network transport — you stream the log delta over Bolt, HTTP long-poll,
or an in-process channel.

### Streaming ingestion ([`src/streaming/`](../src/streaming))

A transport-agnostic, broker-grade ingestion engine. An `IngestConsumer` drives a `StreamSource`
(Kafka / NATS / HTTP long-poll / CDC, behind a trait) into an `IngestSink` (a live `Drevo`
handle, behind a trait), polling batches, decoding `IngestEvent`s addressed by producer-owned
keys, applying them idempotently (last-writer-wins), committing offsets, and dead-lettering
un-ingestable messages. Re-delivery after a crash converges on the identical graph.

### CDC from PostgreSQL ([`src/streaming/cdc.rs`](../src/streaming/cdc.rs))

Decodes a Postgres logical-replication feed in the **wal2json** format and maps row changes into
streaming `IngestEvent`s via a declarative `SchemaMap`: a row's primary key becomes a namespaced
node key, columns map to `title` / `body` / `properties`, and declared foreign keys become
edges. drevo bundles **no** Postgres driver — supply the replication-slot connection and feed
the bytes in.

---

## 9. Operational quick reference

```bash
# Health
curl -sf http://localhost:8080/health      # liveness
curl -sf http://localhost:8080/ready       # readiness
curl    http://localhost:8080/status | jq  # version + uptime

# Metrics
curl -s http://localhost:8080/metrics | grep '^drevo_'

# Backup
cp /data/drevo.redb /backups/drevo-$(date +%F-%H%M%S).redb

# Full-graph export / import (JSON)
curl -s http://localhost:8080/export/json > graph.json
curl -X POST http://localhost:8080/import/json -H 'content-type: application/json' \
     -d "{\"dump\": $(cat graph.json)}"
```
