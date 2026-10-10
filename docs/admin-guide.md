# Admin Guide

Running drevo in production: deployment, configuration, observability, backups, authentication,
and the replication / streaming substrate. For calling the database see the
[SDK Reference](sdk-reference.md).

---

## 1. Binaries

drevo builds one binary (`Cargo.toml` `[[bin]]`):

| Binary | Source | Features | Role |
|--------|--------|----------|------|
| `drevo-server` | [`src/bin/server.rs`](https://github.com/ice1x/drevo/tree/main/src/bin/server.rs) | `http`, `redb-backend` | HTTP API + Web UI + Bolt listener. |

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
| `DREVO_QUERY_TIMEOUT_MS` | `0` (off) | Per-statement wall-clock limit in milliseconds, for HTTP `/cypher` and Bolt, applied to every database. A statement over it fails with `statement exceeded the N ms statement timeout` (Bolt: `Neo.ClientError.Transaction.TransactionTimedOut`) and releases its locks. An autocommit statement keeps the writes it made before the limit hit, as with any other runtime error; inside an explicit transaction, roll back as usual. Non-integer values are rejected. |
| `DREVO_PREPARED_TX_WARN_SECS` | `60` | Log an ERROR (and so raise a Web UI notification) for each two-phase-commit transaction that has stayed prepared longer than this many seconds. While one is prepared, every write is refused. `0` turns the alert off. |
| `DREVO_PREPARED_TX_TIMEOUT_SECS` | unset (never) | Opt-in **heuristic rollback** of a two-phase-commit transaction that has stayed prepared this long. This unblocks writes but overrides the coordinator: its late commit is refused with a heuristic-rollback error, and an ERROR is logged. Leave it off unless writes being blocked is worse than cross-store inconsistency. |
| `DREVO_HTTP_FORMATS` | `json` | Body formats the HTTP API offers next to JSON: a comma list of `json`, `cbor`, `msgpack`. See [Binary HTTP bodies](#binary-http-bodies-cbor-messagepack-opt-in). A format the binary was built without is rejected at startup. |
| `DREVO_WAL_FORMAT` | `json` | Encoding of the write-ahead log: `json`, `cbor` or `msgpack`. See [Write-ahead-log format](#write-ahead-log-format-opt-in). |
| `DREVO_GRPC_PORT` | unset (off) | Serve the gRPC API on this port, same host as HTTP. See [gRPC API](#grpc-api-opt-in). Rejected at startup by a build without the `grpc` feature. |
| `DREVO_FLIGHT_PORT` | unset (off) | Serve the Arrow Flight endpoint on this port, same host as HTTP. See [Arrow Flight](#arrow-flight-opt-in). Rejected at startup by a build without the `arrow-flight` feature. |
| `DREVO_AUTO_COMPACT` | `off` | Opt-in auto-compaction on open (`1`/`true`/`yes`/`on`). See §6. |
| `DREVO_AUTO_COMPACT_RATIO` | `2.0` | Minimum bloat ratio to trigger auto-compaction. |
| `DREVO_AUTO_COMPACT_MIN_BYTES` | `10485760` | Minimum file size (10 MiB) before auto-compaction is considered. |

Invalid configuration exits with code `2`; a runtime failure exits with `1`.

### Binary HTTP bodies (CBOR, MessagePack, opt-in)

JSON is the HTTP API's format and is always available. Two binary formats can
be offered next to it:

| Format | Media type | Cargo feature |
|--------|------------|---------------|
| CBOR | `application/cbor` | `format-cbor` |
| MessagePack | `application/msgpack` (also `application/x-msgpack`, `application/vnd.msgpack`) | `format-msgpack` |

Both are compiled into the Docker image. They are served only once listed in
`DREVO_HTTP_FORMATS`, e.g. `DREVO_HTTP_FORMATS=json,cbor,msgpack`.

A client then chooses per request:

- **Request body:** send `Content-Type: application/cbor` (or msgpack). A
  format that is not enabled is answered `415`, a malformed body `400`.
- **Response body:** send `Accept: application/cbor` (or msgpack); `q`
  weights are honoured. Without `Accept`, or with `*/*`, the answer is JSON. An
  `Accept` that names only formats the server does not offer is answered `406`.

Every JSON endpoint takes part, error bodies included. The Web UI, GraphML and
other non-JSON responses are never transcoded. Bolt is a separate, already
binary protocol and is not affected.

The gain is largest on embeddings. A float that fits in 32 bits, which is what
an embedding model returns, takes 5 bytes in CBOR against about 20 characters
in JSON, so a `/cypher` answer carrying vectors shrinks roughly fourfold.
MessagePack writes every float in 9 bytes.

```bash
curl -s localhost:8080/cypher -H 'Accept: application/cbor' \
  -H 'Content-Type: application/json' -d '{"query":"MATCH (n) RETURN n LIMIT 10"}' \
  -o result.cbor
```

### Write-ahead-log format (opt-in)

The write-ahead log (`native.wal`) is JSON Lines by default. With
`DREVO_WAL_FORMAT=cbor` or `msgpack` it is written as framed binary records
instead. Both formats are compiled into the Docker image (Cargo features
`format-cbor` / `format-msgpack`).

- **Switching:** set the variable and restart. The log is compacted at every
  start, and compaction rewrites it in the configured format, so the switch
  happens at that restart. It works in both directions, and a log in any
  compiled-in format opens whatever the setting is.
- **Size:** the gain is the embeddings. A float that fits in 32 bits takes 5
  bytes in CBOR against about 20 characters in JSON, so a log dominated by
  vectors shrinks several-fold. MessagePack writes every float in 9 bytes.
- **Durability:** unchanged. A binary log starts with an 8-byte header, and
  each record carries its length and a CRC-32. A torn last record (a crash
  mid-write) is dropped on open; a damaged record with valid records after it
  refuses to open, exactly as with JSON Lines.
- **Readability:** a binary log can no longer be inspected with `tail` or
  `jq`. To look inside, set `DREVO_WAL_FORMAT=json` and restart, or use
  `GET /export/json`.

### gRPC API (opt-in)

Next to HTTP and Bolt, drevo can serve Cypher over gRPC (HTTP/2 +
protobuf). It is compiled into the Docker image (Cargo feature `grpc`) and
served only when `DREVO_GRPC_PORT` is set, e.g. `DREVO_GRPC_PORT=50051`.

The schema is [`proto/drevo.proto`](https://github.com/ice1x/drevo/blob/main/proto/drevo.proto)
(package `drevo.v1`); generate a client for any language from it.

- **`Execute(ExecuteRequest)`** runs one statement (`query`, `params`, and
  optionally `database`) and streams back a `Header` with the column names,
  the rows in batches of 256, and a `Summary` with the write counters.
- **Values** keep their Cypher type, including nodes, relationships and
  paths. A list of floats (an embedding) is sent as a packed `FloatList`.
- **Errors** end the call with a status: `INVALID_ARGUMENT` for a query that
  does not parse or fails, `NOT_FOUND` for an unknown database, `UNAVAILABLE`
  while writes are paused by a prepared two-phase commit, `UNIMPLEMENTED` for
  database administration (use HTTP or Bolt).
- **`Health`** returns `ok` and the server version.

There is no authentication on the gRPC port, as on HTTP: expose it only on a
trusted network.

```bash
grpcurl -plaintext -import-path proto -proto drevo.proto \
  -d '{"query":"MATCH (n) RETURN n.title LIMIT 3"}' \
  localhost:50051 drevo.v1.Drevo/Execute
```

### Arrow Flight (opt-in)

For analytics, drevo can return Cypher results as Arrow record batches over
Arrow Flight, straight into pandas, polars, DuckDB or anything else that speaks
Flight. It is compiled into the Docker image (Cargo feature `arrow-flight`)
and served only when `DREVO_FLIGHT_PORT` is set, e.g.
`DREVO_FLIGHT_PORT=8815`.

Call `DoGet` with the statement as the ticket: either the Cypher text, or a
JSON object `{"query": …, "params": {…}, "database": …}`. The result comes
back in record batches of up to 8192 rows. Column types follow the values:

| Values in the column | Arrow type |
|---|---|
| integers | `Int64` |
| floats, or integers and floats | `Float64` |
| booleans | `Boolean` |
| strings | `Utf8` |
| lists of numbers (embeddings) | `List<Float64>` |
| nodes, relationships, maps, mixed types | `Utf8` holding JSON |

Only `DoGet` is served; other Flight calls answer `UNIMPLEMENTED`. As on
HTTP there is no authentication: expose the port only on a trusted network.

```python
import pyarrow.flight as flight
client = flight.connect("grpc://localhost:8815")
table = client.do_get(flight.Ticket(b"MATCH (t:Task) RETURN t.title, t.estimate")).read_all()
df = table.to_pandas()
```

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

   **Runtime configuration (env *or* Web UI).** The same three settings can be
   set at runtime — no restart, no env edit — through the Web UI **Settings**
   panel (the ⚙ button) or the `/config/embeddings` endpoint:

   ```
   GET  /config/embeddings   -> { "configured": true, "upstream": "…",
                                  "model": "…", "api_key_set": true }
   POST /config/embeddings   { "upstream": "https://…/v1/embeddings",
                               "api_key": "sk-…", "model": "…" }
   ```

   The `GET` status is **secret-free** — it reports only whether a key is set,
   never the key. On `POST`, a **blank/omitted `api_key` keeps** the stored
   secret (so the model can change without re-entering the token); a bad
   `upstream` is a `400`. The value is persisted to
   `<DREVO_DATA_DIR>/embeddings_config.json` (mode `0600`) and **wins over the
   env vars** on the next boot, so a UI-set key survives restarts. A change
   applies live to both `/v1/embeddings` and the semantic-query embedder.

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

The [`Dockerfile`](https://github.com/ice1x/drevo/tree/main/Dockerfile) is a multi-stage build (Rust builder → `debian:bookworm-slim`
runtime) that compiles `--features http,redb-backend`, runs as a non-root `drevo` user (UID/GID
`999`), exposes port `8080`, and persists to the `/data` volume.

```bash
docker build -t drevo .
docker run -d -p 8080:8080 -v drevo-data:/data drevo
```

[`docker-compose.yml`](https://github.com/ice1x/drevo/tree/main/docker-compose.yml) wires the same up with a healthcheck on `/health`:

```bash
docker compose up -d        # build + start
docker compose logs -f      # follow logs
docker compose down         # stop (data volume kept)
docker compose down -v      # stop and wipe data
```

### Keeping the container alive

Two layers cover two different failures (see [`scripts/README.md`](https://github.com/ice1x/drevo/tree/main/scripts/README.md)):

- **Docker restart policy** — the compose service sets `restart: unless-stopped`
  (and [`scripts/drevo-restart.sh`](https://github.com/ice1x/drevo/tree/main/scripts/drevo-restart.sh), the bare-`docker
  run` path, passes `--restart unless-stopped`). This relaunches the container
  after a crash, an OOM-kill, or a Docker/host reboot. It does **not** act on an
  intentional stop, and it **cannot** resurrect a *removed* container — a stray
  `docker rm -f drevo` (e.g. another project reusing the name) deletes the
  container object, leaving the restart policy nothing to act on.
- **Watchdog** — [`scripts/drevo-watchdog.sh`](https://github.com/ice1x/drevo/tree/main/scripts/drevo-watchdog.sh),
  scheduled every 30s via the launchd/systemd templates in
  [`scripts/watchdog/`](https://github.com/ice1x/drevo/tree/main/scripts/watchdog), recreates the container whenever it
  is missing or not running. This is the layer that survives an accidental
  `docker rm -f`. Pause it without a fight by `touch ~/.drevo-watchdog.disabled`.

---

## 4. Kubernetes

Manifests live under [`k8s/`](https://github.com/ice1x/drevo/tree/main/k8s) as a Kustomize base plus `dev` / `prod` overlays.

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
bundle (dependency-free, always compiled — see [`src/observability/`](https://github.com/ice1x/drevo/tree/main/src/observability)):

| Metric | Type | Labels |
|--------|------|--------|
| `drevo_http_requests_total` | counter | `status` (`2xx`…`5xx`) |
| `drevo_http_request_duration_seconds` | histogram | — |
| `drevo_http_requests_in_flight` | gauge | — |
| `drevo_queries_total` | counter | `status` (`ok`/`error`) |
| `drevo_query_duration_seconds` | histogram | — |
| `drevo_process_uptime_seconds` | gauge | — |
| `drevo_storage_file_bytes` | gauge | — |
| `drevo_statement_timeouts_total` | counter | — |
| `drevo_prepared_transactions` | gauge | — |
| `drevo_prepared_transaction_oldest_age_seconds` | gauge | — |
| `drevo_build_info` | gauge | `version` |

`drevo_storage_file_bytes` (#253 slice 1) is the physical on-disk size of the backend file,
refreshed on every scrape from an O(1) file stat (`0` for the ephemeral in-memory backend).
Pair it with the **logical** size from `GET /storage/bloat` (below) to alert on reclaimable
copy-on-write bloat.

`drevo_statement_timeouts_total` (#552) counts statements stopped by `DREVO_QUERY_TIMEOUT_MS`,
over HTTP and Bolt. Each one is also logged at **ERROR** (target `drevo::query`) with the
protocol, database, limit and query text (truncated to 2 000 characters). A timeout almost
always means a slow path in drevo worth fixing, so alert on any increase.

**Two-phase commit (#556).** A transaction prepared by a distributed-transaction coordinator (see
the [RFC](rfc-two-phase-commit.md) and the `drevo.tx.*` procedures in the Cypher reference)
**refuses every other write** until it is resolved. Writes fail fast and are retryable: HTTP 503,
Bolt `Neo.TransientError.Transaction.LockAcquisitionTimeout`. Prepared transactions survive restarts
and compaction, and are never resolved automatically. Operators can:

- watch `drevo_prepared_transactions` and `drevo_prepared_transaction_oldest_age_seconds`, and
  alert when the latter grows;
- rely on the ERROR the server logs once a transaction has been prepared longer than
  `DREVO_PREPARED_TX_WARN_SECS`;
- list and resolve transactions over HTTP on the default database:
  - `GET /transactions/prepared` → `{"prepared": [{gid, prepared_at, op_count}]}`;
  - `POST /transactions/prepared/{gid}/commit` and `POST /transactions/prepared/{gid}/rollback`.
    An unknown gid returns 404.
  - `POST /transactions/prepared/{gid}/heuristic-rollback`.

Resolve with the coordinator's decision. If the coordinator is gone and writes must resume, use the
**heuristic rollback**: HTTP above, `CALL drevo.tx.heuristicRollback($gid)`, or
`Drevo.heuristic_rollback_prepared` in Python. Unlike a plain rollback, the outcome is remembered,
so the coordinator's late commit fails loudly (HTTP 409) instead of silently finding nothing.
`DREVO_PREPARED_TX_TIMEOUT_SECS` does the same automatically. Any heuristic outcome means atomicity
across stores may be broken and must be reconciled.

A WAL that contains 2PC records cannot be read by a drevo release older than this one. An older
binary may even drop a trailing prepare silently. Before downgrading, resolve every prepared
transaction and compact the WAL.

**Problem feed.** `GET /problems?since=<seq>` returns the server's recent WARN/ERROR log events
(at most 200 kept in memory, oldest dropped first) as
`{"problems": [{seq, at, level, target, message, fields}], "next": <seq>}`; pass `next` back
as `since` to fetch only newer ones. Credential-looking fields (`*key*`, `*token*`,
`*secret*`, `*password*`, `authorization`) and `sk-…` API keys are redacted before they are
stored. A Bolt client dropping its connection is logged at debug, not as a problem.

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

### Storage bloat (#253 slice 1)

redb is copy-on-write: freed pages go to an internal freelist and the file **keeps its
high-water mark** — it never returns space to the OS on its own. Under heavy churn (constant
create/update/delete, FTS re-index, body rewrites — the agent-memory / KG workload) the file can
outgrow its live data, and only `compact()` (or a dump→fresh-import `shrink`) reclaims the slack.
**But a large file is not automatically bloat:** for text-heavy graphs the FTS trigram index is
legitimately several times the record data, so measure `bloat_ratio` (file ÷ *stored*, below)
before reaching for `compact`/`shrink` — a ratio near 1 means the file is already minimal and a
rebuild will only produce a same-size (or larger) file.

Observe the ratio and act on it:

```bash
# Physical size, continuously, via Prometheus:
curl -s http://localhost:8080/metrics | grep '^drevo_storage_file_bytes'

# On-demand storage report + bloat ratio (HTTP or CLI):
curl -s http://localhost:8080/storage/bloat        # {"file_bytes":…, "stored_bytes":…, "logical_bytes":…, "index_bytes":…, "bloat_ratio":…}
python -m drevo bloat /data/drevo.redb             # prints file / stored (records + index) + ratio, hints at ≥2×

# Reclaim when the ratio is high:
python -m drevo compact /data/drevo.redb           # in place (needs exclusive access)
python -m drevo shrink  /data/drevo.redb small.redb  # dump → fresh import (robust, writes a new file)
```

`bloat_ratio = file_bytes / stored_bytes`, where `stored_bytes` is **all** stored rows — the
`node` + `edge` records (`logical_bytes`) *plus* every secondary index (`index_bytes`: adjacency,
uuid/title/kind keys, property index, FTS trigrams, vectors). The index footprint is a legitimate
cost, not bloat — for text-heavy graphs the FTS trigram index alone can exceed the records several
times over, so dividing by `stored_bytes` (not `logical_bytes`) is what keeps an index-rich file
from reading as bloated. A ratio near 1 means the file is already minimal (compaction/rebuild
cannot shrink it); a ratio well above 1 is reclaimable copy-on-write high-water-mark slack. The
`/storage/bloat` scan streams the whole keyspace — a maintenance call, not per-request.

**Automatic compaction (opt-in, #253 slice 2).** Set `DREVO_AUTO_COMPACT=1` and drevo reclaims
bloat **on open**: the moment a database handle is built it is the sole owner of the file, which
is the one point that satisfies `compact()`'s exclusive-access requirement — so a churny,
long-lived store stays bounded across restarts instead of climbing forever. It fires only when
the file is at least `DREVO_AUTO_COMPACT_MIN_BYTES` (default 10 MiB) **and** the bloat ratio is at
least `DREVO_AUTO_COMPACT_RATIO` (default 2.0). It is **off by default**, and best-effort: a
compaction failure is logged and ignored so it never denies access to intact data. The reclaim
runs a `/storage/bloat`-style scan on open, so keep the ratio threshold meaningful rather than
near 1. For a long-running server that rarely restarts, also schedule the manual `compact` /
`shrink` above as a maintenance job.

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

### Replication ([`src/replication/`](https://github.com/ice1x/drevo/tree/main/src/replication))

A Write-Ahead-Log-based MAIN/REPLICA model. A `Primary<B>` wraps a `StorageBackend`, tees every
write into a `WriteAheadLog` stamped with a monotonic `Lsn`, and returns the assigned LSN. A
read-only `Replica<B>` replays the log in LSN order, enforcing strict ordering and gap
detection. It ships no network transport — you stream the log delta over Bolt, HTTP long-poll,
or an in-process channel.

### Streaming ingestion ([`src/streaming/`](https://github.com/ice1x/drevo/tree/main/src/streaming))

A transport-agnostic, broker-grade ingestion engine. An `IngestConsumer` drives a `StreamSource`
(Kafka / NATS / HTTP long-poll / CDC, behind a trait) into an `IngestSink` (a live `Drevo`
handle, behind a trait), polling batches, decoding `IngestEvent`s addressed by producer-owned
keys, applying them idempotently (last-writer-wins), committing offsets, and dead-lettering
un-ingestable messages. Re-delivery after a crash converges on the identical graph.

### CDC from PostgreSQL ([`src/streaming/cdc.rs`](https://github.com/ice1x/drevo/tree/main/src/streaming/cdc.rs))

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

# Storage bloat (physical vs. logical, #253 slice 1)
curl -s http://localhost:8080/storage/bloat | jq
python -m drevo bloat /data/drevo.redb

# Backup
cp /data/drevo.redb /backups/drevo-$(date +%F-%H%M%S).redb

# Full-graph export / import (JSON)
curl -s http://localhost:8080/export/json > graph.json
curl -X POST http://localhost:8080/import/json -H 'content-type: application/json' \
     -d "{\"dump\": $(cat graph.json)}"
```
