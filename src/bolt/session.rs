//! Bolt session layer — Phase 11 tasks `00071` and `00072`.
//!
//! Sits on top of the wire codec (`00070`) and drives the per-connection
//! state machine that official Neo4j drivers (`cypher-shell`,
//! `neo4j-python-driver`, `neo4j-javascript-driver`) expect after the
//! handshake completes:
//!
//! ```text
//!                                    autocommit RUN
//!                  HELLO            +----------+
//!     Connected -----------> Ready  | RUN      v
//!                   ^          ^----+      Streaming
//!                   |          ^               |
//!                   |          | PULL/DISCARD  |
//!                   | RESET    |  (drained)    |
//!                   |          +---------------+
//!                   |
//!                   |             BEGIN          RUN
//!                   |          Ready -----> TxReady -----> TxStreaming
//!                   |                       ^   ^             |
//!                   |                       |   | PULL/DISCARD|
//!                   |        COMMIT/ROLLBACK|   +-------------+
//!                   |                       |     (drained)
//!                   |                       v
//!                   |                     Ready
//!                   |
//!                   |  any failure → Failed → (RESET) → Ready
//!                   |                          (open tx → rolled back)
//!                  GOODBYE
//!                   v
//!                Defunct
//! ```
//!
//! ## Scope
//!
//! Nine client message families decode through this layer and drive the
//! state machine end-to-end:
//!
//! - `HELLO` (`0x01`) — connection setup; replies with `SUCCESS` carrying
//!   `server` + `connection_id` metadata.
//! - `RUN` (`0x10`) — parses + executes a Cypher query against
//!   [`crate::cypher::executor::execute`], materialises the result, and
//!   replies with `SUCCESS { fields: [..] }`. Allowed both in autocommit
//!   mode (`Ready` → `Streaming`) and inside an explicit transaction
//!   (`TxReady` → `TxStreaming`).
//! - `PULL` (`0x3F`) — streams up to `n` `RECORD` messages followed by
//!   `SUCCESS { has_more: bool }`. Returns to `Ready` or `TxReady`
//!   depending on which branch the stream came from.
//! - `DISCARD` (`0x2F`) — drops up to `n` rows without emitting
//!   `RECORD`s, then replies `SUCCESS`. Same `Ready` / `TxReady`
//!   routing as `PULL`.
//! - `BEGIN` (`0x11`) — opens an explicit transaction (Phase 11 task
//!   `00072`). `Ready` → `TxReady`. Calls
//!   [`crate::db::Drevo::tx_begin`]; concurrent attempts get
//!   `Neo.TransientError.Transaction.Outdated`.
//! - `COMMIT` (`0x12`) — commits the in-flight transaction. `TxReady` →
//!   `Ready`; the journal is discarded.
//! - `ROLLBACK` (`0x13`) — rolls back the in-flight transaction.
//!   `TxReady` → `Ready`; the journal is replayed in reverse.
//! - `RESET` (`0x0F`) — clears any pending stream / failure and returns
//!   to `Ready`. If a transaction was open it is rolled back as part of
//!   the reset.
//! - `GOODBYE` (`0x02`) — closes the session; no reply. Any open
//!   transaction is rolled back on the way out so the next session
//!   isn't left holding the journal slot.
//!
//! ## Authentication (task `00074`)
//!
//! [`Session::new`](crate::bolt::session::Session::new) accepts any
//! `HELLO` extras (loopback / embedded use).
//! [`Session::with_auth`](crate::bolt::session::Session::with_auth) binds a
//! [`crate::bolt::auth::Authenticator`] that validates the `scheme` /
//! `principal` / `credentials` tuple before reaching `Ready`; a denial
//! replies `Neo.ClientError.Security.Unauthorized` and marks the
//! session `Defunct`.
//!
//! ## Not in scope
//!
//! - TLS — task `00073`.
//! - Multi-statement transaction isolation across concurrent sessions —
//!   lands with MVCC (`00080`–`00084`). For 00072 only one explicit
//!   transaction is in flight per [`crate::db::Drevo`] handle at a time.
//!
//! ## Why a materialised result
//!
//! [`crate::cypher::executor::ExecResult`] is already a fully-buffered
//! row set (the executor is fail-fast, not streaming). Bolt's
//! `RUN`/`PULL`/`DISCARD` then becomes a thin queue around an
//! [`std::vec::IntoIter`] over those rows. This will revisit once a
//! genuine streaming executor lands; in the meantime no row leaves the
//! server until the client explicitly `PULL`s it.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::bolt::auth::{AuthOutcome, Authenticator};
use crate::bolt::chunked::{read_message, write_message};
use crate::bolt::error::{BoltError, BoltResult};
use crate::bolt::packstream::{decode, encode, Value};
use crate::cypher::executor::{self, ExecError, Value as CypherValue};
use crate::cypher::parser::{self, ParseError};
use crate::db::Drevo;

// -------------------------------------------------------------------------
// Message tag bytes — pinned to the Bolt v4.4 spec.
// -------------------------------------------------------------------------

/// `HELLO` request: connection setup.
pub const HELLO: u8 = 0x01;
/// `GOODBYE` request: close the session.
pub const GOODBYE: u8 = 0x02;
/// `RESET` request: clear failed state / drop pending stream.
pub const RESET: u8 = 0x0F;
/// `RUN` request: parse + execute a Cypher query.
pub const RUN: u8 = 0x10;
/// `BEGIN` request: open an explicit transaction (Phase 11 task `00072`).
pub const BEGIN: u8 = 0x11;
/// `COMMIT` request: commit the open explicit transaction (Phase 11 task `00072`).
pub const COMMIT: u8 = 0x12;
/// `ROLLBACK` request: roll back the open explicit transaction (Phase 11 task `00072`).
pub const ROLLBACK: u8 = 0x13;
/// `DISCARD` request: drop pending rows without emitting them.
pub const DISCARD: u8 = 0x2F;
/// `PULL` request: stream pending rows as `RECORD` messages.
pub const PULL: u8 = 0x3F;
/// `SUCCESS` response.
pub const SUCCESS: u8 = 0x70;
/// `RECORD` response: a single row from a streaming result set.
pub const RECORD: u8 = 0x71;
/// `IGNORED` response: returned for messages received while in `Failed`.
pub const IGNORED: u8 = 0x7E;
/// `FAILURE` response.
pub const FAILURE: u8 = 0x7F;

// Bolt v4 PackStream graph-type tag bytes.
const NODE_TAG: u8 = 0x4E;
const RELATIONSHIP_TAG: u8 = 0x52;
const UNBOUND_RELATIONSHIP_TAG: u8 = 0x72;
const PATH_TAG: u8 = 0x50;

// -------------------------------------------------------------------------
// Client + server message enums.
// -------------------------------------------------------------------------

/// Client → server messages handled by this layer.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    /// `HELLO` — single dictionary field carrying `user_agent` / auth.
    Hello {
        /// Free-form metadata sent by the driver (user agent, auth scheme, …).
        extra: BTreeMap<String, Value>,
    },
    /// `GOODBYE` — no fields.
    Goodbye,
    /// `RESET` — no fields.
    Reset,
    /// `RUN` — Cypher source + parameters + execution metadata.
    Run {
        /// Cypher source text.
        query: String,
        /// `$name` → value parameter map.
        parameters: BTreeMap<String, Value>,
        /// Per-call metadata (`mode`, `db`, `tx_timeout`, …).
        extra: BTreeMap<String, Value>,
    },
    /// `BEGIN` — open an explicit transaction (Phase 11 task `00072`).
    Begin {
        /// Per-transaction metadata (`tx_timeout`, `mode`, `bookmarks`, …).
        extra: BTreeMap<String, Value>,
    },
    /// `COMMIT` — commit the in-flight explicit transaction.
    Commit,
    /// `ROLLBACK` — roll back the in-flight explicit transaction.
    Rollback,
    /// `DISCARD` — drop pending rows.
    Discard {
        /// Spec field carries `n` (`-1` = all, `N` = at most N).
        extra: BTreeMap<String, Value>,
    },
    /// `PULL` — stream pending rows.
    Pull {
        /// Spec field carries `n` (`-1` = all, `N` = at most N).
        extra: BTreeMap<String, Value>,
    },
}

/// Server → client messages produced by this layer.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    /// `SUCCESS` carrying a single dictionary of metadata.
    Success {
        /// `server`, `connection_id`, `fields`, `has_more`, ….
        metadata: BTreeMap<String, Value>,
    },
    /// `FAILURE` carrying `code` + `message`.
    Failure {
        /// `code` (e.g. `Neo.ClientError.Statement.SyntaxError`) + `message`.
        metadata: BTreeMap<String, Value>,
    },
    /// `RECORD` carrying a single row's field values.
    Record {
        /// Row values, one per column projected by the trailing `RETURN`.
        fields: Vec<Value>,
    },
    /// `IGNORED` — sent in response to any non-`RESET` message while in
    /// the `Failed` state.
    Ignored,
}

// -------------------------------------------------------------------------
// Connection-state machine.
// -------------------------------------------------------------------------

/// Per-connection state. Bolt sessions move through these states in
/// response to client messages; the dispatcher in [`Session::handle`]
/// owns the only transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Handshake complete, no `HELLO` yet.
    Connected,
    /// `HELLO` accepted; ready to receive `RUN` (autocommit) or `BEGIN`.
    Ready,
    /// Autocommit `RUN` succeeded; `PULL` / `DISCARD` drain the pending
    /// result set. Returning to `Ready` releases no transactional state
    /// because the executor has already committed.
    Streaming,
    /// `BEGIN` accepted (Phase 11 task `00072`); inside an explicit
    /// transaction, ready to receive `RUN` / `COMMIT` / `ROLLBACK`.
    TxReady,
    /// `RUN` inside an explicit transaction; `PULL` / `DISCARD` drain
    /// the pending result set and return to [`State::TxReady`]. The
    /// open transaction remains in flight until `COMMIT` / `ROLLBACK`.
    TxStreaming,
    /// Any client-visible error transitioned the session here; only
    /// `RESET` will clear it (every other message → `IGNORED`). If a
    /// transaction was open at the moment of the failure, `RESET` will
    /// roll it back as part of the cleanup.
    Failed,
    /// `GOODBYE` was received; subsequent messages are not processed.
    Defunct,
}

// -------------------------------------------------------------------------
// Decode / encode of PackStream Structure ↔ message enum.
// -------------------------------------------------------------------------

/// Decode a [`ClientMessage`] from a PackStream [`Value`].
///
/// # Errors
///
/// * [`BoltError::UnknownMarker`] — the structure carries a tag byte
///   that does not correspond to any client message family.
/// * [`BoltError::Io`] — a structure field had the wrong shape (e.g.
///   `RUN` arrived with two fields instead of three).
pub fn decode_client(value: &Value) -> BoltResult<ClientMessage> {
    let (tag, fields) = match value {
        Value::Structure { tag, fields } => (*tag, fields),
        other => {
            return Err(BoltError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("expected PackStream Structure for Bolt message, got {other:?}"),
            )));
        }
    };
    match tag {
        HELLO => Ok(ClientMessage::Hello {
            extra: take_dict_field(fields, 0, "HELLO.extra")?,
        }),
        GOODBYE => {
            require_arity(fields, 0, "GOODBYE")?;
            Ok(ClientMessage::Goodbye)
        }
        RESET => {
            require_arity(fields, 0, "RESET")?;
            Ok(ClientMessage::Reset)
        }
        RUN => {
            require_arity(fields, 3, "RUN")?;
            let query = take_string_field(fields, 0, "RUN.query")?;
            let parameters = take_dict_field(fields, 1, "RUN.parameters")?;
            let extra = take_dict_field(fields, 2, "RUN.extra")?;
            Ok(ClientMessage::Run {
                query,
                parameters,
                extra,
            })
        }
        BEGIN => Ok(ClientMessage::Begin {
            extra: take_dict_field(fields, 0, "BEGIN.extra")?,
        }),
        COMMIT => {
            require_arity(fields, 0, "COMMIT")?;
            Ok(ClientMessage::Commit)
        }
        ROLLBACK => {
            require_arity(fields, 0, "ROLLBACK")?;
            Ok(ClientMessage::Rollback)
        }
        DISCARD => Ok(ClientMessage::Discard {
            extra: take_dict_field(fields, 0, "DISCARD.extra")?,
        }),
        PULL => Ok(ClientMessage::Pull {
            extra: take_dict_field(fields, 0, "PULL.extra")?,
        }),
        other => Err(BoltError::UnknownMarker(other)),
    }
}

/// Encode a [`ServerMessage`] as a PackStream [`Value`].
pub fn encode_server(msg: &ServerMessage) -> Value {
    match msg {
        ServerMessage::Success { metadata } => Value::Structure {
            tag: SUCCESS,
            fields: vec![Value::Dictionary(metadata.clone())],
        },
        ServerMessage::Failure { metadata } => Value::Structure {
            tag: FAILURE,
            fields: vec![Value::Dictionary(metadata.clone())],
        },
        ServerMessage::Record { fields } => Value::Structure {
            tag: RECORD,
            fields: vec![Value::List(fields.clone())],
        },
        ServerMessage::Ignored => Value::Structure {
            tag: IGNORED,
            fields: vec![],
        },
    }
}

fn require_arity(fields: &[Value], expected: usize, msg_name: &str) -> BoltResult<()> {
    if fields.len() != expected {
        return Err(BoltError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{msg_name}: expected {expected} field(s), got {}",
                fields.len()
            ),
        )));
    }
    Ok(())
}

fn take_dict_field(
    fields: &[Value],
    idx: usize,
    label: &str,
) -> BoltResult<BTreeMap<String, Value>> {
    match fields.get(idx) {
        Some(Value::Dictionary(m)) => Ok(m.clone()),
        Some(other) => Err(BoltError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{label}: expected Dictionary, got {other:?}"),
        ))),
        None => Err(BoltError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{label}: missing field at index {idx}"),
        ))),
    }
}

fn take_string_field(fields: &[Value], idx: usize, label: &str) -> BoltResult<String> {
    match fields.get(idx) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(BoltError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{label}: expected String, got {other:?}"),
        ))),
        None => Err(BoltError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{label}: missing field at index {idx}"),
        ))),
    }
}

// -------------------------------------------------------------------------
// Failure-code constants (Neo4j status code namespace).
// -------------------------------------------------------------------------

mod codes {
    pub const REQUEST_INVALID: &str = "Neo.ClientError.Request.Invalid";
    pub const PROTOCOL_VIOLATION: &str = "Neo.ClientError.Request.InvalidFormat";
    pub const SYNTAX_ERROR: &str = "Neo.ClientError.Statement.SyntaxError";
    pub const SEMANTIC_ERROR: &str = "Neo.ClientError.Statement.SemanticError";
    pub const PARAMETER_MISSING: &str = "Neo.ClientError.Statement.ParameterMissing";
    pub const UNSUPPORTED: &str = "Neo.DatabaseError.General.UnknownError";
    pub const STORAGE: &str = "Neo.DatabaseError.Statement.ExecutionFailed";
    /// Returned when a `HELLO` carries missing / wrong / unsupported
    /// authentication credentials and the session is bound to an
    /// [`Authenticator`](crate::bolt::auth::Authenticator). Phase 11
    /// task `00074`.
    pub const UNAUTHORIZED: &str = "Neo.ClientError.Security.Unauthorized";
}

// -------------------------------------------------------------------------
// Session.
// -------------------------------------------------------------------------

/// Connection identifier generator — incremented per `Session::new` call.
/// Wrapped in an `AtomicU64` so concurrent sessions get distinct ids
/// without taking a lock.
static CONNECTION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Per-connection Bolt session.
///
/// The session borrows the [`Drevo`] handle for its lifetime — multiple
/// concurrent sessions just hold separate `&Drevo` refs. The session
/// itself is not `Sync`; callers that want to multiplex requests across
/// threads should wrap it in an [`std::sync::Mutex`].
pub struct Session<'a> {
    drevo: &'a Drevo,
    state: State,
    server_agent: String,
    connection_id: String,
    /// Materialised result set for the active `RUN` (set in
    /// `Ready`→`Streaming`, drained by `PULL`/`DISCARD`).
    pending: Option<PendingResult>,
    /// Optional credential check applied to `HELLO`. `None` accepts any
    /// connection (loopback / embedded use); `Some` enforces the
    /// `scheme`/`principal`/`credentials` tuple before reaching
    /// [`State::Ready`]. Phase 11 task `00074`.
    authenticator: Option<&'a dyn Authenticator>,
    /// The [`TxId`](crate::db::TxId) of *this* session's open explicit
    /// transaction, if any — set on `BEGIN`, cleared on `COMMIT` / `ROLLBACK`
    /// (and by the cleanup on `RESET` / `GOODBYE` / drop).
    ///
    /// Each connection's `BEGIN` allocates its own id, so concurrent pooled
    /// connections never collide on a shared slot (issue #298), and the
    /// lifecycle teardown hooks roll back only *this* session's transaction —
    /// a pooled driver's `RESET` on one connection can never disturb a
    /// managed transaction in flight on another (issue #236).
    tx: Option<crate::db::TxId>,
    /// Engine-flip routing (RFC #307 Phase 6): when set, autocommit
    /// statements go through the database's
    /// [`crate::native_mirror::NativeMirror`] (read-only queries served
    /// natively, writes on KV). Statements inside an explicit transaction
    /// always bypass the mirror — they must observe the transaction's own
    /// uncommitted writes on the KV engine.
    native: Option<(
        std::sync::Arc<crate::db::Drevo>,
        std::sync::Arc<crate::native_mirror::NativeMirror>,
    )>,
}

struct PendingResult {
    rows: std::vec::IntoIter<Vec<CypherValue>>,
}

impl<'a> Session<'a> {
    /// Create a new session bound to `drevo`. Starts in
    /// [`State::Connected`]; the caller must send `HELLO` next.
    ///
    /// No authentication is enforced — every `HELLO` is accepted. Use
    /// [`Session::with_auth`](crate::bolt::session::Session::with_auth) to require credentials.
    pub fn new(drevo: &'a Drevo) -> Self {
        Self::build(drevo, None)
    }

    /// Create a new session that authenticates every `HELLO` against
    /// `authenticator` before transitioning to [`State::Ready`]. A
    /// `HELLO` with missing / wrong / unsupported credentials is
    /// answered with a `Neo.ClientError.Security.Unauthorized` failure
    /// and the connection is marked [`State::Defunct`]. Phase 11 task
    /// `00074`.
    pub fn with_auth(drevo: &'a Drevo, authenticator: &'a dyn Authenticator) -> Self {
        Self::build(drevo, Some(authenticator))
    }

    /// Route this session's autocommit statements through the database's
    /// native read mirror (engine flip, RFC #307 Phase 6). `db` must be the
    /// same handle this session was built on; the owned `Arc` is what the
    /// mirror's background rebuild thread clones. Explicit-transaction
    /// statements keep executing directly on the KV engine.
    #[must_use]
    pub fn with_native_mirror(
        mut self,
        db: std::sync::Arc<crate::db::Drevo>,
        mirror: std::sync::Arc<crate::native_mirror::NativeMirror>,
    ) -> Self {
        self.native = Some((db, mirror));
        self
    }

    fn build(drevo: &'a Drevo, authenticator: Option<&'a dyn Authenticator>) -> Self {
        let id = CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self {
            drevo,
            state: State::Connected,
            // The `server` agent MUST start with `Neo4j/` — the official Neo4j
            // drivers reject any other product with `UnsupportedServerProduct`.
            // drevo's Bolt surface is a deliberate Neo4j-compatible drop-in, so
            // we report a Neo4j-version-prefixed agent (drevo's own version is
            // kept as a suffix and remains available via the HTTP `/status`).
            server_agent: format!("Neo4j/5.26.0-drevo-{}", crate::VERSION),
            connection_id: format!("drevo-bolt-{id}"),
            pending: None,
            authenticator,
            tx: None,
            native: None,
        }
    }

    /// Current connection state.
    pub fn state(&self) -> State {
        self.state
    }

    /// `server`-metadata string returned in the HELLO acknowledgement.
    pub fn server_agent(&self) -> &str {
        &self.server_agent
    }

    /// Unique identifier emitted in the HELLO acknowledgement.
    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }

    /// Drive the state machine forward by one client message. Returns
    /// the response(s) that should be written back to the wire, in
    /// order. An empty `Vec` means "no reply" (only `GOODBYE` produces
    /// this).
    pub fn handle(&mut self, msg: ClientMessage) -> Vec<ServerMessage> {
        if self.state == State::Defunct {
            return vec![ServerMessage::Ignored];
        }
        // Failed → IGNORE everything except RESET / GOODBYE.
        if self.state == State::Failed {
            return match msg {
                ClientMessage::Reset => self.handle_reset(),
                ClientMessage::Goodbye => self.handle_goodbye(),
                _ => vec![ServerMessage::Ignored],
            };
        }
        match msg {
            ClientMessage::Hello { extra } => self.handle_hello(extra),
            ClientMessage::Goodbye => self.handle_goodbye(),
            ClientMessage::Reset => self.handle_reset(),
            ClientMessage::Run {
                query,
                parameters,
                extra,
            } => self.handle_run(query, parameters, extra),
            ClientMessage::Pull { extra } => self.handle_pull(extra),
            ClientMessage::Discard { extra } => self.handle_discard(extra),
            ClientMessage::Begin { extra } => self.handle_begin(extra),
            ClientMessage::Commit => self.handle_commit(),
            ClientMessage::Rollback => self.handle_rollback(),
        }
    }

    fn handle_hello(&mut self, extra: BTreeMap<String, Value>) -> Vec<ServerMessage> {
        if self.state != State::Connected {
            self.state = State::Failed;
            return vec![ServerMessage::Failure {
                metadata: failure_metadata(
                    codes::REQUEST_INVALID,
                    "HELLO received outside CONNECTED state",
                ),
            }];
        }
        // Validate the `scheme` / `principal` / `credentials` tuple when
        // an authenticator is bound (Phase 11 task `00074`). A denial
        // marks the connection Defunct after the FAILURE is written —
        // the Bolt contract is to close the socket on a failed auth, and
        // the session loops return once they observe `Defunct`.
        if let Some(auth) = self.authenticator {
            if let AuthOutcome::Denied(reason) = auth.authenticate(&extra) {
                self.state = State::Defunct;
                return vec![ServerMessage::Failure {
                    metadata: failure_metadata(codes::UNAUTHORIZED, &reason),
                }];
            }
        }
        self.state = State::Ready;
        let mut md: BTreeMap<String, Value> = BTreeMap::new();
        md.insert(
            "server".to_string(),
            Value::String(self.server_agent.clone()),
        );
        md.insert(
            "connection_id".to_string(),
            Value::String(self.connection_id.clone()),
        );
        vec![ServerMessage::Success { metadata: md }]
    }

    /// Roll back the explicit transaction *this* session owns, if any,
    /// swallowing any rollback error. Idempotent: clears `owns_tx` so a
    /// later drop / GOODBYE does not double-roll-back. Used by the
    /// no-reply teardown paths (`GOODBYE`, connection drop) where there is
    /// no client left to receive a `FAILURE`.
    fn roll_back_own_tx(&mut self) {
        if let Some(id) = self.tx.take() {
            let _ = self.drevo.tx_rollback(id);
        }
    }

    fn handle_goodbye(&mut self) -> Vec<ServerMessage> {
        // An explicit transaction *this* session left open at GOODBYE is
        // rolled back so the next session through the same `Drevo` handle
        // isn't blocked by a stale journal slot. We gate on `owns_tx`, not
        // the global `is_tx_active()`: another connection may legitimately
        // hold the slot, and its transaction is none of our business
        // (issue #236). Errors during rollback are silently swallowed —
        // the client is already walking away and no reply is delivered.
        self.roll_back_own_tx();
        self.state = State::Defunct;
        self.pending = None;
        Vec::new()
    }

    fn handle_reset(&mut self) -> Vec<ServerMessage> {
        self.pending = None;
        // RESET inside *our own* explicit transaction rolls it back,
        // matching the Neo4j driver contract: the transaction is gone when
        // the session returns to READY. Gated on `owns_tx` so a pooled
        // driver's RESET on this connection never disturbs a transaction
        // in flight on another (issue #236). Any rollback failure surfaces
        // as FAILURE so the driver gets a deterministic signal rather than
        // a torn connection.
        if let Some(id) = self.tx.take() {
            if let Err(e) = self.drevo.tx_rollback(id) {
                self.state = State::Failed;
                return vec![ServerMessage::Failure {
                    metadata: failure_metadata(
                        codes::STORAGE,
                        &format!("rollback during RESET failed: {e}"),
                    ),
                }];
            }
        }
        self.state = State::Ready;
        vec![ServerMessage::Success {
            metadata: BTreeMap::new(),
        }]
    }

    fn handle_begin(&mut self, _extra: BTreeMap<String, Value>) -> Vec<ServerMessage> {
        if self.state != State::Ready {
            self.state = State::Failed;
            return vec![ServerMessage::Failure {
                metadata: failure_metadata(
                    codes::REQUEST_INVALID,
                    "BEGIN outside READY state — explicit transactions cannot nest",
                ),
            }];
        }
        // Per-connection transactions (issue #298): `tx_begin` allocates a
        // fresh id and never collides with another connection's in-flight
        // transaction, so a pooled driver's concurrent `execute_write` calls
        // each open their own instead of one getting `transaction already
        // active`. Nesting on a *single* connection is still rejected above by
        // the `state != Ready` guard.
        let id = self.drevo.tx_begin();
        self.state = State::TxReady;
        self.tx = Some(id);
        vec![ServerMessage::Success {
            metadata: BTreeMap::new(),
        }]
    }

    fn handle_commit(&mut self) -> Vec<ServerMessage> {
        if self.state != State::TxReady {
            self.state = State::Failed;
            return vec![ServerMessage::Failure {
                metadata: failure_metadata(
                    codes::REQUEST_INVALID,
                    "COMMIT without an active transaction",
                ),
            }];
        }
        // Whatever the outcome, the slot is no longer ours to clean up. The
        // TxReady state guarantees an id is present; treat its absence
        // defensively rather than panicking.
        let Some(id) = self.tx.take() else {
            self.state = State::Failed;
            return vec![ServerMessage::Failure {
                metadata: failure_metadata(codes::STORAGE, "COMMIT without an active transaction"),
            }];
        };
        match self.drevo.tx_commit(id) {
            Ok(()) => {
                self.state = State::Ready;
                vec![ServerMessage::Success {
                    metadata: BTreeMap::new(),
                }]
            }
            Err(e) => {
                self.state = State::Failed;
                vec![ServerMessage::Failure {
                    metadata: failure_metadata(codes::STORAGE, &format!("{e}")),
                }]
            }
        }
    }

    fn handle_rollback(&mut self) -> Vec<ServerMessage> {
        if self.state != State::TxReady {
            self.state = State::Failed;
            return vec![ServerMessage::Failure {
                metadata: failure_metadata(
                    codes::REQUEST_INVALID,
                    "ROLLBACK without an active transaction",
                ),
            }];
        }
        // Whatever the outcome, the slot is no longer ours to clean up.
        let Some(id) = self.tx.take() else {
            self.state = State::Failed;
            return vec![ServerMessage::Failure {
                metadata: failure_metadata(
                    codes::STORAGE,
                    "ROLLBACK without an active transaction",
                ),
            }];
        };
        match self.drevo.tx_rollback(id) {
            Ok(()) => {
                self.state = State::Ready;
                vec![ServerMessage::Success {
                    metadata: BTreeMap::new(),
                }]
            }
            Err(e) => {
                self.state = State::Failed;
                vec![ServerMessage::Failure {
                    metadata: failure_metadata(codes::STORAGE, &format!("{e}")),
                }]
            }
        }
    }

    fn handle_run(
        &mut self,
        query: String,
        parameters: BTreeMap<String, Value>,
        _extra: BTreeMap<String, Value>,
    ) -> Vec<ServerMessage> {
        // RUN is legal both in autocommit mode (`Ready`) and inside an
        // explicit transaction (`TxReady`); the resulting stream lands
        // in `Streaming` or `TxStreaming` respectively so PULL knows
        // which state to return to once the rows are drained.
        let in_tx = match self.state {
            State::Ready => false,
            State::TxReady => true,
            _ => {
                self.state = State::Failed;
                return vec![ServerMessage::Failure {
                    metadata: failure_metadata(
                        codes::REQUEST_INVALID,
                        "RUN outside READY / TX_READY state",
                    ),
                }];
            }
        };
        // Convert PackStream parameters into Cypher executor values.
        let cypher_params = match params_to_cypher(parameters) {
            Ok(p) => p,
            Err(msg) => {
                self.state = State::Failed;
                return vec![ServerMessage::Failure {
                    metadata: failure_metadata(codes::PROTOCOL_VIOLATION, &msg),
                }];
            }
        };
        // Parse.
        let ast = match parser::parse(&query) {
            Ok(q) => q,
            Err(e) => {
                self.state = State::Failed;
                return vec![ServerMessage::Failure {
                    metadata: parse_error_metadata(&e),
                }];
            }
        };
        // Execute (already returns a fully-materialised ExecResult). Inside an
        // explicit transaction, bind this statement's mutations to *this*
        // connection's transaction for its duration, so its undo ops journal
        // into the right per-connection slot (issue #298). The guard drops at
        // the end of this block — the scope never outlives the synchronous
        // execute call, so it cannot leak onto a later statement.
        let exec = match (&self.native, in_tx) {
            // Autocommit + engine flip active: the mirror serves read-only
            // queries natively and routes everything else to KV.
            (Some((db, mirror)), false) => mirror.execute(db, &ast, cypher_params),
            _ => {
                let _tx_scope = if in_tx {
                    self.tx.map(crate::db::enter_tx_scope)
                } else {
                    None
                };
                executor::execute(&ast, self.drevo, cypher_params)
            }
        };
        let result = match exec {
            Ok(r) => r,
            Err(e) => {
                self.state = State::Failed;
                return vec![ServerMessage::Failure {
                    metadata: exec_error_metadata(&e),
                }];
            }
        };
        // Build the SUCCESS{fields: [...]} reply and stash the rows.
        let cols = result.columns.clone();
        self.pending = Some(PendingResult {
            rows: result.rows.into_iter(),
        });
        self.state = if in_tx {
            State::TxStreaming
        } else {
            State::Streaming
        };
        let mut md: BTreeMap<String, Value> = BTreeMap::new();
        md.insert(
            "fields".to_string(),
            Value::List(cols.into_iter().map(Value::String).collect()),
        );
        vec![ServerMessage::Success { metadata: md }]
    }

    fn handle_pull(&mut self, extra: BTreeMap<String, Value>) -> Vec<ServerMessage> {
        // Take ownership of the pending stream up front; if either
        // precondition fails, restore nothing and transition to Failed.
        let (mut pending, in_tx) = match (self.state, self.pending.take()) {
            (State::Streaming, Some(p)) => (p, false),
            (State::TxStreaming, Some(p)) => (p, true),
            _ => {
                self.state = State::Failed;
                return vec![ServerMessage::Failure {
                    metadata: failure_metadata(
                        codes::REQUEST_INVALID,
                        "PULL without an active result stream",
                    ),
                }];
            }
        };
        let n = extract_n(&extra);
        let mut out = Vec::new();
        let mut emitted: i64 = 0;
        let exhausted;
        loop {
            if n >= 0 && emitted >= n {
                // Pulled the requested batch — check whether more rows are
                // still queued WITHOUT consuming one. `pending.rows` is a
                // `std::vec::IntoIter`, so `as_slice()` exposes the remaining
                // items non-destructively. (The old
                // `.by_ref().peekable().peek()` built a throwaway `Peekable`
                // that pulled the next row into its buffer and then dropped
                // it — silently losing one row at every PULL batch boundary.)
                exhausted = pending.rows.as_slice().is_empty();
                break;
            }
            match pending.rows.next() {
                Some(row) => {
                    let pack_row: Vec<Value> = row.into_iter().map(cypher_to_pack).collect();
                    out.push(ServerMessage::Record { fields: pack_row });
                    emitted += 1;
                }
                None => {
                    exhausted = true;
                    break;
                }
            }
        }
        let mut md: BTreeMap<String, Value> = BTreeMap::new();
        if exhausted {
            // A drained stream returns to TX_READY when we're inside an
            // explicit transaction, otherwise to READY. The explicit-tx
            // remains in flight until COMMIT / ROLLBACK.
            self.state = if in_tx { State::TxReady } else { State::Ready };
            md.insert("has_more".to_string(), Value::Boolean(false));
            // `type` per Bolt 4.4 spec: r / w / rw / s. We do not yet
            // distinguish — a query that mutated rows is captured by
            // ExecStats; we report `rw` so drivers don't assume
            // read-only and accidentally retry.
            md.insert("type".to_string(), Value::String("rw".to_string()));
        } else {
            // Restore the partially-drained stream so the next PULL /
            // DISCARD can continue from where we stopped.
            self.pending = Some(pending);
            md.insert("has_more".to_string(), Value::Boolean(true));
        }
        out.push(ServerMessage::Success { metadata: md });
        out
    }

    fn handle_discard(&mut self, extra: BTreeMap<String, Value>) -> Vec<ServerMessage> {
        let (mut pending, in_tx) = match (self.state, self.pending.take()) {
            (State::Streaming, Some(p)) => (p, false),
            (State::TxStreaming, Some(p)) => (p, true),
            _ => {
                self.state = State::Failed;
                return vec![ServerMessage::Failure {
                    metadata: failure_metadata(
                        codes::REQUEST_INVALID,
                        "DISCARD without an active result stream",
                    ),
                }];
            }
        };
        let n = extract_n(&extra);
        let mut dropped: i64 = 0;
        let exhausted;
        loop {
            if n >= 0 && dropped >= n {
                // Non-consuming remaining-rows check — see `handle_pull`; the
                // old `.by_ref().peekable().peek()` dropped one row per batch.
                exhausted = pending.rows.as_slice().is_empty();
                break;
            }
            if pending.rows.next().is_none() {
                exhausted = true;
                break;
            }
            dropped += 1;
        }
        let mut md: BTreeMap<String, Value> = BTreeMap::new();
        if exhausted {
            self.state = if in_tx { State::TxReady } else { State::Ready };
            md.insert("has_more".to_string(), Value::Boolean(false));
            md.insert("type".to_string(), Value::String("rw".to_string()));
        } else {
            self.pending = Some(pending);
            md.insert("has_more".to_string(), Value::Boolean(true));
        }
        vec![ServerMessage::Success { metadata: md }]
    }
}

impl Drop for Session<'_> {
    /// A connection that drops without `COMMIT` / `ROLLBACK` / `GOODBYE`
    /// (a hard disconnect, a cancelled task, a panicked handler) must not
    /// leak the `Drevo` handle's single explicit-transaction slot — with
    /// per-session ownership (issue #236), no *other* session will clean
    /// it up, so a leaked slot would reject every future `BEGIN` with
    /// `transaction already active`. Rolling back our own transaction here
    /// closes that gap on every exit path. A no-op unless we still own the
    /// slot (`COMMIT` / `ROLLBACK` / `RESET` / `GOODBYE` already cleared it).
    fn drop(&mut self) {
        self.roll_back_own_tx();
    }
}

// -------------------------------------------------------------------------
// run_session_sync — drives the loop over a sync Read/Write pair.
// -------------------------------------------------------------------------

/// Read framed Bolt messages from `reader`, dispatch them through a
/// [`Session`], and write framed responses back to `writer`. Terminates
/// when the reader hits clean EOF or the session enters [`State::Defunct`].
///
/// # Errors
///
/// * [`BoltError::Eof`] — partial message at the end of the input.
/// * Any other [`BoltError`] surfaced by the codec / framing layers.
///
/// Errors raised *by the Cypher engine* are converted to `FAILURE`
/// responses on the wire and do not abort the loop; the function only
/// returns `Err` for codec-level failures that make further dispatch
/// impossible.
pub fn run_session_sync<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    drevo: &Drevo,
) -> BoltResult<()> {
    run_session_sync_inner(reader, writer, Session::new(drevo))
}

/// Authenticating counterpart of [`run_session_sync`]. Every `HELLO` is
/// checked against `authenticator`; a denial is reported as a
/// `Neo.ClientError.Security.Unauthorized` `FAILURE` and the loop
/// returns (the connection is closed). Phase 11 task `00074`.
///
/// # Errors
///
/// Same as [`run_session_sync`] — only codec-level failures abort the
/// loop; auth denials and Cypher errors surface as `FAILURE` messages.
pub fn run_session_sync_with_auth<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    drevo: &Drevo,
    authenticator: &dyn Authenticator,
) -> BoltResult<()> {
    run_session_sync_inner(reader, writer, Session::with_auth(drevo, authenticator))
}

fn run_session_sync_inner<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    mut session: Session<'_>,
) -> BoltResult<()> {
    loop {
        let payload = match read_message(reader) {
            Ok(p) => p,
            Err(BoltError::Eof) => return Ok(()),
            Err(e) => return Err(e),
        };
        let (value, rest) = decode(&payload)?;
        if !rest.is_empty() {
            return Err(BoltError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "trailing bytes after Bolt message",
            )));
        }
        let msg = match decode_client(&value) {
            Ok(m) => m,
            Err(e) => {
                // A bad-tag / bad-shape message ends up as FAILURE on
                // the wire so the driver gets a deterministic answer
                // instead of a torn TCP connection.
                let reply = ServerMessage::Failure {
                    metadata: failure_metadata(codes::PROTOCOL_VIOLATION, &format!("{e}")),
                };
                write_server(&reply, writer)?;
                continue;
            }
        };
        let replies = session.handle(msg);
        for reply in &replies {
            write_server(reply, writer)?;
        }
        if session.state() == State::Defunct {
            return Ok(());
        }
    }
}

fn write_server<W: Write>(msg: &ServerMessage, writer: &mut W) -> BoltResult<()> {
    let mut payload = Vec::new();
    encode(&encode_server(msg), &mut payload)?;
    write_message(&payload, writer)
}

// -------------------------------------------------------------------------
// PackStream ↔ Cypher value conversions.
// -------------------------------------------------------------------------

fn params_to_cypher(
    params: BTreeMap<String, Value>,
) -> Result<HashMap<String, CypherValue>, String> {
    let mut out = HashMap::with_capacity(params.len());
    for (k, v) in params {
        out.insert(k, pack_to_cypher(v)?);
    }
    Ok(out)
}

fn pack_to_cypher(v: Value) -> Result<CypherValue, String> {
    match v {
        Value::Null => Ok(CypherValue::Null),
        Value::Boolean(b) => Ok(CypherValue::Bool(b)),
        Value::Integer(i) => Ok(CypherValue::Integer(i)),
        Value::Float(f) => Ok(CypherValue::Float(f)),
        Value::String(s) => Ok(CypherValue::String(s)),
        Value::Bytes(_) => {
            Err("Bytes parameters are not yet supported by the Cypher engine".into())
        }
        Value::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(pack_to_cypher(item)?);
            }
            Ok(CypherValue::List(out))
        }
        Value::Dictionary(map) => {
            let mut out = BTreeMap::new();
            for (k, v) in map {
                out.insert(k, pack_to_cypher(v)?);
            }
            Ok(CypherValue::Map(out))
        }
        Value::Structure { tag, fields } => structure_param_to_cypher(tag, fields),
    }
}

// Bolt PackStream temporal structure tags (Neo4j Bolt spec).
const TAG_DATE: u8 = 0x44; // 'D' — days since epoch
const TAG_DATETIME_LEGACY: u8 = 0x46; // 'F' — local seconds + nanos + tz offset
const TAG_DATETIME_UTC: u8 = 0x49; // 'I' — UTC seconds + nanos + tz offset (Bolt 5.0)
const TAG_LOCAL_DATETIME: u8 = 0x64; // 'd' — seconds + nanos, no zone

/// Decode a PackStream **temporal** parameter structure into an ISO-8601 string.
///
/// graphiti (and any neo4j-driver client) serialises `datetime` parameters as
/// PackStream temporal structures. drevo's Cypher engine has no native temporal
/// type, so we render them as ISO-8601 text — which round-trips cleanly on the
/// client via Python's `datetime.fromisoformat` (graphiti `helpers.parse_db_date`).
/// Non-temporal structures (Node, Relationship, Path, …) remain invalid params.
fn structure_param_to_cypher(tag: u8, fields: Vec<Value>) -> Result<CypherValue, String> {
    match tag {
        TAG_DATETIME_LEGACY | TAG_DATETIME_UTC => {
            let secs = struct_int(&fields, 0, tag)?;
            let nanos = struct_int(&fields, 1, tag)?;
            let offset = struct_int(&fields, 2, tag)?;
            // Legacy 0x46 stores local wall-clock seconds (UTC = secs - offset);
            // 0x49 stores the UTC instant directly (offset is display-only).
            let utc = if tag == TAG_DATETIME_LEGACY {
                secs - offset
            } else {
                secs
            };
            Ok(CypherValue::String(format_iso_datetime(utc, nanos, true)))
        }
        TAG_LOCAL_DATETIME => {
            let secs = struct_int(&fields, 0, tag)?;
            let nanos = struct_int(&fields, 1, tag)?;
            Ok(CypherValue::String(format_iso_datetime(secs, nanos, false)))
        }
        TAG_DATE => {
            let (y, m, d) = civil_from_days(struct_int(&fields, 0, tag)?);
            Ok(CypherValue::String(format!("{y:04}-{m:02}-{d:02}")))
        }
        _ => Err(format!(
            "PackStream Structure(0x{tag:02X}) is not a valid Cypher parameter"
        )),
    }
}

fn struct_int(fields: &[Value], idx: usize, tag: u8) -> Result<i64, String> {
    match fields.get(idx) {
        Some(Value::Integer(i)) => Ok(*i),
        _ => Err(format!(
            "PackStream Structure(0x{tag:02X}) has a missing or non-integer field at index {idx}"
        )),
    }
}

/// Format an instant (epoch seconds + nanoseconds) as ISO-8601. When `utc` is
/// set the `+00:00` zone suffix is appended (drevo normalises zoned DateTimes to
/// UTC); otherwise the value is a zone-less LocalDateTime and no suffix is added.
/// Rendered at microsecond precision — the neo4j driver sends nanos as
/// `micros * 1000`, so 6 fractional digits is exact and `fromisoformat`-safe.
fn format_iso_datetime(epoch_seconds: i64, nanos: i64, utc: bool) -> String {
    let days = epoch_seconds.div_euclid(86_400);
    let sod = epoch_seconds.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let suffix = if utc { "+00:00" } else { "" };
    let micros = nanos.rem_euclid(1_000_000_000) / 1000;
    if micros > 0 {
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{micros:06}{suffix}")
    } else {
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}{suffix}")
    }
}

/// Civil `(year, month, day)` from days since 1970-01-01, via Howard Hinnant's
/// `civil_from_days` (valid across the full range, no external date library).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn cypher_to_pack(v: CypherValue) -> Value {
    match v {
        CypherValue::Null => Value::Null,
        CypherValue::Bool(b) => Value::Boolean(b),
        CypherValue::Integer(i) => Value::Integer(i),
        CypherValue::Float(f) => Value::Float(f),
        CypherValue::String(s) => Value::String(s),
        CypherValue::List(items) => Value::List(items.into_iter().map(cypher_to_pack).collect()),
        CypherValue::Map(m) => {
            let mut out = BTreeMap::new();
            for (k, val) in m {
                out.insert(k, cypher_to_pack(val));
            }
            Value::Dictionary(out)
        }
        CypherValue::Node(node) => {
            let labels = Value::List(
                node.labels
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect::<Vec<_>>(),
            );
            let props = Value::Dictionary(
                node.properties
                    .iter()
                    .map(|(k, v)| (k.clone(), cypher_to_pack(v.clone())))
                    .collect(),
            );
            Value::Structure {
                tag: NODE_TAG,
                fields: vec![Value::Integer(node.id as i64), labels, props],
            }
        }
        CypherValue::Relationship(rel) => {
            let props = Value::Dictionary(
                rel.properties
                    .iter()
                    .map(|(k, v)| (k.clone(), cypher_to_pack(v.clone())))
                    .collect(),
            );
            Value::Structure {
                tag: RELATIONSHIP_TAG,
                fields: vec![
                    Value::Integer(rel.id as i64),
                    Value::Integer(rel.from_id as i64),
                    Value::Integer(rel.to_id as i64),
                    Value::String(rel.kind.clone()),
                    props,
                ],
            }
        }
        CypherValue::Path(path) => cypher_path_to_pack(&path),
    }
}

/// Encode a Cypher [`PathValue`](crate::cypher::executor::PathValue) as a Bolt
/// `Path` structure (tag `0x50`).
///
/// The Bolt wire form is three fields: a list of the **distinct** nodes, a list
/// of the **distinct** unbound relationships (tag `0x72` — id, type,
/// properties; endpoints are implied by the index sequence), and a flat list of
/// integer indices. The index list alternates `rel_index, node_index`: the
/// relationship index is 1-based into the unbound-relationship list and
/// **signed** (negative when the hop traverses the relationship against its
/// stored direction), and the node index is 0-based into the node list.
fn cypher_path_to_pack(path: &crate::cypher::executor::PathValue) -> Value {
    // Distinct nodes / relationships, preserving first-seen order.
    let mut uniq_nodes: Vec<u64> = Vec::new();
    let node_index = |id: u64, uniq: &mut Vec<u64>| -> i64 {
        match uniq.iter().position(|&n| n == id) {
            Some(i) => i as i64,
            None => {
                uniq.push(id);
                (uniq.len() - 1) as i64
            }
        }
    };

    let mut node_structs: Vec<Value> = Vec::new();
    let mut seen_nodes: Vec<u64> = Vec::new();
    for nv in &path.nodes {
        if !seen_nodes.contains(&nv.id) {
            seen_nodes.push(nv.id);
            node_structs.push(cypher_to_pack(CypherValue::Node(nv.clone())));
        }
    }

    let mut rel_structs: Vec<Value> = Vec::new();
    let mut rel_ids: Vec<u64> = Vec::new();
    for rv in &path.relationships {
        if !rel_ids.contains(&rv.id) {
            rel_ids.push(rv.id);
            let props = Value::Dictionary(
                rv.properties
                    .iter()
                    .map(|(k, v)| (k.clone(), cypher_to_pack(v.clone())))
                    .collect(),
            );
            rel_structs.push(Value::Structure {
                tag: UNBOUND_RELATIONSHIP_TAG,
                fields: vec![
                    Value::Integer(rv.id as i64),
                    Value::String(rv.kind.clone()),
                    props,
                ],
            });
        }
    }

    // Build the alternating index sequence by walking the path hop by hop.
    let mut indices: Vec<Value> = Vec::new();
    // Seed `uniq_nodes` with the head so node index 0 is the path start.
    if let Some(head) = path.nodes.first() {
        node_index(head.id, &mut uniq_nodes);
    }
    for (hop, rv) in path.relationships.iter().enumerate() {
        let from = &path.nodes[hop];
        let to = &path.nodes[hop + 1];
        let rel_pos = rel_ids.iter().position(|&id| id == rv.id).unwrap_or(0) as i64 + 1;
        // Positive when this hop goes with the stored direction.
        let signed = if rv.from_id == from.id {
            rel_pos
        } else {
            -rel_pos
        };
        indices.push(Value::Integer(signed));
        indices.push(Value::Integer(node_index(to.id, &mut uniq_nodes)));
    }

    Value::Structure {
        tag: PATH_TAG,
        fields: vec![
            Value::List(node_structs),
            Value::List(rel_structs),
            Value::List(indices),
        ],
    }
}

// -------------------------------------------------------------------------
// Failure metadata builders.
// -------------------------------------------------------------------------

fn failure_metadata(code: &str, message: &str) -> BTreeMap<String, Value> {
    let mut md = BTreeMap::new();
    md.insert("code".to_string(), Value::String(code.to_string()));
    md.insert("message".to_string(), Value::String(message.to_string()));
    md
}

/// Build a `FAILURE` metadata dictionary for protocol-level decode
/// errors (bad tag, mis-shaped fields). Exposed for callers that hand-
/// roll a Bolt loop on top of an async transport — see
/// [`crate::bolt::listener::accept_and_run_session`].
pub fn protocol_failure_metadata(message: &str) -> BTreeMap<String, Value> {
    failure_metadata(codes::PROTOCOL_VIOLATION, message)
}

fn parse_error_metadata(e: &ParseError) -> BTreeMap<String, Value> {
    failure_metadata(codes::SYNTAX_ERROR, &format!("{e}"))
}

fn exec_error_metadata(e: &ExecError) -> BTreeMap<String, Value> {
    let code = match e {
        ExecError::Unsupported { .. } | ExecError::EngineCapability { .. } => codes::UNSUPPORTED,
        ExecError::UnboundVariable { .. }
        | ExecError::InvalidCreate(_)
        | ExecError::InvalidMutation(_)
        | ExecError::TypeMismatch { .. }
        | ExecError::InvalidFunctionCall { .. }
        | ExecError::InvalidProcedureCall { .. }
        | ExecError::UnionMismatch { .. }
        | ExecError::InvalidRegex { .. } => codes::SEMANTIC_ERROR,
        ExecError::MissingParameter(_) => codes::PARAMETER_MISSING,
        ExecError::Storage(_) => codes::STORAGE,
    };
    failure_metadata(code, &format!("{e}"))
}

fn extract_n(extra: &BTreeMap<String, Value>) -> i64 {
    match extra.get("n") {
        Some(Value::Integer(i)) => *i,
        _ => -1,
    }
}

// -------------------------------------------------------------------------
// Inline tests.
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_n_defaults_to_all_when_missing() {
        let empty = BTreeMap::new();
        assert_eq!(extract_n(&empty), -1);
    }

    #[test]
    fn extract_n_reads_positive_value() {
        let mut m = BTreeMap::new();
        m.insert("n".to_string(), Value::Integer(5));
        assert_eq!(extract_n(&m), 5);
    }

    #[test]
    fn server_agent_is_neo4j_compatible() {
        // The official Neo4j drivers reject any server whose `server` agent
        // does not start with `Neo4j/` (UnsupportedServerProduct). drevo's
        // Bolt surface is a Neo4j-compatible drop-in, so the agent must carry
        // that prefix while still identifying drevo.
        let db = crate::db::Drevo::open_in_memory().unwrap();
        let session = Session::new(&db);
        let agent = session.server_agent();
        assert!(
            agent.starts_with("Neo4j/"),
            "bolt agent must start with `Neo4j/` for driver compatibility, got {agent:?}"
        );
        assert!(
            agent.contains("drevo"),
            "bolt agent should still identify drevo, got {agent:?}"
        );
    }

    #[test]
    fn batched_pull_does_not_drop_rows_across_batches() {
        // Regression: `handle_pull` checked for remaining rows with
        // `.by_ref().peekable().peek()`, which pulled the next row into a
        // throwaway `Peekable` and then dropped it — silently losing one row
        // at every PULL batch boundary. Pull three rows one at a time and
        // assert none go missing.
        let db = crate::db::Drevo::open_in_memory().unwrap();
        let mut session = Session::new(&db);
        session.state = State::Ready; // skip the HELLO handshake for the unit
        let run = session.handle_run(
            "UNWIND [1, 2, 3] AS x RETURN x".to_string(),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        assert!(
            matches!(run.last(), Some(ServerMessage::Success { .. })),
            "RUN should succeed, got {run:?}"
        );

        let mut n1 = BTreeMap::new();
        n1.insert("n".to_string(), Value::Integer(1));
        let mut records: Vec<i64> = Vec::new();
        for _ in 0..3 {
            for msg in session.handle_pull(n1.clone()) {
                if let ServerMessage::Record { fields } = msg {
                    if let Some(Value::Integer(i)) = fields.first() {
                        records.push(*i);
                    }
                }
            }
        }
        assert_eq!(
            records,
            vec![1, 2, 3],
            "every row must survive batched PULL — none dropped by the exhaustion check"
        );
    }

    #[test]
    fn cypher_path_packs_as_bolt_path_structure() {
        use std::collections::BTreeMap;
        use std::sync::Arc;
        let node = |id: u64| {
            Arc::new(executor::NodeValue {
                id,
                uuid: [0; 16],
                labels: vec!["N".into()],
                properties: BTreeMap::new(),
            })
        };
        let path = executor::PathValue {
            nodes: vec![node(1), node(2)],
            relationships: vec![Arc::new(executor::RelationshipValue {
                id: 7,
                uuid: [0; 16],
                from_id: 1,
                to_id: 2,
                kind: "R".into(),
                properties: BTreeMap::new(),
            })],
        };
        let packed = cypher_to_pack(CypherValue::Path(Arc::new(path)));
        match packed {
            Value::Structure { tag, fields } => {
                assert_eq!(tag, PATH_TAG);
                assert_eq!(fields.len(), 3, "nodes, rels, indices");
                // Two distinct nodes, one unbound relationship.
                match (&fields[0], &fields[1], &fields[2]) {
                    (Value::List(ns), Value::List(rs), Value::List(idx)) => {
                        assert_eq!(ns.len(), 2);
                        assert_eq!(rs.len(), 1);
                        // One forward hop: rel index +1, target node index 1.
                        assert_eq!(idx, &vec![Value::Integer(1), Value::Integer(1)]);
                        // The unbound relationship omits endpoints (3 fields).
                        match &rs[0] {
                            Value::Structure { tag, fields } => {
                                assert_eq!(*tag, UNBOUND_RELATIONSHIP_TAG);
                                assert_eq!(fields.len(), 3);
                            }
                            other => panic!("expected unbound rel structure, got {other:?}"),
                        }
                    }
                    other => panic!("expected three lists, got {other:?}"),
                }
            }
            other => panic!("expected a Path structure, got {other:?}"),
        }
    }

    #[test]
    fn pack_to_cypher_translates_basic_types() {
        assert!(matches!(
            pack_to_cypher(Value::Null).unwrap(),
            CypherValue::Null
        ));
        assert!(matches!(
            pack_to_cypher(Value::Boolean(true)).unwrap(),
            CypherValue::Bool(true)
        ));
        assert!(matches!(
            pack_to_cypher(Value::Integer(7)).unwrap(),
            CypherValue::Integer(7)
        ));
    }

    #[test]
    fn pack_to_cypher_rejects_structure_parameter() {
        // Non-temporal structures (here a Node, 0x4E) are still not valid params.
        let err = pack_to_cypher(Value::Structure {
            tag: 0x4E,
            fields: vec![],
        })
        .unwrap_err();
        assert!(err.contains("Structure"));
    }

    fn cypher_string(v: CypherValue) -> String {
        match v {
            CypherValue::String(s) => s,
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[test]
    fn pack_to_cypher_decodes_datetime_epoch_zero() {
        // DateTime (0x46): seconds, nanos, tz_offset. UTC epoch 0, offset 0.
        let v = pack_to_cypher(Value::Structure {
            tag: 0x46,
            fields: vec![Value::Integer(0), Value::Integer(0), Value::Integer(0)],
        })
        .unwrap();
        assert_eq!(cypher_string(v), "1970-01-01T00:00:00+00:00");
    }

    #[test]
    fn pack_to_cypher_decodes_datetime_known_instant_with_nanos() {
        // 1_000_000_000s since epoch = 2001-09-09T01:46:40Z; 123_456_000 ns -> .123456
        let v = pack_to_cypher(Value::Structure {
            tag: 0x46,
            fields: vec![
                Value::Integer(1_000_000_000),
                Value::Integer(123_456_000),
                Value::Integer(0),
            ],
        })
        .unwrap();
        assert_eq!(cypher_string(v), "2001-09-09T01:46:40.123456+00:00");
    }

    #[test]
    fn pack_to_cypher_datetime_0x46_subtracts_offset_to_utc() {
        // Legacy 0x46: `seconds` is local wall-clock; UTC instant = seconds - offset.
        // 1_000_000_000 local at +01:00 -> 999_996_400 UTC = 2001-09-09T00:46:40Z.
        let v = pack_to_cypher(Value::Structure {
            tag: 0x46,
            fields: vec![
                Value::Integer(1_000_000_000),
                Value::Integer(0),
                Value::Integer(3600),
            ],
        })
        .unwrap();
        assert_eq!(cypher_string(v), "2001-09-09T00:46:40+00:00");
    }

    #[test]
    fn pack_to_cypher_datetime_0x49_is_already_utc() {
        // Bolt 5 UTC DateTime (0x49): `seconds` is the UTC instant; offset is display-only.
        let v = pack_to_cypher(Value::Structure {
            tag: 0x49,
            fields: vec![
                Value::Integer(1_000_000_000),
                Value::Integer(0),
                Value::Integer(3600),
            ],
        })
        .unwrap();
        assert_eq!(cypher_string(v), "2001-09-09T01:46:40+00:00");
    }

    #[test]
    fn pack_to_cypher_decodes_local_datetime_as_naive() {
        // LocalDateTime (0x64): seconds, nanos — no zone, so no offset suffix.
        let v = pack_to_cypher(Value::Structure {
            tag: 0x64,
            fields: vec![Value::Integer(0), Value::Integer(0)],
        })
        .unwrap();
        assert_eq!(cypher_string(v), "1970-01-01T00:00:00");
    }

    #[test]
    fn pack_to_cypher_decodes_date() {
        // Date (0x44): days since epoch. day 0 = 1970-01-01, day 1 = 1970-01-02.
        assert_eq!(
            cypher_string(
                pack_to_cypher(Value::Structure {
                    tag: 0x44,
                    fields: vec![Value::Integer(0)],
                })
                .unwrap()
            ),
            "1970-01-01"
        );
        assert_eq!(
            cypher_string(
                pack_to_cypher(Value::Structure {
                    tag: 0x44,
                    fields: vec![Value::Integer(1)],
                })
                .unwrap()
            ),
            "1970-01-02"
        );
    }

    #[test]
    fn pack_to_cypher_datetime_malformed_fields_error() {
        // Wrong arity / non-integer fields must error, not panic.
        assert!(pack_to_cypher(Value::Structure {
            tag: 0x46,
            fields: vec![Value::Integer(0)],
        })
        .is_err());
        assert!(pack_to_cypher(Value::Structure {
            tag: 0x46,
            fields: vec![
                Value::String('x'.to_string()),
                Value::Integer(0),
                Value::Integer(0),
            ],
        })
        .is_err());
    }

    #[test]
    fn cypher_to_pack_roundtrips_simple_types() {
        assert_eq!(cypher_to_pack(CypherValue::Null), Value::Null);
        assert_eq!(cypher_to_pack(CypherValue::Integer(42)), Value::Integer(42));
    }

    #[test]
    fn failure_metadata_carries_code_and_message_keys() {
        let md = failure_metadata("X", "msg");
        assert_eq!(md.get("code"), Some(&Value::String("X".to_string())));
        assert_eq!(md.get("message"), Some(&Value::String("msg".to_string())));
    }

    #[test]
    fn require_arity_rejects_mismatch() {
        let err = require_arity(&[Value::Null], 0, "X").unwrap_err();
        assert!(matches!(err, BoltError::Io(_)));
    }

    #[test]
    fn decode_client_rejects_non_structure() {
        let err = decode_client(&Value::Integer(1)).unwrap_err();
        assert!(matches!(err, BoltError::Io(_)));
    }

    #[test]
    fn decode_client_rejects_unknown_tag() {
        let err = decode_client(&Value::Structure {
            tag: 0xEE,
            fields: vec![],
        })
        .unwrap_err();
        assert!(matches!(err, BoltError::UnknownMarker(0xEE)));
    }

    #[test]
    fn encode_server_record_wraps_fields_in_list() {
        let v = encode_server(&ServerMessage::Record {
            fields: vec![Value::Integer(1)],
        });
        if let Value::Structure { tag, fields } = v {
            assert_eq!(tag, RECORD);
            assert!(matches!(fields[0], Value::List(_)));
        } else {
            panic!("expected structure");
        }
    }
}
