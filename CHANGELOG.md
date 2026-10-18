# Changelog

All notable changes to drevo are recorded here. This complements the git
history and the in-repo issue tracker; format loosely follows
[Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added

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
