# Changelog

All notable changes to drevo are recorded here. This complements the git
history and the in-repo issue tracker; format loosely follows
[Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Fixed

- **`scripts/drevo-restart.sh` did not forward `DREVO_TEXT2CYPHER_*`**, so a
  text-to-Cypher upstream set in `~/.drevo.env` never reached the container
  and the Web UI's **Ask** reported "not configured". The three variables
  are forwarded like the embeddings ones.

- **`MATCH … ORDER BY … LIMIT k` held every matched row until the sort**
  (issue #743). Two such scans over 240,451 road junctions, each returning
  six rows, took up to 183 MB of transient heap that the allocator then kept
  as RSS. The MATCH now holds only the `SKIP + LIMIT` rows that sort first
  (ties in scan order, as before) and the clause after it sorts those. The
  rows are the same; on 50,000 nodes the peak for `… ORDER BY n.i DESC
  LIMIT 6` is a few MB instead of about 40.

- **A binary log reopened about twice as slowly as the same log as JSON**
  (issue #740). `cbor`, `msgpack` and `cbor-zstd` records were read into a
  `serde_json::Value` tree and converted from there; they are now read and
  written straight from the ops. A road network's log of 518,828 records
  reopens in 1.33 s as `cbor` (was 2.35 s; 1.25 s as `json`) and 1.10 s as
  `msgpack`; a knowledge graph's `cbor` log in 208 ms (was 291 ms). Logs
  written before and after read each other. `cbor-compact` keeps the tree
  its key transform works on and is unchanged.

- **Reading a `cbor-zstd` log of small records took twice as long as
  `cbor`**: a zstd context was created for every record. One context per
  thread is reused, and a record's buffer is sized from its frame header;
  the road network's log now reopens in 2.5 s against 2.4 s as `cbor`
  (was 4.6 s).

- **The Bolt idle timeout counted a running statement as idle time** (issue
  #730). `DREVO_IDLE_SESSION_TIMEOUT_MS` was measured from when a message
  arrived, so after a statement longer than the limit a client that took a
  few milliseconds before its next message had its connection closed and its
  transaction rolled back. Idleness is now measured from the last reply.

- **An aggregation inside another expression was refused** (issue #729).
  `size(collect(x))`, `toFloat(sum(x)) / count(x)`, `collect(x)[0]`,
  `collect(x)[0..3]`, `{c: count(*)}`, `[y IN collect(x) WHERE …]`,
  `any(y IN collect(x) WHERE …)` and `reduce(s = 0, y IN collect(x) | …)`
  failed with "unsupported Cypher feature"; the aggregation is now folded over
  the group and the expression evaluated around it, as in Neo4j.

- **Every write transaction copied the whole graph's adjacency** (issue
  #716). A transaction's copy of the store now shares the adjacency lists
  (in chunks of 64 nodes), embeddings and the kind / title indexes with the
  committed store until it writes them: one `SET` in an `execute_write` on
  a 112k-node, 210k-relationship graph went from +22 MB and 17 ms to
  +0.1 MB and 0.3 ms, with the store's own size unchanged.
- **`MATCH … WITH n LIMIT k` built every match first** (issue #714). A
  `MATCH` whose rows the next `WITH` / `RETURN` cuts to a constant `LIMIT`
  (no `ORDER BY`, `DISTINCT`, aggregation or `WITH … WHERE`) now stops once
  it has enough, and a one-node pattern projects only the nodes it keeps:
  `MATCH (n) WHERE n.c = 1 WITH n LIMIT 2000` on 50,000 nodes peaks at
  1.3 MB instead of 31 MB — the delete loop importers run (`… LIMIT 2000
  DETACH DELETE n`).
- **Memory not given back after deleting and rewriting a large graph**
  (issue #711). The Docker image sets `MALLOC_ARENA_MAX=2`: with one glibc
  arena per busy thread, the memory a delete and rewrite freed stayed
  resident (265 MB → 398 MB on a 112k-node road graph, 198 MB after a
  restart); with two arenas the process settles at 219 MB. drevo's own live
  heap returns to the graph's size. Set the variable too when running the
  binary on Linux outside the image.
- **`MATCH` by an indexed property inside a transaction scanned every
  node** (issue #686). In an explicit transaction (a driver's
  `execute_write`), and in auto-commit statements run as one transaction
  (triggers, existence constraints, restricted users), `MATCH (n:L {key:
  r.x})` read all nodes per row: 30 ms a row on 20,000 nodes. The label,
  property, path and text indexes now serve these statements whenever
  nothing was committed since the transaction began, completed with the
  nodes the transaction wrote; 300 `UNWIND` rows on 20,000 nodes take
  milliseconds instead of seconds.
- **Relationships lost in a fresh database** (issue #708). In a database
  that had never held relationships, the first ones created between nodes
  far apart in the id space (about 100 of 2,000 with 112,000 nodes) were
  written to the log but not visible: not from either endpoint, not by id,
  not after a restart. The id-indexed tables behind the adjacency lists
  hid an entry first stored as an outlier once the table grew past its id.
  The log always held every relationship, so restarting on a fixed build
  shows them all again — an importer that re-created the missing ones as
  a workaround will then see those twice.

### Changed

- **A prepared two-phase-commit transaction no longer pauses every write**
  (issue #749, slice 4). While a transaction is prepared, a Cypher statement
  or explicit transaction now waits (the same retryable
  `Neo.TransientError.Transaction.LockAcquisitionTimeout` / HTTP 503) only if
  it writes a node or relationship the prepared one wrote, links to a node it
  deleted, deletes a node its relationships need, or would break a declared
  constraint once it commits. Writes on other records commit as usual, and
  resolving the prepared transaction still cannot fail. Direct REST and
  embedded engine writes still wait whatever they touch.

- **Explicit transactions that touch different records no longer roll each
  other back** (issue #749). The default isolation is now *snapshot*: a
  transaction commits unless a transaction that committed after it began
  wrote a node or relationship it also wrote, or deleted an endpoint of a
  relationship it wrote; its writes are then applied on top of the others'.
  Before, any commit in between was a conflict. Eight writers running ten
  transactions each on different nodes no longer need retries. Updates are
  never lost and unique / existence constraints are checked on the combined
  result, but *write skew* is now possible: two transactions that each read
  what the other writes can both commit. To keep the old rule, set
  `DREVO_TX_ISOLATION=serializable`, or send `drevo.isolation: serializable`
  in one transaction's metadata (Python driver:
  `@unit_of_work(metadata={"drevo.isolation": "serializable"})`). Node and
  relationship ids now come from one counter shared by all transactions, so
  a rolled-back transaction leaves a gap in the ids. New metric
  `drevo_tx_merged_commits_total`; databases created with `CREATE DATABASE`
  now inherit the conflict-history size and the isolation.

- **An unsupported Cypher form is a client error over Bolt** (issue #733).
  An unknown function or an aggregation where none is allowed (for example
  in the body of a list comprehension) now fails with
  `Neo.ClientError.Statement.UnsupportedOperationError` instead of
  `Neo.DatabaseError.General.UnknownError`, and the message reads
  ``function call `triage.score` is not supported`` with no roadmap note.
  `ExecError::Unsupported` no longer has a `task` field.

- **Weighted path search reads relationships in place** (issue #684).
  `drevo.path.weighted` no longer copies every relationship record it
  visits: the new `GraphEngine::for_each_edge_of` (and
  `GraphSnapshot::for_each_edge_of`) hands each one by reference. On the
  Sofia road network a route is 11–23% faster (driving A* 90 → 80 ms,
  walking Dijkstra 137 → 105 ms), and reading every drivable junction's
  relationships takes 5 ms instead of 68 ms.
- **A writing auto-commit statement is one transaction** (issue #715).
  Its writes go to the log as one record with one fsync, and a statement
  that fails — an error, the statement timeout, `TERMINATE TRANSACTIONS` —
  leaves none of them, as in Neo4j (before, the writes made before the
  failure stayed). On a durable store, creating 2,000 nodes in one
  statement went from 11.5 s to 22 ms and deleting 2,000 from 16.6 s to
  43 ms. Concurrent auto-commit writers take turns.
- **drevo-core 0.2: `Properties` is a compact map** (issue #687). It is no
  longer a public `HashMap<String, serde_json::Value>` newtype: entries are
  a vector sorted by name and names are shared across every record, which
  cuts the heap of the restarted Sofia road graph by 11% (RSS by 14%). The
  map keeps the familiar methods (`get`, `insert`, `remove`,
  `contains_key`, `iter`, `keys`, `values`, `len`, …) with `&str` keys;
  build one with `Properties::new()`, `Properties::from(hash_map)`,
  `Properties::from([("k", value)])` or `collect()`. Serialized forms (WAL,
  dumps, HTTP) are unchanged.

### Added

- **Every node within a cost budget: `CALL drevo.path.reachable(start,
  {maxCost, …}) YIELD node, cost, path`** (issue #763). One Dijkstra search
  from `start` returns each node it reaches within `maxCost`, at its least
  cost, cheapest first — isochrones, and the walk from a place to every
  nearby stop. It takes the `relTypes`, `direction` and `cost` options of
  `drevo.path.weighted`, plus `targets: {label}` (return only those nodes,
  search through all), `limit` and `path: true`. Also
  `NativeService::reachable` and `drevo::weighted_path::reachable`.
- **Public-transport routing: `CALL drevo.path.timetable(starts, ends,
  options) YIELD arriveAt, departAt, transfers, legs`** (issue #762). It
  plans "leave at 08:10 from near here, arrive there as early as possible"
  over a timetable stored in the graph: hop relationships between
  consecutive stops carry `dep` / `arr` / `trip` / `svc` lists, one entry per
  trip, and footpaths carry seconds. It returns one journey per number of
  transfers that arrives earlier than every journey with fewer, with its
  legs (access, rides with their hops, walks, egress). Options: `departAt`,
  service `days` with an offset (yesterday's trips past midnight),
  `minTransfer`, `maxTransfers`, `maxDuration`, and the relationship and
  property names. The native call is `NativeService::timetable`. On a
  synthetic city the size of Sofia's weekday feed, see
  `benches/timetable_bench.rs`.

- **Slow statements are logged** (issue #752). With `DREVO_SLOW_QUERY_MS`
  set, every statement that runs longer is logged once at WARN (target
  `drevo::slow_query`) with its duration, text, parameter names (never
  values), protocol, user and database, in auto-commit mode and in explicit
  transactions alike. It shows in the problem feed and the Web UI's
  notifications and is counted in `drevo_slow_statements_total`. The
  threshold can be changed at runtime from the Settings panel or
  `POST /config/slow-query`. Off by default.

- **An explicit transaction has a write limit** (issue #751). A transaction
  keeps every write in memory until it commits, so one `BEGIN` that never
  commits could grow the server until the OS killed it. `DREVO_MAX_TX_WRITES`
  (default 1,000,000; `0` = no limit) caps the writes one transaction may
  buffer. The statement that goes over fails with Neo4j's
  `Neo.ClientError.General.TransactionOutOfMemoryError` (HTTP 507, gRPC
  `RESOURCE_EXHAUSTED`, drevo-py `TransactionTooLarge`), which drivers do not
  retry, and the transaction is rolled back. `SHOW TRANSACTIONS` gains a
  `bufferedWrites` column, and drevo-py gets `Drevo.set_max_tx_writes()`.
  Auto-commit statements are not limited.

- **Rejected transaction commits are classified and counted** (issue #749,
  slice 1). Each database keeps the records the newest `DREVO_TX_CONFLICT_HISTORY`
  (default 10,000) commits wrote, deleted and depend on, and when an explicit
  transaction fails with `Transaction.Outdated` the engine records whether a
  commit it did not see touched the same nodes or relationships (`overlap`),
  none did (`would_pass`) or the transaction outlived the history (`history`).
  `drevo_tx_conflicts_total{reason}` on `/metrics` exposes the counts. The
  decision itself is unchanged: this slice measures how much a per-record rule
  would gain before one lands (design: `docs/rfc-per-entity-conflicts.md`).

- **Connection limits** (issue #750). A server serves at most
  `DREVO_MAX_CONNECTIONS` Bolt connections (default 512) and
  `DREVO_HTTP_MAX_IN_FLIGHT` HTTP requests (default 256) at once. A Bolt
  connection over the limit is refused at `HELLO` with
  `Neo.TransientError.Request.NoThreadsAvailable`, which the Neo4j drivers
  retry; an HTTP request over it gets `503` with `Retry-After: 1`, while
  `/health`, `/ready` and `/metrics` are always served. New metrics
  `drevo_connections_rejected_total{protocol}` and
  `drevo_connections_active{protocol}`; `0` turns a limit off.

- **Cypher reference: a _Concurrency_ section.** What happens when two
  transactions overlap, in one place: every statement and transaction reads
  one snapshot; writers never wait, so there are no deadlocks; an explicit
  transaction that commits after another one fails with the retryable
  `Neo.TransientError.Transaction.Outdated` (Python `TransactionConflict`)
  and the drivers' managed transactions retry it; auto-commit statements are
  retried by the server. With a "coming from PostgreSQL" table (`40P01`,
  `statement_timeout`, `pg_terminate_backend`, …) and a pointer from the
  admin guide's timeouts section.

- **The text-to-Cypher provider is set at runtime** (issue #747). The Web
  UI's Settings panel has a *Text-to-Cypher (Ask)* section — chat completions
  URL, write-only API key, model — backed by `GET`/`POST /config/text2cypher`.
  A save is kept in `<data dir>/text2cypher_config.json` (owner-only, sealed
  under encryption at rest, included in server backups and rekeyed with the
  other files), wins over `DREVO_TEXT2CYPHER_*` at the next start, and
  `drevo.cypher.fromText` uses it from the next call with no restart.

- **Admin guide: Memory footprint.** What a graph costs in memory, measured
  on a 524,368-entity road network: 512 B per entity for the graph itself
  and 1,489 B with every secondary index, the record layout behind those
  numbers, and the three things to do when a graph has to fit a small box.

- **`DREVO_WAL_FORMAT=cbor-compact` and `cbor-compact-zstd`** (issue #738).
  CBOR records whose built-in field names (`UpsertNode`, `id`, `kind`,
  `created_at`, …) are small integers; property names stay text. A road
  network's log of 518,828 small records went from 91.7 MB as `cbor` to
  59.2 MB; a knowledge graph's log of embeddings from 12.8 MB as
  `cbor-zstd` to 12.1 MB as `cbor-compact-zstd`. Same framing, checksums,
  recovery rules, compaction and encryption as the other binary formats;
  needs only the `format-cbor` feature (`wal-zstd` for the compressed one).

- **`DREVO_WAL_FORMAT=cbor-zstd`** (issue #688). CBOR records compressed one
  by one with zstd, behind the `wal-zstd` feature (in the Docker image). On
  a knowledge graph with 2,152 embeddings the compacted log measured 73.8 MB
  as JSON, 19.5 MB as CBOR and 12.8 MB as `cbor-zstd`; reopening took 195,
  259 and 277 ms. A compressed log keeps the framing, checksums and
  recovery rules of the other binary formats, works under encryption at
  rest, and is converted to or from any other format by compaction.

- **Admin guide: Upgrading a server.** The steps to move a data directory
  to a new image: copy it, choose `DREVO_AUTH` before the first start (an
  image that requires a login otherwise creates `neo4j` with a generated
  password and refuses clients without credentials), read the first-start
  log, compare the node count.

- **`elementId()`** (issue #707). `elementId(n)` / `elementId(r)` return
  the id as a string, the `element_id` Bolt 5 drivers see, and
  `MATCH (n) WHERE elementId(n) = $e` (or `IN $ids`) is a lookup by id.
- **`SHOW CONNECTIONS` / `TERMINATE CONNECTIONS`** (issue #669). Lists the
  open Bolt connections (with the driver's user agent and the logged-in
  user, idle ones included) and the Web UI's signed-in sessions;
  terminating closes a Bolt connection and rolls back its transaction, or
  logs a Web UI session out. Users see their own; admins see all.
  `SHOW TRANSACTIONS` now names the real `connectionId`.

- **TLS for the gRPC API and Arrow Flight** (issue #655). With
  `DREVO_TLS_CERT` / `DREVO_TLS_KEY` set, `DREVO_GRPC_PORT` and
  `DREVO_FLIGHT_PORT` serve gRPC over TLS (ALPN `h2`) with the HTTPS
  certificate, so `authorization` headers and session tokens no longer
  travel in plain text; a plain-text client gets no answer. In Rust:
  `grpc::serve_tls_with_security`, `flight::serve_tls_with_security`,
  `rpc_tls::incoming`.
- **Bolt 5.0–5.4** (issue #651). The handshake takes version ranges and
  picks the highest version both sides speak, so Neo4j 5.x drivers get
  Bolt 5.4; 4.4 / 4.3 clients are unchanged. Nodes and relationships carry
  `element_id`, `LOGON` / `LOGOFF` log a connection in and out (so
  `driver.verify_authentication()` and per-session `auth` work), and
  `TELEMETRY` is acknowledged.
- **Per-transaction and idle timeouts** (issue #646). Bolt honours the
  `tx_timeout` Neo4j drivers send on `BEGIN` and auto-commit `RUN`, and
  `POST /cypher` takes `timeout_ms`. Past it the statement stops and an
  explicit transaction is rolled back with
  `Neo.ClientError.Transaction.TransactionTimedOutClientConfiguration`;
  `DREVO_QUERY_TIMEOUT_MS` caps the client's value.
  `DREVO_IDLE_SESSION_TIMEOUT_MS` closes a Bolt connection that holds an
  open transaction and sends nothing, rolling the transaction back.
- **Encryption at rest on the server** (issues #645, #701). Built with the
  `encryption` feature (now in the Docker image), `drevo-server` encrypts
  every database's log and sidecars, `database_access.json`, the users
  (`security.json`), the embeddings settings (`embeddings_config.json`) and
  physical backups with the 256-bit key from `DREVO_ENCRYPTION_KEY_FILE` (or
  `DREVO_ENCRYPTION_KEY`). `drevo-server encrypt`, `decrypt` and
  `rotate-key --new-key-file FILE` convert a stopped server's data in place;
  in Rust, `NativeGraph::reencrypt`. Compared with the clear, one durable
  write costs about the same, a 1,000-node batch +10%, reopening a store
  +107%. Nothing in the data directory stays in the clear.
- **Encrypted sidecars in drevo-core** (issue #645). An encrypted store
  also seals `origin.json`, `tombstones.json`, the stamp log,
  `sequences.json` and its backup copies.
- **Encrypted write-ahead log in drevo-core** (issue #645, first part).
  With the `encryption` feature, `NativeGraph::open_durable_encrypted(path,
  format, key)` seals every log record with XChaCha20-Poly1305 under a
  caller-supplied `drevo_core::encryption::Key`: the log starts with a key
  check, so a wrong or missing key fails the open (never read as a torn
  tail), a changed record is refused, and compaction, transactions and
  backups stay encrypted.
- **Sequences** (issue #644). `CALL drevo.sequence.create('bug', {start: 1000,
  step: 1})`, `drevo.sequence.drop`, `drevo.sequence.list`, and the functions
  `drevo.sequence.next(name)` / `drevo.sequence.current(name)`: unique,
  increasing numbers handed out outside transactions, so writers never
  conflict over them; a rollback leaves a gap. Kept in `sequences.json` next to
  the log with block reservation, so a restart never reissues a value; included
  in backups. In Rust: `NativeGraph::sequences()` / `sequence_next`
  (`drevo_core::sequence`).
- **Lean databases** (issue #687). `CALL drevo.indexes.setSecondary(false)`
  drops a database's property-value index, value cache and full-text indexes
  and frees their memory; queries stay correct (property lookups scan,
  full-text procedures are unavailable) and the choice survives a restart.
  `setSecondary(true)` rebuilds them, `drevo.indexes.secondary()` reports
  it. The Sofia road graph after a restart: ~1 GB → ~450 MB.
- **Triggers** (issue #643). `CALL apoc.trigger.install(db, name, statement,
  {phase: 'before'|'after'|'afterAsync'}[, {params}])`, `apoc.trigger.list`,
  `stop` / `start`, `drop` / `dropAll`, and the APOC 4 `add` / `remove` /
  `removeAll` / `pause` / `resume`. The statement runs on every commit that
  changes something and sees `$createdNodes`, `$deletedNodes`,
  `$assignedNodeProperties`, `$removedLabels`, … as in APOC. A failing `before`
  trigger rolls the transaction back (Bolt
  `Neo.ClientError.Transaction.TransactionHookFailed`); a trigger's own writes
  do not fire triggers. Kept per database in `semantic.json`. Procedure names
  may now use keywords after a dot (`apoc.trigger.add`).
- **Weighted paths for routing** (issue #680). `CALL
  drevo.path.weighted(start, end, {relTypes, direction, cost, heuristic,
  maxCost}) YIELD path, cost` returns the path with the lowest summed cost of
  a numeric relationship property (`car_s`, `walk_s`, …), over several
  relationship types and one direction, once per input row. With
  `heuristic: {lat, lon, maxSpeedMps}` it is an A* search, otherwise a
  bidirectional Dijkstra; `maxCost` stops it at a budget, and a negative cost
  is an error. `NativeService::weighted_path` and
  `drevo::weighted_path::weighted_path` do the same in Rust.
- **Web UI: read-only databases** (issue #642). The database picker labels
  read-only databases and shows a `read-only` badge for the active one;
  Shrink is turned off there, and New database on a read-only server.
  `GET /databases` reports `read_only_server`.
- **Read-only databases** (issue #642). `ALTER DATABASE <name> SET ACCESS
  READ ONLY | READ WRITE` switches one database and is remembered across
  restarts (`<data_dir>/database_access.json`). `DREVO_READ_ONLY=true` serves
  every database read-only and writes nothing in the data directory, so it
  can be mounted read-only. Writes and schema changes fail with
  `Neo.ClientError.General.ForbiddenOnReadOnlyDatabase` (HTTP 403);
  `SHOW DATABASES` gains an `access` column and `GET /databases` an `access`
  map.
- **drevo-core: read-only stores** (issue #642).
  `NativeGraph::open_read_only(path)` replays a durable store without ever
  writing to it: no sidecar is created or rewritten and a torn tail stays in
  place, so it opens on read-only media or in a directory the process cannot
  write to. Read-only handles share a store; a writer and a read-only handle
  exclude each other. `set_read_only(true)` refuses writes on a writable
  store until it is switched back. Every write fails with
  `CoreError::ReadOnly` (and `ReadOnly` on the commit, prepare and resolve
  errors); the server answers it with HTTP 403 and Bolt
  `Neo.ClientError.General.ForbiddenOnReadOnlyDatabase`.
- **Web UI: physical backups** (issue #641). The Storage panel lists the
  newest backups (when, full or incremental, databases, size) and takes a
  full backup, or an incremental one that continues the newest.
- **Physical server backups** (issue #641). `POST /admin/backup` backs up
  every database, its schema sidecar, the users and the embeddings
  settings while the server keeps serving; `{"since": "<earlier>"}` makes it
  incremental per database. `GET /admin/backups` lists them;
  `DREVO_BACKUP_DIR` sets where they go (`<data_dir>/backups` by default).
  `drevo-server backup --to DIR [--since DIR]` backs up a stopped server and
  `drevo-server restore --from DIR … --to DATA_DIR` restores, checking every
  checksum and the chain first.
- **drevo-core: online physical backups** (issue #641).
  `NativeGraph::backup_to(dir, options)` copies the write-ahead log and its
  sidecars at a commit boundary while the store keeps serving, with a
  `manifest.json` of sizes and SHA-256 checksums. With `since`, only the log
  bytes written after that backup are copied; after a compaction the backup
  is taken in full and the manifest says why. `backup::restore(chain, wal)`
  checks every file and the chain, then writes a new store.
- **Change events over Server-Sent Events: `GET /events`** (issue #640).
  - Every node or relationship a committed write creates, changes or
    deletes is one event, in commit order. Rolled-back work never appears.
  - The SSE id is the change's cursor, so a client resumes from `?since=`
    or `Last-Event-ID` (which `EventSource` sends) without gaps.
  - A cursor older than the retained 10 000 changes, or one from before a
    restart, gets a `lagged` event telling the client to reload.
  - `labels=` narrows the stream by label or relationship type, and `db=`
    picks the database.
  - With logins required, the stream leaves out what the user may not
    traverse.
  - `CALL drevo.events.post(name, payload)` posts a named application
    event, once per row, delivered as a `posted` event when its statement
    or transaction commits; `names=` picks events by name. Stream
    positions read `<cursor>:<n>` once named events exist.
- **Web UI: Live overview** (issue #640). Ticking **Live** in the Graph
  panel reloads the overview when the graph changes (from any client),
  once per burst of changes. A query or search result on screen stays put
  and **Refresh** is highlighted instead. The setting is remembered per
  browser.

- **`SHOW TRANSACTIONS` and `TERMINATE TRANSACTIONS`** (issue #639).
  - The server lists every running statement and open explicit
    transaction, on every database, under Neo4j's ids
    (`drevo-transaction-42`). Each entry shows who runs it (user, client,
    protocol), the current query, and its start and elapsed time.
  - `TERMINATE TRANSACTIONS 'id', …` stops a statement at its next
    cancellation check with `Neo.ClientError.Transaction.Terminated`.
    An explicit transaction refuses its next statement and its commit, and
    is rolled back.
  - Over Bolt, HTTP, gRPC and Arrow Flight.
  - With logins required, users manage their own; `TRANSACTION MANAGEMENT`
    covers others'.
  - The Web UI's Storage panel lists them, refreshed every few seconds,
    with a Terminate button on each.

### Fixed

- **A long statement right after a write no longer holds up every other
  statement.** The first statement to see the indexes behind the graph
  brought them up to date and then ran under the same exclusive lock, so
  every other query waited for it. It now releases the lock before
  running.

- **Sign in with an identity provider** (issue #638, feature `oidc`, in the
  Docker image). With `DREVO_OIDC_ISSUER` and `DREVO_OIDC_AUDIENCE` set, the
  server accepts an OpenID Connect access token where it accepts a password:
  - HTTP `Authorization: Bearer`;
  - the Bolt `bearer` scheme (`neo4j.bearer_auth`);
  - gRPC and Arrow Flight `authorization` metadata.

  The provider's keys come from its discovery document over HTTPS.
  `DREVO_OIDC_USERNAME_CLAIM` (default `sub`) names the user, and
  `DREVO_OIDC_ROLES_CLAIM` (default `groups`) with `DREVO_OIDC_ROLE_MAP`
  (`admins=admin,devs=editor`) gives the roles. Such a user has no record in
  `security.json` and never gets a local user's privileges or password, even
  under the same name. `SHOW CURRENT USER` and `SHOW USER PRIVILEGES` answer
  from the token's roles. A refused token is audited with its reason. A
  provider makes logins required even with `DREVO_AUTH=none`. drevo-auth
  gains `PrivilegeFilter::Roles` and `AuthManager::security_context_for_roles`.

- **OIDC token validator in drevo-auth** (issue #638, feature `oidc`).
  `TokenValidator` checks a provider's JWT: an RS256 or ES256 signature
  against the provider's key set, then `iss`, `aud` (a string or a list),
  `exp` and `nbf`, with 60 s leeway. It refuses `none`, HMAC and other
  algorithms. Keys are cached by `kid`, and an unknown `kid` fetches the set
  again, at most once a minute. `JwksSource` supplies the key set, so the
  crate itself makes no network calls. `ClaimMapping` takes the user name
  from a claim and maps the groups in a list claim to roles
  (`parse_role_map("admins=admin,devs=editor")`), giving an `OidcPrincipal`
  with no local user record.

- **Security audit log** (issue #637). `DREVO_AUDIT_LOG=off|stdout|<path>`
  records the following, with time, user, client address, protocol, target,
  outcome and reason:
  - password logins, failed logins and lockouts;
  - logouts;
  - password changes;
  - user, role and privilege changes;
  - refusals for a missing privilege.

  A file is written as JSON lines, readable by its owner only, rotated at
  10 MiB with five old files kept. Events never carry a password or token.
  `DREVO_AUDIT_QUERY_TEXT=true` adds a refused statement's text, but never
  that of a user or role command. Failed logins and refusals also reach the
  Web UI's problem feed. drevo-auth gains the `audit` module (`AuditEvent`,
  `AuditSink`, `JsonLinesFile`, `Stdout`, `MemorySink`).

- **REST routes need the privileges they bypass** (issue #636). With logins
  required, the routes outside `/cypher` (which serve the graph unfiltered)
  answer `403 Neo.ClientError.Security.Forbidden` unless:
  - graph reads (`/export/…`, `/nodes…`, `/edges`, `/facets`,
    `/paths/shortest`, `/search/fts`): the user reads the whole graph;
  - REST writes and `/import/json`: the user reads and writes the whole
    graph;
  - creating or dropping a database: `DATABASE MANAGEMENT`;
  - server administration (`/storage/…`, `/config/…`,
    `/transactions/prepared…`, `/problems`, `/report`): `ALL DBMS
    PRIVILEGES`.

  The Web UI stops showing problem notifications to non-admins.

- **Reads filter by privilege** (issue #636). As in Neo4j, a node the
  signed-in user may not traverse is not matched, expanded, counted
  (`count(*)` included), returned, or found by search, vector and memory
  procedures. A relationship is hidden when its type or either of its nodes
  is. A property without `READ` reads as `null` and does not match in
  `WHERE` or patterns. `SHOW DATABASES` lists only accessible databases.

  Roles without graph denies (`reader`, `editor`, `admin`) skip filtering at
  no measurable cost. A role with denies is answered from a per-statement
  memo, about 1.6× on a 20 000-node scan where every node is filtered; see
  `benches/privilege_bench.rs`. Graph algorithms filter only their results,
  and the REST routes outside `/cypher` are not filtered yet.

- **Privileges are enforced on writes, schema, procedures and database access**
  (issue #636). When the server requires a login, every statement on Bolt,
  HTTP, gRPC and Flight runs with the signed-in user's privileges. A missing
  privilege fails the statement with `Neo.ClientError.Security.Forbidden`
  (HTTP 403, gRPC `PERMISSION_DENIED`) and nothing of it is written. Checked:
  - `ACCESS` to the database, and `DATABASE MANAGEMENT` for `CREATE DATABASE`;
  - `CREATE`, `DELETE`, `SET PROPERTY`, `SET LABEL` and `REMOVE LABEL`;
  - `INDEX` / `CONSTRAINT MANAGEMENT` for schema commands;
  - `EXECUTE PROCEDURE` for every `CALL`, and `EXECUTE ADMIN PROCEDURES` for
    the two-phase-commit resolvers.

  A writing statement of a user who may not write everywhere runs as one
  transaction. `NativeService::execute_as` / `execute_in_tx_as` take a
  `Restriction`, and `Security::context_for` caches each user's compiled
  context until roles change. Reads are not narrowed yet.
  - **Upgrading:** users created with `CREATE USER` hold only `PUBLIC`, so they
    can no longer write until given a role such as `editor`
    (`GRANT ROLE editor TO …`). Users from before roles existed were made
    `admin` when the catalog was upgraded. Clients such as drevo-mcp need a
    user with the roles their work needs.

- **`SecurityContext` in drevo-auth** (issue #636). A session's privileges
  are compiled once from the user's roles, and each check is answered from a
  cache:
  - `can_access`, `can_traverse_node`, `can_traverse_relationship` and
    `can_read_property`;
  - `can_write` (create, delete, labels, properties), `can_database`,
    `can_execute_procedure` / `can_execute_function` and `has_dbms`;
  - `reads_everything` / `writes_everything`, so roles such as `reader` and
    `editor` can skip filtering.

  `SecurityContext::unrestricted()` checks nothing.
  `AuthManager::security_context(user, home)` builds a user's context, and
  `AuthManager::generation()` moves on with every change, so a cached
  context can tell it is stale.

- **Role and privilege commands** (issue #635). These are Cypher over HTTP and Bolt, in
  auto-commit mode:
  - roles: `CREATE ROLE … [IF NOT EXISTS] [AS COPY OF …]`, `DROP ROLE`,
    `RENAME ROLE`, `GRANT ROLE[S] … TO …`, `REVOKE ROLE[S] … FROM …` and
    `SHOW [ALL | POPULATED] ROLES [WITH USERS]`;
  - privileges: `GRANT` / `DENY` / `REVOKE [GRANT | DENY]` on graphs,
    databases and the DBMS;
  - listing: `SHOW [ALL | ROLE r | USER [u]] PRIVILEGES [AS [REVOKE]
    COMMANDS]` and `SHOW SUPPORTED PRIVILEGES`.

  Running the `AS COMMANDS` output on a server with the same roles recreates
  the privileges. drevo-auth gains `Entry::to_revoke_command` and
  `Action::ALL`. The privileges are not enforced yet.

- **Roles and Neo4j-style privileges in drevo-auth** (issue #635). The new
  `privileges` module holds Neo4j's privilege model, with deny overriding
  grant:
  - graph privileges: `TRAVERSE`, `READ {…}`, `MATCH {…}`, `CREATE`, `DELETE`,
    `SET LABEL`, `REMOVE LABEL`, `SET PROPERTY {…}`, `MERGE {…}`, `WRITE` and
    `ALL GRAPH PRIVILEGES`, for nodes, relationships or elements;
  - database privileges: `ACCESS`, `START`, `STOP`, and index, constraint,
    name and transaction management;
  - DBMS privileges: user, role, privilege and database management, and
    `EXECUTE PROCEDURE`, `EXECUTE FUNCTION` and `EXECUTE ADMIN PROCEDURES`.

  The module parses `GRANT` / `DENY` / `REVOKE` commands, writes privileges
  back as commands, and decides requests. The user catalog (format 2) holds
  roles with their privileges, starting with Neo4j's built-in roles and
  their default privileges. Users have roles; `AuthManager` manages roles,
  role assignments and privileges, and decides requests per user. `SHOW
  USERS` lists each user's roles.
  - **Upgrading:** a format-1 `security.json` is upgraded on the first
    start, and its users get `admin`. Older drevo versions cannot read the
    upgraded file.

- **TLS for HTTP and Bolt** (issue #634). `DREVO_TLS_CERT` and `DREVO_TLS_KEY`
  (PEM) turn the HTTP port into HTTPS and the Bolt port into `bolt+s://`; the
  Web UI's session cookie is then `Secure`. Setting only one of the two, or a
  certificate the server cannot read, stops it at startup. `DREVO_BOLT_PORT`
  is now part of the server configuration. The Docker image is built with
  `bolt-tls`. New `drevo::https::TlsListener` serves an axum router over TLS.

- **gRPC and Arrow Flight login** (issue #634). With user management on,
  gRPC `Execute` and Flight `DoGet` need `authorization` metadata — `Basic`
  (user:password) or `Bearer <session token>` — else `UNAUTHENTICATED`
  (`RESOURCE_EXHAUSTED` after repeated failures, `PERMISSION_DENIED` for a
  user who must change the password). Flight `Handshake` trades a password
  for a session token (pyarrow `authenticate_basic_token`). gRPC `Health`
  needs no login.
  - **Upgrading:** gRPC and Flight clients of a server with logins need
    credentials too.

- **HTTP login and Web UI sign-in** (issue #634). With user management on,
  every HTTP request outside `/`, `/health`, `/ready`, `/status`, the Web UI's
  files and `POST /auth/login` needs `Authorization: Basic`, `Bearer <session
  token>` or the `drevo_session` cookie (HttpOnly, SameSite=Strict), else
  `401` with a `Bearer` challenge (no browser login box). `POST /auth/login`,
  `POST /auth/logout` and `GET /auth/me` manage the session; the Web UI shows
  a sign-in screen, the signed-in user with **Sign out** and **Change
  password**, and the forced password change. A user who must change the
  password reaches only that change (`403 CredentialsExpired`). drevo-auth:
  `Credentials::from_http_authorization`.
  - **Upgrading:** scripts calling the HTTP API (e.g. a restart script that
    probes `/v1/embeddings`) need credentials too, or `DREVO_AUTH=none`.

- **Bolt login** (issue #634). With user management on, a Bolt `HELLO` must
  log in as one of the server's users, with a password or a session token,
  unless `DREVO_AUTH=none`:
  - Refused logins get `Neo.ClientError.Security.Unauthorized` with one
    message for a wrong password, an unknown user and a suspended user.
    Three failures in a row lock the user for five seconds
    (`AuthenticationRateLimit`).
  - A user who must change the password can only run `ALTER CURRENT USER`
    until they do (`CredentialsExpired`). `SHOW CURRENT USER` and
    `ALTER CURRENT USER` act on the logged-in user.
  - drevo-auth: `AuthManager::authenticate`, session tokens with idle and
    total lifetimes (`SessionPolicy`), `LockoutPolicy`, `Principal`,
    `LoginError` and a `Clock` trait. Changing a password, suspending or
    dropping a user ends their sessions.
  - **Upgrading:** a server started without `DREVO_AUTH` now requires a Bolt
    login. Set `DREVO_AUTH=user/password` (or `none`) before upgrading.

- **Users** (issue #633). A server built with `auth` (the Docker image is)
  keeps its users in `<data dir>/security.json` and manages them with Neo4j's
  Cypher commands over HTTP and Bolt: `CREATE USER`, `ALTER USER`,
  `DROP USER`, `SHOW USERS`, `SHOW CURRENT USER` and
  `ALTER CURRENT USER SET PASSWORD`.
  - `DREVO_AUTH` creates the first user like `NEO4J_AUTH`: `user/password`,
    `none`, or unset for `neo4j` with a generated password that is logged
    once and must be changed.
  - Passwords are Argon2id-hashed, at least 8 characters, and never appear in
    results, errors or logs. The file is written atomically, readable by its
    owner only, and a damaged one stops the server.
  - drevo-auth: `catalog` (`Catalog`, `CatalogStore`, `FileStore`,
    `MemoryStore`) and `users` (`AuthManager`, `NewUser`, `AlterUser`,
    `PasswordPolicy`, `Bootstrap`).
  - These commands manage users only; logging in is not yet required.

- **`drevo-auth` crate** (issue #647). Authentication and role-based access
  control now live in a workspace crate of their own, publishable to
  crates.io and usable outside drevo:
  - `drevo_auth::authn`: the Argon2id `UserStore` with session tokens, plus
    `Credentials` (`Basic` or `Token`, built from a scheme / principal /
    credentials triple) and `AuthFailure`.
  - `drevo_auth::authz`: roles, permissions and the deny-overrides
    `AccessPolicy`.
  - drevo re-exports both as before (`drevo::authz`,
    `drevo::bolt::auth::UserStore`); its Cargo feature is now `auth`, with
    `bolt-auth` kept as an alias.
  - Session tokens are now 32 random bytes from the operating system; they
    were two time-ordered UUIDs, whose leading characters repeat between
    consecutive tokens.

- **Node menu** (issue #630). A right-click on a node in the Web UI graph
  opens a menu: **Expand neighbours**, **Show only its neighbourhood**,
  **Query its neighbours** (puts `MATCH (n)-[r]-(m) WHERE id(n) = 42 RETURN
  n, r, m` in the editor), **Copy Cypher**, **Copy id** and **Hide from the
  canvas** (the node stays in the database).

- **Request progress and Stop** (issue #628). While a Cypher query, a search
  or an Ask request runs, the Web UI status bar counts up (`Running Cypher…
  12.3s`) and a **Stop** button abandons the request in the browser; the
  server may still finish it (`DREVO_QUERY_TIMEOUT_MS` caps queries). A new
  request replaces a running one.

- **Graph labels at any zoom** (issue #626). In the Web UI graph, a node's
  size follows its number of connections on the canvas, and the 8 most
  connected nodes keep a readable label however far the view is zoomed out.
  Clicking a node labels it and its 8 most connected neighbours the same
  way until the background is clicked.

- **Colour nodes by a property** (issue #624). A **Colour by** select in the
  Web UI's left rail colours the graph by label (the default) or by a
  property such as `type` or `project`, with a legend of the values on the
  canvas. It offers string properties on at least 30% of the nodes whose 12
  most common values cover at least 60% of the nodes that have them. The
  choice is remembered per browser.

- **Readable inspector** (issue #622). The node and relationship inspector
  shows strings without quotes, `null` dimmed and every date as
  `YYYY-MM-DD HH:MM:SS UTC` (epoch numbers under time-like keys and ISO 8601
  strings alike, raw value in the tooltip). Text over 300 characters is
  clipped behind **Show more**, and a relationship's **from** / **to** are
  node links that open that node's neighbourhood.

- **Web UI label filter** (issue #620). The label chips show the 12 most
  common labels (plus the selected one) with **Show all N labels**, and a
  **Filter labels** box shows every label containing the typed text. On a
  window narrower than 1200px the top bar hides the server version to keep
  room for the query editor.

- **Ask in plain language** (issue #616). The Web UI's **Ask** button sends
  the editor's text to `drevo.cypher.fromText` and puts the generated Cypher
  in the editor to review; it never runs it. `GET /` reports
  `"text2cypher": {"model": …}` when an upstream is configured (`null`
  otherwise), and the button explains the setup until then.

- **Web UI query editor** (issue #615). The query bar takes several lines and
  grows with the text. Enter runs a one-line query, Shift+Enter adds a line,
  Ctrl/Cmd+Enter runs any query. Alt+↑ / Alt+↓ step through the last 50
  queries, which the ▾ button also lists; the history is kept in the browser.

- **Web UI result views** (issue #613). A Cypher result opens in the main area
  under **Graph**, **Table** and **JSON** tabs instead of a table in the left
  rail. A result with nodes opens as a graph, otherwise as a table. Table cells
  show strings without quotes, `null` dimmed, numbers right-aligned and nodes
  as clickable labels; the header stays visible while scrolling, and
  **Copy CSV** copies the rows.

- **`IS NOT NULL`, composite `IS UNIQUE` and `IS NODE KEY` constraints**
  (issue #606), in the Neo4j 5 `FOR … REQUIRE` and Neo4j 4 `ON … ASSERT`
  spellings (incl. `exists(n.p)`):
  - Uniqueness of a property tuple is checked on every write; presence at the
    end of the statement and at transaction commit. While a presence rule
    exists, each writing statement runs as one transaction, so
    `MERGE … ON CREATE SET` can fill a required property and a violating
    statement writes nothing.
  - `SHOW CONSTRAINTS` reports types `NODE_PROPERTY_EXISTENCE` and `NODE_KEY`
    and accepts multi-word filters (`SHOW NODE KEY CONSTRAINTS`).
  - drevo-core: `Constraint::UniqueNodeProperties`;
    `NativeGraph::has_existence_constraints`. `PropertyExists` and `NodeKey`
    now cover secondary labels and the `title`/`body` fields, treat `null` as
    missing, and are checked on writes outside a transaction too; a `NodeKey`
    duplicate is refused by the write itself.

- **UNIQUE constraints** (issue #604):
  - `CREATE CONSTRAINT [name] [IF NOT EXISTS] FOR (n:Label) REQUIRE n.prop IS
    UNIQUE` (and Neo4j 4's `ON … ASSERT …`), `DROP CONSTRAINT`, and
    `SHOW CONSTRAINTS` listing them.
  - Enforced on every node write through a value index.
  - A violation surfaces as Bolt `Neo.ClientError.Schema.ConstraintValidationFailed`,
    HTTP 409 with that `code`, and Python `ConstraintViolation`.
  - Definitions persist next to the WAL. Other constraint forms stay
    accepted no-ops.

### Fixed

- **User, role and privilege commands now need their DBMS privileges** (issue
  #636). With logins required, any signed-in user could run `CREATE USER`,
  `GRANT ROLE admin TO …` or `GRANT …` privileges. Now:
  - the user commands and `SHOW USERS` need `USER MANAGEMENT`;
  - the role commands and `SHOW ROLES` need `ROLE MANAGEMENT`;
  - privilege commands and `SHOW PRIVILEGES` need `PRIVILEGE MANAGEMENT`.

  A command without its privilege fails with
  `Neo.ClientError.Security.Forbidden` (HTTP 403) and changes nothing. Users
  may still look at and change themselves: `SHOW CURRENT USER`, `ALTER CURRENT
  USER SET PASSWORD`, and their own `SHOW USER PRIVILEGES`. `admin` holds all
  three.
  - `CALL drevo.tx.prepare` inside a Bolt transaction now needs `EXECUTE
    ADMIN PROCEDURES`, like the other `drevo.tx.*` procedures. A prepared
    transaction pauses every write until it is resolved.

- **Replica stamps survive a reopen** (issue #612). A durable store keeps
  each live node's and edge's causal stamp in `stamps.jsonl` next to the WAL,
  so after a restart its version vector is intact:
  - `delta_since(&VersionVector::new())` still carries every live record, so
    a new peer can bootstrap from a replica that restarted since its last write.
  - A local edit keeps its stamp, so a peer's older copy cannot overwrite it
    after the restart, and a peer's newer edit still wins.
  - A record without a saved stamp (a store from an earlier version, or a
    crash before the stamp reached disk) reopens with the lowest stamp: it
    is still sent to a new peer but loses to any real write.
  - The log is appended per stamped write without `fsync` and rewritten to one
    line per live record on open, on `compact_wal` and once it grows large.

- **Search, vector and schema statements inside explicit transactions**
  (issue #610). Inside Bolt `BEGIN` … `COMMIT` (the Neo4j drivers'
  `execute_read` / `execute_write`) and a Python `Transaction`:
  - Full-text, vector and fuzzy search, `keywords()`, `drevo.semantic.*`
    queries and status now work. Before, they failed with an error blaming a
    "KV storage engine". Hits resolve against the transaction's snapshot.
  - `SHOW INDEXES` / `SHOW CONSTRAINTS` list the schema (they were empty).
  - Nodes and relationships created in the transaction are auto-embedded.
  - Full-text search after the transaction (or the same statement) wrote fails
    with a message saying the index covers committed data only (Bolt
    `Neo.ClientError.Transaction.ForbiddenDueToTransactionType`).
  - Schema commands (`CREATE` / `DROP INDEX`, `CREATE` / `DROP CONSTRAINT`,
    `drevo.semantic.register*` / `reindex*`) take effect at once and cannot be
    rolled back, so they fail with that code and say to use auto-commit mode.
  - A query run without the server's services now says which one is missing
    instead of naming the removed KV engine.
  - drevo-core: `NativeGraph::tx_has_writes`.

- **Explicit transactions while a two-phase commit is prepared** (issue #608):
  - A transaction that only read now commits while a transaction is prepared
    (before, its `COMMIT` failed with the retryable "writes are paused"
    error). In drevo-core, a commit with an empty write set always succeeds
    and never reports a conflict.
  - `drevo.tx.commitPrepared` / `rollbackPrepared` / `heuristicRollback` /
    `listPrepared` inside an explicit transaction fail with
    `Neo.ClientError.Transaction.ForbiddenDueToTransactionType` and a message
    saying to call them in auto-commit mode, instead of an unknown error that
    blamed the storage engine.

### Changed

- **Node titles are no longer unique** (issue #595). Any number of nodes may
  share a title, as in Neo4j and Memgraph:
  - Cypher `CREATE` / `SET`, the HTTP API, the Python SDK, batch creates and
    imports no longer reject a repeat. HTTP `POST /nodes` no longer answers 409.
  - `get_node_by_title` returns the oldest node with the title; the new
    `nodes_by_title` returns all of them.
  - `DuplicateTitle` / `DuplicateTitleError` stay in the API so existing
    matches compile, but node writes no longer produce them.

### Added

- **CSR adjacency snapshot** (`drevo_core::csr::CsrAdjacency`, issue #382,
  Phase 8): a Compressed-Sparse-Row view of a `GraphSnapshot`'s out-adjacency
  (`GraphSnapshot::csr_out`) and in-adjacency (`GraphSnapshot::csr_in`) — a
  cache-friendly, contiguous layout with dense vertex indices, built off the
  immutable MVCC snapshot. The substrate for whole-graph parallel algorithms
  (#408, #412).
- **Morsel-driven parallel scan** (`CsrAdjacency::par_map`, issue #382): runs a
  per-vertex closure across worker threads over disjoint output slices — no
  locking, no contention, no new dependency (`std::thread::scope`). The
  parallel-execution primitive the analytics can fan out over (#411).

### Removed

- **drevo-auth's generic `authz` module** (and the `drevo::authz`
  re-export). The `privileges` module replaces its action/scope model with
  Neo4j's privilege model, which drevo enforces through `SecurityContext`.
  `drevo::privileges` re-exports it.

- **Duplicate CSR PageRank and weakly-connected-components** that had been added
  to `drevo_core::csr` in #409 and #410. **This was my (the assistant's)
  mistake:** I built second implementations of PageRank and WCC over the CSR
  layout without first checking that the crate already ships a complete graph
  analytics suite in `src/algorithms/` — `pagerank` (serial, `pagerank_parallel`,
  and `pagerank_native` over the native MVCC snapshot), `louvain`, `wcc`, `scc`,
  `triangles`, `betweenness`, `closeness` — the canonical, more capable versions
  (weighted edges, tolerance-based convergence, configurable damping), exposed as
  `CALL drevo.*` procedures. The csr.rs copies were weaker (unweighted,
  fixed-iteration), unused by anything, and pure duplication, so they are
  removed. Lesson recorded: grep `src/algorithms/` before implementing a graph
  algorithm; #382's actual intent is to give the *existing* algorithms a CSR
  layout for a real parallel speedup (as the `pagerank_native` doc comment
  already noted), not to reimplement them. The CSR layout and `par_map` above
  remain — those are the genuinely new, on-target contribution.
