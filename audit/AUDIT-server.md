# AUDIT-server — Phase 8.5 task `00112`

**Scope.** `src/bin/server.rs` (was 93 LOC pre-refactor, now 52 LOC) and the
newly-introduced `src/server.rs` (340 LOC) into which the binary's parsing,
validation, bind/serve loop, and shutdown-signal future were moved so each
rule below can be exercised by a unit test. Also reviews the
`tracing` / `tracing-subscriber` wiring inside the `http` feature and the
`Cargo.toml` changes that follow from it. The audit's primary subject is the
**server-binary entry point** — not the HTTP router (`api.rs`, audit 00109)
nor the `ApiState`/`signal_shutdown` plumbing (task 00048).

**Rules verified against.** Cited verbatim from the spec sections this
audit is required to compare against:

- `drevo-rust` §"Async / Tokio" — _"HTTP server uses `axum` + `tokio`
  (multi-thread runtime)"; "Don't make the public API async unless it
  actually awaits something — `async fn` that doesn't await is a worse
  signature"._
- `drevo-rust` §"Error Handling" — _"Never `unwrap()` / `expect()` in
  library code — only in tests and benchmarks"._
- `drevo-rust` §"Code Style" — _"Doc-comments on every `pub` item;
  runnable examples in doc-comments where useful"; "Max 3 levels of
  indentation"; "SCREAMING_SNAKE_CASE constants"._
- `drevo-database` §"HTTP API" — REST table + the container convention
  (`0.0.0.0:8080`, `/data/drevo.redb`).
- `drevo-architecture` anti-pattern #2 ("Premature Abstraction"), #5
  ("Unwrap in Library Code"), #9 ("YAGNI" — applied in reverse to the
  deferred refactor targets).
- `drevo-tdd` §"every public function — at least 1 test"; §"Three Test
  Layers" (unit on `Config`; integration on `run()` via the existing
  `server_binary_tests.rs`).
- README task `00112` line items — env-var bounds, `tracing` adoption,
  Windows signal handling, cross-link with `00048`.

**Test baseline at audit start.** 1191 tests passing
(`cargo test`). After this PR: **1216 tests** passing (+25 — 23 new in
`tests/server_config_tests.rs`, 1 new in `tests/server_binary_tests.rs`
covering `drevo::server::run()` against a real temp data directory, plus
one upgrade that replaced the 4 stub tests in `server_binary_tests.rs`
with `Config`-backed assertions, net +1 because the old 4 already
counted). Zero regressions; `cargo clippy --all-features --all-targets
-- -D warnings` and `cargo clippy --target wasm32-unknown-unknown
--no-default-features --features wasm -- -D warnings` both clean;
`cargo fmt --check` clean.

---

## Summary table

| # | Severity | Rule | Status |
|---|----------|------|--------|
| F1 | **high** | `drevo-rust` §"Error Handling" — _"No `unwrap()` / `expect()` in library code"_ | **Fixed in this PR** (`Config` + `RunError`; `main` returns `ExitCode`) |
| F2 | **high** | README task 00112 — _"Replace `eprintln!` with `tracing` + `tracing-subscriber`"_ + cross-link to 00109 | **Fixed in this PR** (4 of 4 `eprintln!` sites converted; `RUST_LOG` env-filter wired) |
| F3 | medium | README task 00112 — _"`DREVO_PORT` bounds (u16, 1024+ recommended in container); `DREVO_DATA_DIR` path validation"_ | **Fixed in this PR** (`Config::from_env` rejects port 0 / non-u16 / empty data_dir; warns on 1..1024) |
| F4 | low | `drevo-database` §"HTTP API" — container default `0.0.0.0:8080` + `/data/drevo.redb` | **Pass** (encoded as `DEFAULT_HOST` / `DEFAULT_PORT` / `DEFAULT_DATA_DIR` / `DB_FILENAME` constants; locked in by `default_listen_addr_is_0_0_0_0_8080` + `data_directory_convention`) |
| F5 | low | `drevo-rust` §"Async / Tokio" — async only where awaiting | **Pass** (only `run()` and `shutdown_signal()` are async; `Config::*` is sync) |
| F6 | low | `drevo-rust` §"Code Style" — doc-comments on every `pub` item | **Pass** (`Config`, `ConfigError`, `RunError`, all four constants, every `pub fn`) |
| F7 | info | Cross-link to task `00048` — `signal_shutdown()` flow correctness | **Pass** (graceful-shutdown contract carried verbatim into `run()`; flip-then-drain ordering preserved; existing 00048 tests still green) |
| F8 | info | README task 00112 — _"Signal handling on Windows (currently `cfg(unix)` only) — either document the limitation or implement `Ctrl-Break` for Windows"_ | **Documented · defer** (module-level rustdoc records the limitation; full Windows SCM-stop wiring deferred to 00113) |
| F9 | info | README refactor target — `--config-file` CLI flag | **Deferred** — YAGNI / Premature Abstraction (no third reader of these values exists today) |
| F10 | info | README refactor target — document Windows signal behaviour | **Fixed in this PR** (see F8 disposition) |
| F11 | low | `drevo-rust` §"Code Style" — `SCREAMING_SNAKE_CASE` constants | **Pass** (`DEFAULT_HOST`, `DEFAULT_PORT`, `DEFAULT_DATA_DIR`, `DB_FILENAME`, `PRIVILEGED_PORT_CEILING`) |
| F12 | low | IPv6 host literals in `DREVO_HOST` (operator UX) | **Pass + regression test** (`socket_addr` brackets bare IPv6 before parse; `config_socket_addr_supports_ipv6`) |
| F13 | info | `drevo-tdd` §"every `pub fn` has ≥1 test" | **Pass** (23 dedicated tests in `tests/server_config_tests.rs` + 1 end-to-end `run_serves_health_against_a_temp_data_dir_and_shuts_down`) |

Severity legend: **high** = rule violation that materially affects
correctness / operability, **medium** = README-cited line item with a
named refactor target, **low** = stylistic / consistency, **info** =
informational pass-through with cross-link to a follow-up.

---

## Findings

### F1 — Four `expect()` calls in the binary's startup path (high; fixed)

**Rule.** `drevo-rust` §"Error Handling" — _"Never `unwrap()` /
`expect()` in library code — only in tests and benchmarks"_; the
project skill treats the binary's entry point as library-like
because operator-facing failure messages should be structured,
not a Rust panic backtrace dumped to stderr.

**Sites (pre-fix).**
- `src/bin/server.rs:35` — `.expect("DREVO_PORT must be a valid port number")`
- `src/bin/server.rs:41` — `.expect("failed to open database")`
- `src/bin/server.rs:48` — `.expect("invalid DREVO_HOST:DREVO_PORT combination")`
- `src/bin/server.rs:52` — `.expect("failed to bind TCP listener")`
- `src/bin/server.rs:66` — `.expect("server error")`
- `src/bin/server.rs:75` — `.expect("failed to install Ctrl+C handler")` _(in `shutdown_signal`)_
- `src/bin/server.rs:81` — `.expect("failed to install SIGTERM handler")` _(in `shutdown_signal`)_

**Why this matters.** Every one of these crashes the process with a
generic panic message — the operator gets a stack trace through
`Termination::report` but not a structured log line they can grep
for. Worse, the panics happen **after** axum's worker threads may
have already started, so the partial-init state is opaque.

**Fix.**

1. Extract the validation logic into [`drevo::server::Config::from_env`]
   ([src/server.rs:122](src/server.rs:122)), which returns a typed
   [`ConfigError`] for every failure case (port 0, port out of u16
   range, empty host, empty data_dir, …).
2. Move the bind / serve / shutdown loop into [`drevo::server::run`]
   ([src/server.rs:230](src/server.rs:230)), which returns a typed
   [`RunError`] for the three runtime failure cases
   (`DatabaseOpen`, `Bind`, `Serve`).
3. Rewrite `src/bin/server.rs` ([src/bin/server.rs:1](src/bin/server.rs:1))
   as a thin shim that initialises `tracing` and translates
   `ConfigError` / `RunError` into a `std::process::ExitCode` — exit
   code 2 for invalid configuration (operator can fix), exit code 1
   for runtime failures (likely a system / I/O problem).
4. The two `expect()` calls inside `shutdown_signal()` were converted
   to `tracing::error!` + a fall-through so that a signal-handler-
   install failure doesn't kill an otherwise-healthy server. This is
   the one place where the pre-refactor behaviour was actively
   wrong: a Unix process whose `SIGTERM` handler somehow failed to
   install should keep serving on `SIGINT` and not crash.

**Tests added.** [`tests/server_config_tests.rs`](tests/server_config_tests.rs):
`port_zero_is_rejected`, `port_above_u16_is_rejected`,
`port_negative_is_rejected`, `port_garbage_string_is_rejected`,
`port_empty_string_is_rejected`, `host_empty_string_is_rejected`,
`host_garbage_is_rejected_when_building_socket_addr`,
`data_dir_empty_string_is_rejected` — each asserts the precise
`ConfigError` variant.

---

### F2 — `eprintln!` in production startup path (high; fixed)

**Rule.** README task 00112 — _"Replace `eprintln!` with `tracing` +
`tracing-subscriber` (the project doesn't have a logging story yet;
introducing one here also unblocks `00109`)"_. Cross-linked from
audit 00109 finding F7 (_"Pre-existing `eprintln!` in handler error
paths — Pass (zero occurrences in `api.rs`; server binary tracked
under 00112)"_).

**Sites (pre-fix).**
- `src/bin/server.rs:40` — `eprintln!("drevo: opening database at {}", db_path.display());`
- `src/bin/server.rs:54` — `eprintln!("drevo: listening on {addr}");`
- `src/bin/server.rs:63` — `eprintln!("drevo: shutdown signal received, draining…");`
- `src/bin/server.rs:68` — `eprintln!("drevo: shut down cleanly");`

**Why this matters.** `eprintln!` is unstructured, untimestamped, and
not filterable. Container log shippers (Fluent Bit, Vector, journald)
treat the entire line as a single unparsed string. Once Phase 11
(Bolt) and Phase 14 (query optimiser) start emitting their own log
lines, the operator can't correlate them with the HTTP layer's
output. `tracing` solves this without bloating the dependency tree
that much (the lib already pulls in `axum` + `tokio` which depend on
the `tracing` macro for their own logs; before this PR drevo was
silently swallowing those).

**Fix.**

1. Add `tracing` + `tracing-subscriber` (with the `env-filter` +
   `fmt` features) to `Cargo.toml` under the `http` feature
   ([Cargo.toml:22](Cargo.toml:22)) so they're only compiled in
   server-binary builds, not in the FFI/WASM library distributions.
2. Convert all four `eprintln!` sites to `tracing::info!` with
   structured fields (`path = %db_path.display()`,
   `%addr`, `port = cfg.port`).
3. Initialise the subscriber from `RUST_LOG` in
   [src/bin/server.rs:48](src/bin/server.rs:48) with `info` as the
   default — operators can override (`RUST_LOG=drevo=debug`) without
   recompiling.

**Tests added.** None directly — `tracing` output is observable in
the binary's stderr but asserting on it would couple unit tests to
the human-readable log format. The
`run_serves_health_against_a_temp_data_dir_and_shuts_down`
integration test exercises the full startup → bind → serve → drop
sequence and would fail loudly if subscriber init panicked.

---

### F3 — Env-var validation gaps (medium; fixed)

**Rule.** README task 00112 — _"Env-var parsing: `DREVO_PORT` bounds
(u16, 1024+ recommended in container); `DREVO_DATA_DIR` path
validation"_.

**Sites (pre-fix).** `src/bin/server.rs:33` (port parse) and
`src/bin/server.rs:36` (data dir lookup) — neither validates beyond
what `str::parse::<u16>()` does. In particular:

- Port `0` parses fine as a `u16` but means "kernel-chosen
  ephemeral port" to `bind(2)`, which is always an operator mistake
  for a long-running container.
- Negative numbers, the literal `"abc"`, and the empty string fall
  through to the same generic `.expect("must be a valid port number")`
  panic — no hint that the value was, say, `-1` from an unquoted
  shell variable.
- `DREVO_DATA_DIR` with the empty string passes silently to
  `Path::new("")` which then joins to just `drevo.redb` — the
  database lands in the current working directory, which on
  Kubernetes is usually `/` (read-only) and the redb open errors
  with `permission denied` instead of pointing at the misconfigured
  env var.

**Why this matters.** Container-orchestration platforms (k8s, Nomad)
expand env vars from `ConfigMap` / `Secret` references; a missing
ConfigMap key silently produces the empty string. Catching it at
parse time gives the operator a single grep-able error line; falling
through to redb produces a stack trace.

**Fix.** Three rules in
[`Config::from_env`](src/server.rs:122):

| Variable          | Validation rule                                  |
|-------------------|--------------------------------------------------|
| `DREVO_PORT`      | parses as `u16`, rejects 0 (`ConfigError::InvalidPort`) |
| `DREVO_PORT`      | `1..1024` accepted but `Config::is_privileged_port` flags it for a `tracing::warn!` at startup |
| `DREVO_HOST`      | empty string rejected (`ConfigError::InvalidHost`); DNS names accepted (deferred to `socket_addr`'s parse) |
| `DREVO_DATA_DIR`  | empty string rejected (`ConfigError::InvalidDataDir`); absolute *and* relative paths accepted |

**Why not stricter validation on `data_dir`?** Local dev uses
relative paths (e.g. `./var/data`); enforcing `is_absolute()` would
make the binary unusable outside containers. Existence-checking the
parent directory is the redb backend's job — duplicating it here
would only widen the error-handling surface without preventing any
real misconfiguration.

**Tests added.** See F1's list — every validation rule has a
dedicated test in `tests/server_config_tests.rs`.

---

### F4 — Container default contract (low; pass + regression test)

**Rule.** `drevo-database` §"HTTP API" — REST table headers; README
task 00045 — _"drevo standalone HTTP server binary … container
deployment"_; `Dockerfile` — `EXPOSE 8080` + `VOLUME /data`.

**Site.** Pre-fix the defaults were string literals inlined in
`main()`. Post-fix they're the
`DEFAULT_HOST = "0.0.0.0"` /
`DEFAULT_PORT = 8080` /
`DEFAULT_DATA_DIR = "/data"` /
`DB_FILENAME = "drevo.redb"` constants in
[src/server.rs:47](src/server.rs:47).

**Disposition.** Pass. The four stub tests in
`tests/server_binary_tests.rs` (which used to assert against
hard-coded local strings) were rewritten in this PR to assert against
`Config::from_env(|_| None)` so a future drift between the binary
defaults and the Dockerfile / README contract trips a test.

---

### F5 — Async-surface discipline (low; pass)

**Rule.** `drevo-rust` §"Async / Tokio" — _"Don't make the public
API async unless it actually awaits something — `async fn` that
doesn't await is a worse signature."_

**Sites.**
- `Config::from_env` / `Config::socket_addr` / `Config::db_path` /
  `Config::is_privileged_port` — all sync. Pure env-var → struct
  validation has no reason to be async.
- `run()` / `shutdown_signal()` — async because they
  `axum::serve(...).await` and `tokio::signal::ctrl_c().await`
  respectively.

**Disposition.** Pass.

---

### F6 — Doc-coverage of the new public surface (low; pass)

**Rule.** `drevo-rust` §"Code Style" — _"Doc-comments on every `pub`
item"_.

**Sites.** Every new `pub` item in `src/server.rs` carries a `///`
block — the struct, both error enums, every field, every variant,
every method, every constant. Module-level `//!` block (39 lines)
documents the env vars, error semantics, and the Windows signal
caveat.

**Disposition.** Pass. `cargo doc --no-deps -- -D missing_docs` was
not run in this PR (it's task 00113's job) but a spot-check shows
the module compiles clean under `-W missing_docs`.

---

### F7 — `signal_shutdown()` cross-link with task 00048 (info; pass)

**Rule.** README task 00112 — _"The newly-added `signal_shutdown()`
flow from task `00048` is correct — cross-link with that task's PR"_.

**Site.** The pre-refactor binary did:
```rust
.with_graceful_shutdown(async move {
    shutdown_signal().await;
    shutdown_state.signal_shutdown();
    eprintln!("drevo: shutdown signal received, draining…");
})
```

The post-refactor `run()` keeps the exact same flip-then-drain
ordering ([src/server.rs:258](src/server.rs:258)). The reason matters:
flipping `ApiState::shutting_down` **before** axum begins draining
gives load balancers a chance to observe `503` on `/health` and
`/ready` and stop routing new traffic. Reversing the order would
race the drain window.

**Disposition.** Pass. The four 00048 tests
(`health_returns_503_after_signal_shutdown`,
`ready_returns_503_after_signal_shutdown`,
`signal_shutdown_flips_flag_and_is_idempotent`,
`shutdown_flag_is_shared_between_clones`) all stay green and
the new `run_serves_health_against_a_temp_data_dir_and_shuts_down`
test covers the bind + serve half of the path that those four
in-process tests stub out.

---

### F8 — Windows signal handling (info; documented · defer)

**Rule.** README task 00112 — _"Signal handling on Windows
(currently `cfg(unix)` only) — either document the limitation or
implement `Ctrl-Break` for Windows."_

**Site.** [src/server.rs:289](src/server.rs:289).
Pre-fix the `#[cfg(not(unix))]` branch was `std::future::pending::<()>()`,
i.e. on Windows the process only ever responds to `Ctrl+C` — never
`Ctrl+Break`, never a Windows-service-control-manager stop. Post-fix
**the behaviour is unchanged** but the module-level rustdoc now
explicitly states the limitation:

> _"Non-Unix: only `Ctrl+C` is observed; `SIGTERM` is unavailable on
> Windows. Windows console `Ctrl+Break` and Windows
> service-control-manager stop notifications are tracked as a
> follow-up under task `00113`."_

**Why defer?** Implementing it requires `tokio::signal::windows`
(currently not in our tokio feature flags) plus a behavioural
decision about whether the binary should integrate with the Windows
SCM at all — that's a deployment-story call that doesn't fit Phase
8.5's compliance scope. The Dockerfile ships a Linux container, so
the production code path is fully covered.

**Disposition.** Documented · defer. Cross-link added to
[task 00113](README.md:735) as a cross-cutting item.

---

### F9 — `--config-file` CLI flag (info; defer / YAGNI)

**Rule.** README task 00112 refactor targets — _"`--config-file`
CLI flag"_.

**Disposition.** Defer. The three env vars are the only configuration
inputs today; a config-file reader would introduce a second source
of truth (env vs file precedence, schema versioning, hot reload
semantics) **before** a single caller asks for it. Cross-link to
Phase 15's MCP / Web UI tasks which are the most likely consumers
of a future `--config-file` flag. `drevo-architecture` anti-pattern
#2 ("Premature Abstraction") explicitly forbids this kind of
speculative API.

---

### F10 — Document Windows signal behaviour (info; fixed)

See F8 disposition. The module-level `//!` block now records the
limitation explicitly and points at task 00113.

---

### F11 — Magic numbers / constants discipline (low; pass)

**Rule.** `drevo-rust` §"Code Style" — _"`SCREAMING_SNAKE_CASE`
constants"_.

**Sites.** Five new constants in `src/server.rs`:
- [`DEFAULT_HOST`](src/server.rs:48)
- [`DEFAULT_PORT`](src/server.rs:51)
- [`DEFAULT_DATA_DIR`](src/server.rs:53)
- [`DB_FILENAME`](src/server.rs:55)
- [`PRIVILEGED_PORT_CEILING`](src/server.rs:61) (module-private, so
  no `pub` doc-comment requirement; still carries a `///` block
  documenting **why** 1024 is the cutoff)

**Disposition.** Pass.

---

### F12 — IPv6 host literals (low; pass + regression test)

**Rule.** None explicit in the skills, but operator UX
expectation — k8s pod-level networking commonly uses `::1` or `::`
for IPv6-only deployments. The pre-refactor `format!("{}:{}", host,
port)` would silently produce `::1:8080` which is **not** a parsable
`SocketAddr` (the parser can't tell the address colons apart from
the port separator).

**Fix.** [`socket_addr()`](src/server.rs:159) detects bare IPv6
hosts (`host.contains(':') && !host.starts_with('[')`) and brackets
them before appending the port.

**Test.** `config_socket_addr_supports_ipv6` in
`tests/server_config_tests.rs`.

---

### F13 — Test coverage (info; pass)

**Rule.** `drevo-tdd` §"every `pub fn` — at least 1 test"; §"Three
Test Layers".

**Sites.**
- **Unit layer** — 23 tests in `tests/server_config_tests.rs` (the
  whole `Config` surface plus error message-content assertions).
- **Integration layer** —
  `run_serves_health_against_a_temp_data_dir_and_shuts_down` in
  `tests/server_binary_tests.rs` exercises `Config` → `run()` end
  to end with a real temp dir, ephemeral port, and abort-on-success.
- **Scenario layer** — out of scope for a binary entry point.

**Disposition.** Pass.

---

## Deferred refactors

| Target | Disposition | Cross-link |
|--------|-------------|------------|
| `--config-file` CLI flag | Defer / YAGNI | Phase 15 MCP / Web UI |
| Windows `Ctrl+Break` + SCM-stop wiring | Document · defer | task `00113` |
| `tracing-bunyan-formatter` or JSON log output for production | Defer | Phase 15 ops tasks (out of audit scope) |

---

## Files touched

| Path                                  | Change |
|---------------------------------------|--------|
| `src/server.rs`                       | **new** — 340 LOC: `Config`, `ConfigError`, `RunError`, `run()`, `shutdown_signal()`, container constants |
| `src/bin/server.rs`                   | rewritten — 93 → 52 LOC; thin shim that initialises `tracing` and translates errors into `ExitCode` |
| `src/lib.rs`                          | `pub mod server;` gated under `feature = "http"` |
| `Cargo.toml`                          | `tracing` + `tracing-subscriber` added; both wired into the `http` feature |
| `tests/server_config_tests.rs`        | **new** — 23 unit tests for `Config` |
| `tests/server_binary_tests.rs`        | 4 stub tests upgraded to assert against `Config`; 1 new end-to-end test for `run()` |
| `audit/AUDIT-server.md`               | **new** — this report |
| `README.md`                           | task `00112` marked done; Phase 8.5 progress line updated |

---

## Definition-of-done checklist (`Phase 8.5`)

- [x] `audit/AUDIT-server.md` exists and cites every skill rule it verified
- [x] Every cited rule is either ✅ compliant or has a follow-up refactor PR / accepted exception recorded
- [x] Test baseline grows (1191 → 1216, +25) with new unit + integration coverage
- [x] `cargo clippy --all-features --all-targets -- -D warnings` clean
- [x] `cargo clippy --target wasm32-unknown-unknown --no-default-features --features wasm -- -D warnings` clean
- [x] `cargo fmt --check` clean
- [x] Cross-link with audit 00109 finding F7 (`eprintln!` follow-up) explicit in F2
- [x] Cross-link with task 00048 (`signal_shutdown` flow) explicit in F7
- [x] Cross-link with task 00113 (deferred Windows signal work) explicit in F8
