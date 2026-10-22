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
| `DREVO_QUERY_TIMEOUT_MS` | `0` (off) | Per-statement wall-clock limit in milliseconds, for HTTP `/cypher` and Bolt, applied to every database. A statement over it fails with `statement exceeded the N ms statement timeout` (Bolt: `Neo.ClientError.Transaction.TransactionTimedOut`) and releases its locks. A statement is all or nothing: an auto-commit statement that times out leaves none of its writes, and an explicit transaction is rolled back as usual. Non-integer values are rejected. |
| `DREVO_PREPARED_TX_WARN_SECS` | `60` | Log an ERROR (and so raise a Web UI notification) for each two-phase-commit transaction that has stayed prepared longer than this many seconds. While one is prepared, every write is refused. `0` turns the alert off. |
| `DREVO_PREPARED_TX_TIMEOUT_SECS` | unset (never) | Opt-in **heuristic rollback** of a two-phase-commit transaction that has stayed prepared this long. This unblocks writes but overrides the coordinator: its late commit is refused with a heuristic-rollback error, and an ERROR is logged. Leave it off unless writes being blocked is worse than cross-store inconsistency. |
| `DREVO_HTTP_FORMATS` | `json` | Body formats the HTTP API offers next to JSON: a comma list of `json`, `cbor`, `msgpack`. See [Binary HTTP bodies](#binary-http-bodies-cbor-messagepack-opt-in). A format the binary was built without is rejected at startup. |
| `DREVO_WAL_FORMAT` | `json` | Encoding of the write-ahead log: `json`, `cbor`, `msgpack`, `cbor-compact` (CBOR with the built-in field names as small integers), `cbor-zstd` or `cbor-compact-zstd` (the same, each record compressed with zstd). See [Write-ahead-log format](#write-ahead-log-format-opt-in). |
| `DREVO_READ_ONLY` | `false` | Serve every database read-only. See [Read-only databases](#read-only-databases). |
| `DREVO_AUDIT_LOG` | `off` | Security audit log: `off`, `stdout`, or a file path. See [Audit log](#audit-log). Needs the `auth` feature. |
| `DREVO_AUDIT_QUERY_TEXT` | `false` | Add a refused statement's text to its audit event. |
| `DREVO_OIDC_ISSUER` | unset (off) | Accept bearer tokens from this OpenID Connect provider: its exact `iss`. See [Identity provider (OIDC)](#identity-provider-oidc). Needs the `oidc` feature. |
| `DREVO_OIDC_AUDIENCE` | required with an issuer | A value the token's `aud` must hold, usually the client id. |
| `DREVO_OIDC_USERNAME_CLAIM` | `sub` | The claim naming the user, such as `preferred_username` or `email`. |
| `DREVO_OIDC_ROLES_CLAIM` | `groups` | The claim listing the user's groups. |
| `DREVO_OIDC_ROLE_MAP` | unset | Groups to roles: `admins=admin,devs=editor`. A group not listed gives no role. |
| `DREVO_BOLT_PORT` | unset (off) | Serve Bolt on this port, same host as HTTP (Neo4j drivers, `cypher-shell`). |
| `DREVO_IDLE_SESSION_TIMEOUT_MS` | `0` (off) | Close a Bolt connection that holds an open transaction and sends nothing for this many milliseconds, and roll the transaction back. Connections without a transaction may stay idle. See [Client timeouts](#client-timeouts). |
| `DREVO_TLS_CERT` | unset (off) | PEM certificate chain, leaf first. Together with `DREVO_TLS_KEY` it turns on TLS: HTTP is served as HTTPS and Bolt as `bolt+s://`. See [TLS](#tls). Setting only one of the pair, or a build without the `bolt-tls` feature, is rejected at startup. |
| `DREVO_TLS_KEY` | unset (off) | PEM private key for `DREVO_TLS_CERT` (PKCS#8, PKCS#1 RSA or SEC1 EC). |
| `DREVO_GRPC_PORT` | unset (off) | Serve the gRPC API on this port, same host as HTTP. See [gRPC API](#grpc-api-opt-in). Rejected at startup by a build without the `grpc` feature. |
| `DREVO_FLIGHT_PORT` | unset (off) | Serve the Arrow Flight endpoint on this port, same host as HTTP. See [Arrow Flight](#arrow-flight-opt-in). Rejected at startup by a build without the `arrow-flight` feature. |
| `DREVO_AUTO_COMPACT` | `off` | Opt-in auto-compaction on open (`1`/`true`/`yes`/`on`). See §6. |
| `DREVO_AUTO_COMPACT_RATIO` | `2.0` | Minimum bloat ratio to trigger auto-compaction. |
| `DREVO_AUTO_COMPACT_MIN_BYTES` | `10485760` | Minimum file size (10 MiB) before auto-compaction is considered. |

Invalid configuration exits with code `2`; a runtime failure exits with `1`.

### Client timeouts

A client can give its own statement or transaction a time limit, in milliseconds:

- **Bolt:** the `tx_timeout` that Neo4j drivers send on `BEGIN` and on an auto-commit `RUN`.
  In Python that is `session.run(Query(q, timeout=1))` or `@unit_of_work(timeout=1)`. For an
  explicit transaction the limit covers the whole transaction from `BEGIN`.
- **HTTP:** `timeout_ms` in the `POST /cypher` body:

  ```sh
  curl -s localhost:8080/cypher -H 'Content-Type: application/json' \
    -d '{"query": "MATCH (t:Ticket) RETURN count(t)", "timeout_ms": 2000}'
  ```

Past the limit the statement stops and an explicit transaction is rolled back. Over Bolt the error is
`Neo.ClientError.Transaction.TransactionTimedOutClientConfiguration`, which drivers do not retry.
A transaction that runs past its limit while the client sends nothing is rolled back at once. The
connection stays open, and the next `RUN` or `COMMIT` reports the timeout. `0` means no limit.

`DREVO_QUERY_TIMEOUT_MS` caps a client's limit: a client cannot ask for more time than the server
gives a statement. When the server's limit is the shorter one, the error is the server's
`Neo.ClientError.Transaction.TransactionTimedOut`.

`DREVO_IDLE_SESSION_TIMEOUT_MS` deals with clients that open a transaction and go silent, such as a
crashed script or a paused debugger. Their connection is closed and the transaction rolled back,
so it stops holding its snapshot of the graph and any locks. The driver sees a closed connection and
opens a new one. The silence is counted from the server's last reply, so a statement that runs
longer than the limit is not idle time.

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
instead. `cbor-compact` is CBOR in which the built-in field names of a
record (`UpsertNode`, `id`, `kind`, `created_at`, …) are small integers
while property names stay text; `cbor-zstd` and `cbor-compact-zstd` also
compress each record with zstd (level 3). All of them are compiled into the
Docker image (Cargo features `format-cbor` / `format-msgpack` / `wal-zstd`).

- **Switching:** set the variable and restart. The log is compacted at every
  start, and compaction rewrites it in the configured format, so the switch
  happens at that restart. It works in both directions, and a log in any
  compiled-in format opens whatever the setting is.
- **Size:** which format is smallest depends on what the records hold. A
  float that fits in 32 bits takes 5 bytes in CBOR against about 20
  characters in JSON, so a log dominated by vectors shrinks several-fold
  (MessagePack writes every float in 9 bytes), and zstd takes a further third
  off such records. A log of small records is mostly field names, which
  `cbor-compact` removes, while a record that small does not compress and
  still pays the zstd frame header. Two real logs, compacted (reopen is the
  best of three on the same machine):

  | Format | Knowledge graph: 4,185 nodes, 2,152 with a 1,536-float embedding | Road network: 240,451 junctions and 278,377 roads, 518,828 records |
  |---|---:|---:|
  | `json` | 73.8 MB, 199 ms | 127.3 MB, 1.25 s |
  | `cbor` | 19.5 MB, 208 ms | 91.7 MB, 1.33 s |
  | `msgpack` | 39.2 MB, 185 ms | 93.7 MB, 1.10 s |
  | `cbor-zstd` | 12.8 MB, 217 ms | 93.4 MB, 1.56 s |
  | `cbor-compact` | 18.6 MB, 281 ms | **59.2 MB**, 2.35 s |
  | `cbor-compact-zstd` | **12.1 MB**, 289 ms | 63.9 MB, 2.40 s |

  So: `cbor-zstd` for logs of large records such as embeddings and long
  texts; `cbor-compact` where disk matters more than restart time on a log
  of many small records (it reads through an extra tree, so it reopens
  slower than `cbor`); plain `cbor` otherwise, and when a log must stay
  readable by a build without the `wal-zstd` feature. Compressing costs
  about 10 µs a record. A compressed JSON record would still be larger than
  plain CBOR, which is why compression comes with CBOR only.
- **Durability:** unchanged. A binary log starts with an 8-byte header, and
  each record carries its length and a CRC-32. A torn last record (a crash
  mid-write) is dropped on open; a damaged record with valid records after it
  refuses to open, exactly as with JSON Lines.
- **Readability:** a binary or compressed log can no longer be inspected
  with `tail` or `jq`. To look inside, set `DREVO_WAL_FORMAT=json` and restart, or use
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

When the server requires a login (see [Logging in](#logging-in)), `Execute`
needs an `authorization` metadata entry written as the HTTP header:
`Basic <base64 of user:password>` or `Bearer <session token>` (from
`POST /auth/login`). Without a valid one the call ends with
`UNAUTHENTICATED`; too many failed logins in a row with
`RESOURCE_EXHAUSTED`; a user who must change the password first with
`PERMISSION_DENIED` (change it over HTTP or Bolt). `Health` needs no login.

With [TLS](#tls) configured, use `-cacert cert.pem` instead of `-plaintext`.

```bash
grpcurl -plaintext -import-path proto -proto drevo.proto \
  -H "authorization: Basic $(printf 'ann:ann-password' | base64)" \
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

Only `DoGet` and `Handshake` are served; other Flight calls answer
`UNIMPLEMENTED`.

When the server requires a login, `DoGet` takes the same `authorization`
header as the gRPC API. `Handshake` with a `Basic` header answers with a
session token, as the payload and as an `authorization: Bearer <token>`
header, so a client logs in once with the password and then sends only the
token:

With [TLS](#tls) configured, connect to `grpc+tls://` and pass `tls_root_certs`.

```python
import pyarrow.flight as flight
client = flight.connect("grpc://localhost:8815")
token = client.authenticate_basic_token("ann", "ann-password")
options = flight.FlightCallOptions(headers=[token])
table = client.do_get(
    flight.Ticket(b"MATCH (t:Task) RETURN t.title, t.estimate"), options
).read_all()
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

### Text-to-Cypher (`drevo.cypher.fromText`, opt-in)

`CALL drevo.cypher.fromText($question) YIELD cypher` turns a plain-language
question into a Cypher query with an LLM. drevo sends the question and the
graph's schema (labels, relationship types, property keys) to an
OpenAI-compatible `chat/completions` endpoint and returns the generated query
**without running it**. The Web UI's **Ask** button uses it. It needs the
`embeddings-proxy` build feature, which the Docker image includes.

| Variable | Default | Meaning |
|----------|---------|---------|
| `DREVO_TEXT2CYPHER_UPSTREAM` | (unset) | Full `chat/completions` URL. Unset ⇒ the procedure reports "not configured". Must be `http`/`https`. |
| `DREVO_TEXT2CYPHER_MODEL` | `gpt-4o-mini` | Chat model to request. |
| `DREVO_TEXT2CYPHER_API_KEY` | (unset) | Bearer token, if the upstream needs one. |

A local Ollama needs no key. From the drevo container it is reached through the
Docker host:

```
DREVO_TEXT2CYPHER_UPSTREAM=http://host.docker.internal:11434/v1/chat/completions
DREVO_TEXT2CYPHER_MODEL=qwen2.5-coder:14b
```

`GET /` reports `"text2cypher": {"model": "…"}` once an upstream is configured
(`null` otherwise); the upstream URL is not exposed. The prompt carries the
whole schema, so on a large graph a model running on CPU can take minutes;
on Apple Silicon run the native arm64 Ollama so the model uses the GPU.

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

### Memory after large deletes

The image sets `MALLOC_ARENA_MAX=2`. glibc's allocator otherwise keeps one
memory arena per busy thread, and what a delete and rewrite of a large graph
frees in them stays resident. On a 112,000-node, 210,000-relationship road
graph, a delete and rewrite took the process from 265 MB to 398 MB without
the setting (198 MB after a restart), and to 219 MB with it.

Running `drevo-server` on Linux outside the image, set the same variable:

```bash
MALLOC_ARENA_MAX=2 drevo-server
```

### Upgrading a server

A new image opens the data directory of an earlier one as it is: the write-ahead log keeps
its format, and there is no migration step. The image requires a
login, so a data directory with no `<DREVO_DATA_DIR>/security.json` gets its first user
when the new image first starts (see [Users](#users)). Choose that user before the first
start. Otherwise the server creates `neo4j` with a generated password, and clients that
connect without credentials are refused.

1. Copy the data while the old container is stopped, so you can go back:

   ```bash
   docker stop drevo
   cp -a ~/drevo_data ~/drevo_data.bak-before-upgrade
   ```

2. Set `DREVO_AUTH` in the container's environment: `none` to keep connections without a
   login, or `user/password` for the first user, whose credentials the clients then pass.
   A wrapper script that runs `docker run` must forward the variable;
   [`scripts/drevo-restart.sh`](https://github.com/ice1x/drevo/tree/main/scripts/drevo-restart.sh)
   forwards `DREVO_AUTH` from `~/.drevo.env`.

   ```bash
   docker run -d --name drevo -p 8080:8080 -p 7687:7687 \
     -e DREVO_AUTH=none -v ~/drevo_data:/data ice1x/drevo:latest
   ```

3. Check the first start's log. It names what was done with `DREVO_AUTH`:
   `DREVO_AUTH=none: no first user created`, `created the first user from DREVO_AUTH`, or a
   `WARN` line with the generated `neo4j` password.

   ```bash
   docker logs drevo 2>&1 | grep DREVO_AUTH
   ```

4. Compare the node count with the one before the upgrade:

   ```bash
   curl -s -X POST localhost:8080/cypher -H 'Content-Type: application/json' \
     -d '{"query": "MATCH (n) RETURN count(n) AS nodes"}'
   ```

A data directory that already has `security.json` keeps its users, whatever `DREVO_AUTH`
says.

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
Bolt `Neo.TransientError.Transaction.LockAcquisitionTimeout`. Reads, including explicit
transactions that only read, keep working. Prepared transactions survive restarts
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

Each database is a write-ahead log plus a few sidecars. The default database lives in
`${DREVO_DATA_DIR}/native.wal`, and every other one in `${DREVO_DATA_DIR}/databases/<name>/`.
Next to each log sit:

- `origin.json`, `tombstones.json` and `stamps.jsonl`: replica identity and causal stamps;
- `semantic.json`: index registrations and constraints.

The users (`security.json`) and the embeddings settings (`embeddings_config.json`) are kept in
the data directory itself. Every acknowledged write is fsynced to the log before it returns; the
log is compacted in place as it grows.

### Physical backups

A physical backup copies every database's log and sidecars, plus the server's files, while the
server keeps serving. Writers pause only for the moment it takes to note where each log ends.
Each file is recorded with its SHA-256 in a manifest.

```bash
curl -X POST http://localhost:8080/admin/backup -H 'content-type: application/json' \
     -d '{"name": "monday"}'
curl -X POST http://localhost:8080/admin/backup -H 'content-type: application/json' \
     -d '{"name": "tuesday", "since": "monday"}'
curl http://localhost:8080/admin/backups
```

- **Where.** Backups go to `DREVO_BACKUP_DIR`, `<data_dir>/backups` by default; copy them off
  the machine as usual. Without a `name`, one is made from the time (`backup-20261017-153012`).
- **Incremental.** With `since`, each database that continues that backup gets only the log
  written since. A database created since, or whose log was compacted since, is backed up in
  full, and the answer says why (`full_because`).
- **Logins.** With logins required, both routes need `ALL DBMS PRIVILEGES`.
- **Web UI.** The Storage panel (⛁) lists the newest backups and has **Full backup** and
  **Incremental** buttons; Incremental continues the newest backup in the list.

A stopped server is backed up from the command line, with the same environment:

```bash
DREVO_DATA_DIR=/data drevo-server backup --to /backups/monday
DREVO_DATA_DIR=/data drevo-server backup --to /backups/tuesday --since /backups/monday
```

**Restore** into a new, empty data directory, giving the full backup and then each incremental in
the order they were taken. Every checksum and the chain are checked first; a damaged file or a
gap stops the restore and leaves nothing behind. Then point the server at the restored directory.

```bash
drevo-server restore --from /backups/monday --from /backups/tuesday --to /data-restored
DREVO_DATA_DIR=/data-restored drevo-server
```

With Docker, the image's entry point is `drevo-server`, so the command follows the image name
in a container that mounts the volumes (with the server's container stopped, for `backup`):

```bash
docker run --rm -v drevo_data:/data -v drevo_backups:/backups ice1x/drevo backup --to /backups/monday
```

A GraphML or JSON export (`GET /export/graphml`, `GET /export/json`) remains the portable,
logical copy of one database's graph.

### Read-only databases

A read-only database answers queries, search and exports, and refuses every write and schema
change with Neo4j's `Neo.ClientError.General.ForbiddenOnReadOnlyDatabase` (HTTP 403). Drivers do
not retry it.

Make one database read-only, and back:

```bash
curl -X POST http://localhost:8080/cypher -H 'content-type: application/json' \
     -d '{"query": "ALTER DATABASE erp SET ACCESS READ ONLY"}'
curl -X POST http://localhost:8080/cypher -H 'content-type: application/json' \
     -d '{"query": "ALTER DATABASE erp SET ACCESS READ WRITE"}'
```

The setting is kept in `<data_dir>/database_access.json`, so it survives a restart.
`SHOW DATABASES` has an `access` column (`read-only` or `read-write`), and `GET /databases` an
`access` map and `read_only_server`. The Web UI marks a read-only database in its picker. With
logins required, `ALTER DATABASE` needs `DATABASE MANAGEMENT`.

`DREVO_READ_ONLY=true` makes the whole server read-only:

- every database is opened read-only;
- no database can be created, dropped or switched;
- nothing in the data directory is written, so it can be mounted read-only. This suits a
  reporting replica, a published snapshot or a demo copy.

The server still writes its users and settings files when they change. Create the users
beforehand, or set `DREVO_AUTH=none`.

```bash
docker run -d -p 8080:8080 -e DREVO_READ_ONLY=true -v drevo_data:/data:ro ice1x/drevo
```

### Encryption at rest

A server built with the `encryption` feature (the Docker image has it) encrypts every database
with a 256-bit key when one is given. Each log record and each sidecar is sealed with
XChaCha20-Poly1305: the data on disk reads as random bytes, and a changed byte is detected and
refused. Encrypted:

- every database's log, `origin.json`, `tombstones.json`, `stamps.jsonl`, `semantic.json` and
  `sequences.json`;
- the server's `database_access.json`, `security.json` (users, password hashes, roles and
  privileges) and `embeddings_config.json` (the embeddings provider and its API key);
- physical backups, which copy the encrypted files as they are.

Nothing in the data directory is left in the clear.

Make a key and keep it outside the data directory; without it the data cannot be read:

```bash
openssl rand -hex 32 > /etc/drevo/drevo.key
chmod 600 /etc/drevo/drevo.key
DREVO_ENCRYPTION_KEY_FILE=/etc/drevo/drevo.key drevo-server
```

`DREVO_ENCRYPTION_KEY` takes the 64 hex digits directly instead (set one of the two). A server
started without the key, or with another one, refuses to open an encrypted database, and a
server with a key refuses a database in the clear.

Encrypt existing data, change the key, or go back to the clear, with the server stopped:

```bash
# Encrypt a data directory in the clear with the key
DREVO_DATA_DIR=/data DREVO_ENCRYPTION_KEY_FILE=/etc/drevo/drevo.key drevo-server encrypt
# Re-encrypt with a new key; afterwards start the server with the new key
DREVO_DATA_DIR=/data DREVO_ENCRYPTION_KEY_FILE=/etc/drevo/drevo.key \
    drevo-server rotate-key --new-key-file /etc/drevo/new.key
# Decrypt
DREVO_DATA_DIR=/data DREVO_ENCRYPTION_KEY_FILE=/etc/drevo/new.key drevo-server decrypt
```

Each command rewrites every database and swaps each file in atomically. A backup or restore of an
encrypted server needs the same key in its environment.

What it costs (`cargo bench --features encryption --bench encryption_bench`, Apple M-series):

| Operation | Compared with the clear |
|---|---|
| One durable write | about the same (the fsync dominates) |
| A batch of 1,000 nodes | +10% |
| Reopening a store (replaying its log) | +107% |

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

Users, passwords, session tokens and roles come from the
[`drevo-auth`](https://github.com/ice1x/drevo/tree/main/drevo-auth) crate, which drevo
re-exports. Bolt authentication is built in two layers:

- A dependency-free `Authenticator` trait (always compiled). A session with no authenticator
  accepts any connection; with one, it authenticates the Bolt `HELLO` before going ready.
- An Argon2id-backed `UserStore` behind the **`auth`** feature (`bolt-auth` is the same
  feature under its earlier name). Passwords are stored as salted Argon2id PHC strings, and
  a successful login can issue a session token: 32 random bytes from the operating system,
  kept in memory.

```rust
use drevo::bolt::auth::UserStore;

let mut store = UserStore::new();
store.add_user("neo4j", "secret")?;           // hashes with Argon2id
assert!(store.verify_basic("neo4j", "secret"));
let token = store.issue_token("neo4j");       // Some(64 hex characters)
```

Roles and privileges live in the user catalog (the crate's `privileges` and `users` modules,
below).

### Users

A server built with the `auth` feature (the Docker image is) keeps its users in
`<DREVO_DATA_DIR>/security.json`. The file holds Argon2id hashes, never passwords, and is
readable by its owner only. A damaged file stops the server rather than starting it with no
users.

The first user is created from `DREVO_AUTH` when there are none, as with Neo4j's
`NEO4J_AUTH`:

| `DREVO_AUTH` | First start |
|---|---|
| `admin/long-password` | creates `admin` with that password (at least 8 characters) |
| `none` | creates nobody |
| unset | creates `neo4j` with a generated password, logged once at `WARN`; it must be changed at the first login |

Later starts leave existing users alone, whatever `DREVO_AUTH` says.

Manage users with Cypher over HTTP (`POST /cypher`) or Bolt, in auto-commit mode; inside an
explicit transaction these commands fail with
`Neo.ClientError.Transaction.ForbiddenDueToTransactionType`:

```
CREATE USER ann SET PASSWORD 'first-password'
CREATE USER bob IF NOT EXISTS SET PASSWORD $pw CHANGE NOT REQUIRED SET STATUS SUSPENDED
ALTER USER ann SET PASSWORD 'second-password' CHANGE REQUIRED
ALTER USER bob SET STATUS ACTIVE
DROP USER bob IF EXISTS
SHOW USERS
SHOW CURRENT USER
ALTER CURRENT USER SET PASSWORD FROM 'old-password' TO 'new-password'
```

`SHOW USERS` returns `user`, `roles`, `passwordChangeRequired`, `suspended` and `home`.
`roles` lists `PUBLIC`, which every user has, then the roles given to the user. The first user
(from `DREVO_AUTH`) has `admin`. The catalog also holds Neo4j's built-in roles (`PUBLIC`,
`reader`, `editor`, `publisher`, `architect`, `admin`) with their default privileges. A catalog
written before roles existed is upgraded on the first start: its users get `admin`, so they keep
the access they had. An upgraded `security.json` cannot be read by older drevo versions.
Passwords are string literals or `$parameters`; neither a password nor its hash ever appears in
a result, an error or a log line. Errors use Neo4j codes: a bad name or password, an existing or
unknown user is `Neo.ClientError.General.InvalidArguments` (HTTP 400, 409 or 404).

### Roles and privileges

Roles hold privileges in Neo4j's model, and users get roles. Manage them with Cypher, in
auto-commit mode like the user commands:

```
CREATE ROLE triage IF NOT EXISTS AS COPY OF reader
RENAME ROLE triage TO bug_triage
DROP ROLE bug_triage IF EXISTS
GRANT ROLES reader, triage TO ann, bob
REVOKE ROLE triage FROM bob
SHOW POPULATED ROLES WITH USERS

GRANT MATCH {*} ON GRAPH bugs NODES Bug, Feature TO triage
DENY READ {reporter} ON GRAPH bugs NODES Bug TO triage
GRANT SET PROPERTY {status} ON GRAPH bugs NODES Bug TO triage
GRANT ACCESS ON DATABASE bugs TO triage
GRANT EXECUTE PROCEDURE apoc.text.* ON DBMS TO triage
REVOKE DENY READ {reporter} ON GRAPH bugs NODES Bug FROM triage

SHOW ROLE triage PRIVILEGES
SHOW USER ann PRIVILEGES
SHOW PRIVILEGES AS COMMANDS
SHOW SUPPORTED PRIVILEGES
```

- **Graph privileges**: `TRAVERSE`, `READ {props}`, `MATCH {props}`, `CREATE`, `DELETE`,
  `SET LABEL labels`, `REMOVE LABEL labels`, `SET PROPERTY {props}`, `MERGE {props}`, `WRITE`
  and `ALL GRAPH PRIVILEGES`, `ON GRAPH * | name, … | HOME GRAPH`, for `NODES labels`,
  `RELATIONSHIPS types` or `ELEMENTS …` (every node and relationship when omitted).
- **Database privileges**: `ACCESS`, `START`, `STOP`, `INDEX`, `CONSTRAINT`, `NAME` and
  `TRANSACTION [MANAGEMENT] [(users)]`, `ALL DATABASE PRIVILEGES`,
  `ON DATABASE * | name, … | HOME DATABASE`.
- **DBMS privileges**: `USER`, `ROLE`, `PRIVILEGE` and `DATABASE MANAGEMENT`,
  `EXECUTE PROCEDURE globs`, `EXECUTE FUNCTION globs`, `EXECUTE ADMIN PROCEDURES`,
  `ALL DBMS PRIVILEGES`, `ON DBMS`.

A deny always wins over a grant. `REVOKE GRANT …` or `REVOKE DENY …` takes back one kind;
`REVOKE …` alone takes back both. `SHOW … PRIVILEGES` returns Neo4j's columns `access`,
`action`, `resource`, `graph`, `segment`, `role` and `immutable` (plus `user` for user
privileges). `AS COMMANDS` returns the `GRANT` / `DENY` commands that recreate them, and
`AS REVOKE COMMANDS` returns the commands that remove them. Run on a server with the same
roles and no privileges, the `AS COMMANDS` output recreates the same privileges.

Every user has `PUBLIC`, which cannot be dropped, renamed, granted or revoked. The built-in
roles start with Neo4j's default privileges:

| Role | Privileges |
|---|---|
| `PUBLIC` | `ACCESS ON HOME DATABASE`, `EXECUTE PROCEDURE *`, `EXECUTE FUNCTION *` |
| `reader` | `ACCESS ON DATABASE *`, `MATCH {*}` on every node and relationship |
| `editor` | `reader`'s, and `WRITE ON GRAPH *` |
| `publisher` | `editor`'s, and `NAME MANAGEMENT ON DATABASE *` |
| `architect` | `publisher`'s, and `INDEX` and `CONSTRAINT MANAGEMENT ON DATABASE *` |
| `admin` | `architect`'s, and `START`, `STOP`, `TRANSACTION MANAGEMENT ON DATABASE *` and `ALL DBMS PRIVILEGES` |

Not supported: `IMMUTABLE` privileges, property-based segments (`WHERE` / `FOR`) and
`DEFAULT GRAPH`.

#### What is checked

When the server requires a login, every statement on Bolt, HTTP, gRPC and Arrow Flight runs
with the signed-in user's privileges. A statement that needs a privilege the user lacks fails
with `Neo.ClientError.Security.Forbidden` (HTTP 403, gRPC `PERMISSION_DENIED`), and nothing of
it is written:

- **Database**: using a database needs `ACCESS` on it (`PUBLIC` has its home database, the
  default one); `CREATE DATABASE` needs `DATABASE MANAGEMENT`.
- **Writes**: `CREATE` needs `CREATE` on the node's labels or the relationship's type, plus
  `SET PROPERTY` for each property it sets; `DELETE` needs `DELETE`; `SET` / `REMOVE` of a
  property need `SET PROPERTY` for it; `SET` / `REMOVE` of a label need `SET LABEL` /
  `REMOVE LABEL`. `MERGE`, `FOREACH` and the `drevo.memory.*` procedures write through the
  same checks.
- **Schema**: `CREATE` / `DROP INDEX` need `INDEX MANAGEMENT`, constraints `CONSTRAINT
  MANAGEMENT`. `SHOW INDEXES` / `SHOW CONSTRAINTS` need only access.
- **Users, roles and privileges**: `CREATE` / `ALTER` / `DROP USER` and `SHOW USERS` need
  `USER MANAGEMENT`; the role commands and `SHOW ROLES` need `ROLE MANAGEMENT`; `GRANT` /
  `DENY` / `REVOKE` and `SHOW PRIVILEGES` need `PRIVILEGE MANAGEMENT`. Every user may run
  `SHOW CURRENT USER`, `ALTER CURRENT USER SET PASSWORD`, `SHOW USER PRIVILEGES` (their own)
  and `SHOW SUPPORTED PRIVILEGES`.
- **Procedures**: every `CALL` needs `EXECUTE PROCEDURE` (which `PUBLIC` has for every
  procedure). The two-phase-commit procedures (`drevo.tx.prepare`, which pauses every write
  until resolved, and `commitPrepared`, `rollbackPrepared`, `heuristicRollback`,
  `listPrepared`) also need `EXECUTE ADMIN PROCEDURES`, and
  `drevo.semantic.register` / `reindex` need `INDEX MANAGEMENT`.

Reads filter instead of failing, as in Neo4j:

- A node the user may not `TRAVERSE` is not matched, expanded, counted (`count(*)` included),
  returned or found by `fts.search` and the other search, vector and memory procedures. A
  relationship is hidden when its type may not be traversed or either of its nodes is hidden,
  so variable-length and shortest paths route around hidden nodes.
- A property without `READ` reads as `null`: `n.salary`, `keys(n)`, `properties(n)` and a
  returned `n` leave it out, and `WHERE n.salary > 0` or `{salary: …}` in a pattern does not
  match on it.
- `SHOW DATABASES` lists only the databases the user may access.

Roles with graph-wide grants and no graph denies (`reader`, `editor`, `admin`) skip filtering
altogether, at no measurable cost. A role with denies costs about 1.6× on a filtered scan of
20 000 nodes that hides a property on every node (`cargo bench --features auth --bench
privilege_bench`).

Still not filtered: graph algorithms (`drevo.pagerank`, …) compute over the whole graph, and
only their results are filtered; `db.labels()` / `db.propertyKeys()` list every name. `MERGE`
does not match a node the user cannot see, and so may create a second one; constraint checks
still see all data.

The REST routes outside `/cypher` serve the graph as stored, so they need privileges under
which filtering would change nothing (otherwise `403`, `Neo.ClientError.Security.Forbidden`):

| Routes | Need |
|---|---|
| `GET /export/…`, `/nodes…`, `/edges`, `/facets`, `/paths/shortest`, `POST /search/fts` | read the whole graph (no graph denies, e.g. `reader`) |
| `POST /nodes`, `/edges`, `/import/json` | read and write the whole graph (e.g. `editor`) |
| `POST /databases`, `DELETE /databases/{name}` | `DATABASE MANAGEMENT` |
| `/storage/…`, `/config/…`, `/transactions/prepared…`, `/problems`, `/report` | `ALL DBMS PRIVILEGES` (`admin`) |

`/status`, `/auth/…`, `GET /databases`, `/metrics`, `/v1/embeddings` and the Web UI need only a
login.

A role change applies from the next statement. A server started with `DREVO_AUTH=none` checks
nothing.

### Logging in

Bolt, HTTP, gRPC and Arrow Flight clients must log in as one of these users, unless the server
runs with `DREVO_AUTH=none`:

- **Bolt**: the `HELLO` credentials, a password (`basic`) or a session token (`bearer`).
- **HTTP**: `Authorization: Basic …` or `Bearer <session token>`, or the `drevo_session` cookie
  that `POST /auth/login` sets for the Web UI, which then shows a sign-in screen. `/`, `/health`,
  `/ready`, `/status` and the Web UI's own files stay open, so health checks keep working.
  Refused requests get `401` (`429` while locked, `403` until a required password change).
- **gRPC and Arrow Flight**: an `authorization` metadata entry, `Basic …` or `Bearer <session
  token>`, on every call (see [gRPC API](#grpc-api-opt-in) and [Arrow Flight](#arrow-flight-opt-in)).
- Session tokens expire after 30 minutes unused or 12 hours in all.

Either way:

- A wrong password, an unknown user or a suspended user gets
  `Neo.ClientError.Security.Unauthorized` with the same message, and the connection closes.
- Three failed logins in a row lock the user for five seconds
  (`Neo.ClientError.Security.AuthenticationRateLimit`), even with the right password.
- A user whose password must be changed can log in but can only run
  `ALTER CURRENT USER SET PASSWORD FROM … TO …`; anything else gets
  `Neo.ClientError.Security.CredentialsExpired`.
- `SHOW CURRENT USER` and `ALTER CURRENT USER` act on the logged-in user.
- Changing a user's password, suspending or dropping the user ends their session tokens.

Without TLS, passwords and session tokens travel in clear text: turn TLS on (below) or keep
the server on a trusted network.

### Identity provider (OIDC)

drevo accepts access tokens from an OpenID Connect provider, such as Keycloak, Kanidm,
Auth0, Entra ID or Dex. A client sends the token where it would send a password:

- HTTP: `Authorization: Bearer <token>` on any request.
- Bolt: the `bearer` scheme, e.g. `neo4j.bearer_auth(token)` in the Python driver.
- gRPC and Arrow Flight: `authorization: Bearer <token>` metadata.

```bash
docker run -d --name drevo -p 8080:8080 -p 7687:7687 -v drevo_data:/data \
  -e DREVO_BOLT_PORT=7687 \
  -e DREVO_OIDC_ISSUER=https://login.example.com/realms/drevo \
  -e DREVO_OIDC_AUDIENCE=drevo \
  -e DREVO_OIDC_USERNAME_CLAIM=preferred_username \
  -e DREVO_OIDC_ROLE_MAP='drevo-admins=admin,drevo-devs=editor,staff=reader' \
  ice1x/drevo:latest
```

```python
from neo4j import GraphDatabase, bearer_auth

driver = GraphDatabase.driver("bolt://localhost:7687", auth=bearer_auth(access_token))
driver.execute_query("MATCH (t:Task) RETURN t.title")
```

drevo checks each token as follows:

1. The server fetches the provider's keys from `<issuer>/.well-known/openid-configuration`
   and caches them. A token signed with an unknown key fetches them again, at most once a
   minute, so key rotation needs no restart.
2. The signature must be RS256 or ES256 and verify against those keys. `none`, HMAC and
   other algorithms are refused.
3. `iss` must equal `DREVO_OIDC_ISSUER`, and `aud` must hold `DREVO_OIDC_AUDIENCE`.
4. `exp` must be in the future and `nbf`, if present, in the past, with 60 seconds of clock
   skew allowed.

A refused token gets the same `Neo.ClientError.Security.Unauthorized` answer as a wrong
password. The audit log records why it was refused (`login_failed`).

The provider and its key set must be served over HTTPS, except from the same machine
(`localhost`, `127.0.0.1`, `::1`).

The user is the value of `DREVO_OIDC_USERNAME_CLAIM`, and their roles come from the groups in
`DREVO_OIDC_ROLES_CLAIM` through `DREVO_OIDC_ROLE_MAP`. Everyone also has `PUBLIC`. The
roles and their privileges are the server's own: create and grant them as for local users,
and a change applies to the next statement.

Such a user has no record in `security.json`, even when a local user has the same name. Their
token never gives them that local user's roles or password. For them:

- `SHOW CURRENT USER` shows the token's name and roles.
- `SHOW USER PRIVILEGES` shows the privileges of those roles.
- `ALTER CURRENT USER SET PASSWORD` is refused: the password lives with the provider.

Local users keep working next to the provider: passwords, session tokens and the Web UI's
sign-in. A provider also makes logins required when the server was started with
`DREVO_AUTH=none`. In that case no first local user is created, and the users come from the
provider.

Typical settings:

| Provider | `DREVO_OIDC_ISSUER` | Username claim | Groups claim |
|---|---|---|---|
| Keycloak | `https://<host>/realms/<realm>` | `preferred_username` | `groups` (add the *Group Membership* mapper to the client) |
| Kanidm | `https://<host>/oauth2/openid/<client>` | `preferred_username` | `groups` |
| Auth0 | `https://<tenant>.auth0.com/` (with the trailing slash) | `sub` or `email` | a namespaced claim added by an Action, e.g. `https://drevo/groups` |

The token must be an access token whose `aud` names drevo. With Keycloak, add an
*Audience* mapper; with Auth0, request it for an API whose identifier is
`DREVO_OIDC_AUDIENCE`.

### Audit log

`DREVO_AUDIT_LOG` records security events: `off` (the default), `stdout`, or a file path. A
file gets one JSON object per line, readable by its owner only. It rotates at 10 MiB, and
five old files are kept (`audit.log.1` … `audit.log.5`).

```json
{"time":"2026-10-17T09:12:44.031Z","event":"login_failed","outcome":"failure","user":"ann","client":"10.0.0.7:51234","surface":"bolt","reason":"invalid principal or credentials"}
```

| `event` | When |
|---|---|
| `login_succeeded` | a password login (a reused session token is not logged again) |
| `login_failed`, `login_locked` | a refused login; a user locked after failed logins |
| `logout` | `POST /auth/logout` |
| `password_changed` | `ALTER USER … SET PASSWORD`, `ALTER CURRENT USER SET PASSWORD` |
| `user_changed`, `role_changed`, `privilege_changed` | the other user, role and privilege commands; `target` names what changed |
| `forbidden` | a statement, command or route refused for a missing privilege |

`surface` is `bolt`, `http`, `grpc` or `flight`, and `user` is who acted. An event never
carries a password or token. Set `DREVO_AUDIT_QUERY_TEXT=true` to add the text of a refused
statement as `query`; the text of user and role commands is never recorded. Failed logins,
lockouts and refusals are also logged at WARN, so they show in the Web UI's problem feed
(for admins) even without an audit log.

### TLS

With `DREVO_TLS_CERT` and `DREVO_TLS_KEY` set, the HTTP port serves **HTTPS**, the Bolt
port serves **`bolt+s://`**, and the gRPC and Arrow Flight ports serve gRPC over TLS (ALPN
`h2`), all with the same certificate. Plain-text clients on those ports get no answer. The Web UI's
session cookie is then marked `Secure`. The Docker image is built with TLS (Cargo feature
`bolt-tls`, pure-Rust rustls); a certificate the server cannot read stops it at startup.

```bash
docker run -d --name drevo -p 8443:8080 -p 7687:7687 \
  -v drevo_data:/data -v /etc/drevo/tls:/tls:ro \
  -e DREVO_AUTH=admin/change-me-now -e DREVO_BOLT_PORT=7687 \
  -e DREVO_TLS_CERT=/tls/cert.pem -e DREVO_TLS_KEY=/tls/key.pem \
  ice1x/drevo:latest

curl --cacert /etc/drevo/tls/cert.pem -u admin:change-me-now https://localhost:8443/auth/me
```

Drivers connect with `bolt+s://host:7687`, or `bolt+ssc://` for a self-signed certificate.
For a test certificate:

```bash
openssl req -x509 -newkey rsa:2048 -nodes -days 365 -subj /CN=localhost \
  -addext subjectAltName=DNS:localhost -keyout key.pem -out cert.pem
```

gRPC and Flight clients trust the same certificate:

```bash
grpcurl -cacert /etc/drevo/tls/cert.pem -import-path proto -proto drevo.proto \
  -H "authorization: Basic $(printf 'admin:change-me-now' | base64)" \
  -d '{"query":"MATCH (n) RETURN n.title LIMIT 3"}' \
  localhost:50051 drevo.v1.Drevo/Execute
```

```python
import pyarrow.flight as flight
cert = open("/etc/drevo/tls/cert.pem", "rb").read()
client = flight.connect("grpc+tls://localhost:8815", tls_root_certs=cert)
token = client.authenticate_basic_token("admin", "change-me-now")
table = client.do_get(
    flight.Ticket(b"MATCH (t:Task) RETURN t.title"), flight.FlightCallOptions(headers=[token])
).read_all()
```

---

### Change events (`GET /events`)

`GET /events` streams committed changes as [Server-Sent
Events](https://html.spec.whatwg.org/multipage/server-sent-events.html), so an app refreshes when
the graph changes instead of polling. Each node or relationship that a committed write creates,
changes or deletes is one event, in commit order. Work that was rolled back never appears.

```bash
curl -N 'http://localhost:8080/events?labels=Task,DEPENDS_ON'
```

```text
id: 42
event: node
data: {"op":"upsert","id":7,"uuid":"0192…","labels":["Task"],"cursor":42}

id: 43
event: relationship
data: {"op":"upsert","id":3,"uuid":"0192…","type":"DEPENDS_ON","from":7,"to":8,"cursor":43}

id: 44
event: node
data: {"op":"delete","id":8,"cursor":44}
```

An event names what changed, not its values: read them with a query. In a browser:

```js
const events = new EventSource("/events?labels=Task");
events.addEventListener("node", (e) => refresh(JSON.parse(e.data)));
events.addEventListener("lagged", () => reloadEverything());
```

The Web UI follows this stream when **Live** is ticked in its Graph panel: the overview reloads
shortly after the graph changes, once for a burst of changes. While a query or search result is
on screen it stays put and the **Refresh** button is highlighted instead. The browser remembers the
setting.

#### Named events (`drevo.events.post`)

A write can also say what happened, so clients need not work it out from the changed nodes.
`CALL drevo.events.post(name, payload)` posts a named event, once per row; the payload is any
value but a node, relationship or path. The event waits for the statement, or its explicit
transaction, to commit, then arrives as a `posted` event after the changes it committed. A failed
statement or a rolled-back transaction posts nothing.

```cypher
CREATE (i:Invoice {no: 'INV-7', status: 'approved'})
WITH i
CALL drevo.events.post('invoice_approved', {invoice: i.no}) YIELD name
RETURN i.no, name
```

```text
id: 45:1
event: posted
data: {"name":"invoice_approved","payload":{"invoice":"INV-7"},"cursor":45}
```

```js
const events = new EventSource("/events?names=invoice_approved");
events.addEventListener("posted", (e) => notify(JSON.parse(e.data).payload));
```

#### Stream options

- **Resuming.** The SSE `id` is the stream position: the change cursor, followed by `:<n>` once
  named events have been posted, where `n` counts them. After a reconnect, a client resumes from
  `?since=<id>` or from the `Last-Event-ID` header, which `EventSource` sends by itself, and misses
  nothing. `since` also takes a bare cursor from an event's data. Without either, the stream starts
  at the latest change.
- **`lagged`.** The server keeps the latest 10 000 changes and 10 000 named events for resuming;
  named events are kept in memory only. An older position, or one from before a restart, gets a
  `lagged` event carrying the current cursor: reload, then follow from there.
- **`labels`** keeps nodes with any of the listed labels and relationships of any of the listed
  types. A deletion carries only the id, because what was deleted is gone, so it passes `labels`.
  Deleting a node removes its relationships with it, without an event for each of them.
- **`names`** keeps the named events with any of the listed names. Naming one kind (`labels` or
  `names`) leaves the other out unless it is named too: `?names=invoice_approved` carries only
  those events, `?labels=Invoice&names=invoice_approved` both.
- **`db`** follows another database than the default: `?db=erp`.
- **Logins.** With logins required, the stream needs a login like any other route. It also leaves
  out the nodes and relationships the user may not traverse.

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

# Backup (physical, online) and restore
curl -X POST http://localhost:8080/admin/backup -H 'content-type: application/json' -d '{}'
drevo-server restore --from /data/backups/<name> --to /data-restored

# Read-only: one database, or the whole server
curl -X POST http://localhost:8080/cypher -H 'content-type: application/json' \
     -d '{"query": "ALTER DATABASE erp SET ACCESS READ ONLY"}'
DREVO_READ_ONLY=true drevo-server

# Full-graph export / import (JSON)
curl -s http://localhost:8080/export/json > graph.json
curl -X POST http://localhost:8080/import/json -H 'content-type: application/json' \
     -d "{\"dump\": $(cat graph.json)}"
```
