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
//! ## Not in scope
//!
//! - TLS — task `00073`.
//! - Authentication (`scheme` / `principal` / `credentials` fields of
//!   `HELLO`) — task `00074`. Today the session accepts any extras and
//!   transitions to `Ready` regardless.
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
    /// Returned for a second `BEGIN` while one transaction is already
    /// in flight on the same `Drevo` handle. Phase 11 task `00072` is
    /// single-writer; proper multi-tx isolation lands with MVCC.
    pub const TRANSACTION_OUTDATED: &str = "Neo.TransientError.Transaction.Outdated";
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
}

struct PendingResult {
    rows: std::vec::IntoIter<Vec<CypherValue>>,
}

impl<'a> Session<'a> {
    /// Create a new session bound to `drevo`. Starts in
    /// [`State::Connected`]; the caller must send `HELLO` next.
    pub fn new(drevo: &'a Drevo) -> Self {
        let id = CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self {
            drevo,
            state: State::Connected,
            server_agent: format!("drevo/{}", env!("CARGO_PKG_VERSION")),
            connection_id: format!("drevo-bolt-{id}"),
            pending: None,
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

    fn handle_hello(&mut self, _extra: BTreeMap<String, Value>) -> Vec<ServerMessage> {
        if self.state != State::Connected {
            self.state = State::Failed;
            return vec![ServerMessage::Failure {
                metadata: failure_metadata(
                    codes::REQUEST_INVALID,
                    "HELLO received outside CONNECTED state",
                ),
            }];
        }
        // 00074 will validate the `scheme` / `principal` / `credentials`
        // tuple. For now, accept any extras.
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

    fn handle_goodbye(&mut self) -> Vec<ServerMessage> {
        // An explicit transaction left open at GOODBYE is rolled back
        // so the next session that connects through the same `Drevo`
        // handle isn't blocked by a stale journal slot. Errors during
        // rollback are silently swallowed — the client is already
        // walking away and no reply is delivered for `GOODBYE`.
        if self.drevo.is_tx_active() {
            let _ = self.drevo.tx_rollback();
        }
        self.state = State::Defunct;
        self.pending = None;
        Vec::new()
    }

    fn handle_reset(&mut self) -> Vec<ServerMessage> {
        self.pending = None;
        // RESET inside an explicit transaction rolls it back, matching
        // the Neo4j driver contract: the transaction is gone when the
        // session returns to READY. Any rollback failure surfaces as
        // FAILURE so the driver gets a deterministic signal rather
        // than a torn connection.
        if self.drevo.is_tx_active() {
            if let Err(e) = self.drevo.tx_rollback() {
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
        match self.drevo.tx_begin() {
            Ok(()) => {
                self.state = State::TxReady;
                vec![ServerMessage::Success {
                    metadata: BTreeMap::new(),
                }]
            }
            Err(e) => {
                self.state = State::Failed;
                vec![ServerMessage::Failure {
                    metadata: failure_metadata(codes::TRANSACTION_OUTDATED, &format!("{e}")),
                }]
            }
        }
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
        match self.drevo.tx_commit() {
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
        match self.drevo.tx_rollback() {
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
        // Execute (already returns a fully-materialised ExecResult).
        let result = match executor::execute(&ast, self.drevo, cypher_params) {
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
                // Pulled the requested batch — peek to see whether more
                // rows are still queued.
                exhausted = pending.rows.by_ref().peekable().peek().is_none();
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
                exhausted = pending.rows.by_ref().peekable().peek().is_none();
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
    let mut session = Session::new(drevo);
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
        Value::Structure { tag, .. } => Err(format!(
            "PackStream Structure(0x{tag:02X}) is not a valid Cypher parameter"
        )),
    }
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
        ExecError::Unsupported { .. } => codes::UNSUPPORTED,
        ExecError::UnboundVariable { .. }
        | ExecError::InvalidCreate(_)
        | ExecError::InvalidMutation(_)
        | ExecError::TypeMismatch { .. } => codes::SEMANTIC_ERROR,
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
        let err = pack_to_cypher(Value::Structure {
            tag: 0x4E,
            fields: vec![],
        })
        .unwrap_err();
        assert!(err.contains("Structure"));
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
