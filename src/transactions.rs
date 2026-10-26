//! The running statements, open transactions and client connections of a
//! server, for `SHOW TRANSACTIONS` / `TERMINATE TRANSACTIONS` and `SHOW
//! CONNECTIONS` / `TERMINATE CONNECTIONS`.
//!
//! Every statement a [`NativeService`](crate::native_service::NativeService)
//! runs is listed while it runs, and every explicit transaction while it is
//! open, under a Neo4j-style id such as `drevo-transaction-42`. The databases
//! of one server share one list. Terminating marks the entry:
//!
//! - a running statement stops at its next cancellation check, the one the
//!   statement timeout uses, with
//!   [`ExecError::Terminated`](crate::cypher::executor::ExecError::Terminated);
//! - an explicit transaction refuses its next statement and its commit, and
//!   is rolled back.
//!
//! Who runs a statement comes from [`scope`](crate::transactions::scope), which each protocol sets around
//! the statements it runs: `bolt`, `http`, `grpc` or `flight`, the client's
//! address, the signed-in user and its connection.
//!
//! A Bolt connection is listed from its handshake until it closes, under an
//! id such as `bolt-7`, with the driver's user agent and the user it logged
//! in as. The Web UI's signed-in sessions are listed as `http-N`; see
//! [`Transactions::set_http_sessions`](crate::transactions::Transactions::set_http_sessions).
//! Terminating a connection closes it and rolls back its transaction.
//!
//! ```
//! use std::collections::HashMap;
//! use drevo::native_service::NativeService;
//! use drevo::transactions::{scope, Caller};
//!
//! let svc = NativeService::in_memory();
//! let q = "SHOW TRANSACTIONS YIELD username, currentQuery";
//! let caller = Caller { surface: "http", user: Some("ann".into()), ..Caller::default() };
//! let rows = scope(caller, q, || {
//!     svc.execute(&drevo::cypher::parser::parse(q).unwrap(), HashMap::new())
//! })
//! .unwrap()
//! .rows;
//! assert_eq!(rows.len(), 1); // the SHOW itself
//! ```

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Who runs a statement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caller {
    /// `bolt`, `http`, `grpc` or `flight`; empty when embedded.
    pub surface: &'static str,
    /// The client's `host:port`, when known.
    pub client: Option<String>,
    /// The signed-in user, when logins are required.
    pub user: Option<String>,
    /// The connection the statement came in on (`bolt-7`), when listed.
    pub connection: Option<String>,
}

thread_local! {
    /// The caller and query text of the statement this thread is running.
    static CURRENT: RefCell<Option<(Caller, String)>> = const { RefCell::new(None) };
}

/// Run `work` (one statement, `query`) on behalf of `caller`: the statements
/// it runs are listed with them. The previous scope is restored afterwards.
pub fn scope<R>(caller: Caller, query: &str, work: impl FnOnce() -> R) -> R {
    let previous = CURRENT.with(|c| c.replace(Some((caller, query.to_string()))));
    struct Restore(Option<(Caller, String)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            CURRENT.with(|c| *c.borrow_mut() = previous);
        }
    }
    let _restore = Restore(previous);
    work()
}

/// The caller of the statement this thread runs, if a protocol set one.
#[must_use]
pub fn current_caller() -> Option<Caller> {
    CURRENT.with(|c| c.borrow().as_ref().map(|(caller, _)| caller.clone()))
}

/// The query text of the statement this thread runs, if a protocol set one.
pub(crate) fn current_query() -> Option<String> {
    CURRENT.with(|c| c.borrow().as_ref().map(|(_, query)| query.clone()))
}

fn current() -> (Caller, Option<String>) {
    CURRENT.with(|c| match c.borrow().as_ref() {
        Some((caller, query)) => (caller.clone(), Some(query.clone())),
        None => (Caller::default(), None),
    })
}

/// A listed statement or transaction, as `SHOW TRANSACTIONS` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionInfo {
    /// `<database>-transaction-<n>`.
    pub id: String,
    /// The database it runs in.
    pub database: String,
    /// Who runs it.
    pub user: Option<String>,
    /// The client's address.
    pub client: Option<String>,
    /// The protocol: `bolt`, `http`, `grpc`, `flight`, or empty.
    pub surface: &'static str,
    /// The connection it came in on, when listed (`bolt-7`).
    pub connection: Option<String>,
    /// An explicit transaction (`BEGIN` … `COMMIT`), not a single statement.
    pub explicit: bool,
    /// When it began, in milliseconds since the Unix epoch.
    pub started_ms: i64,
    /// How long it has run.
    pub elapsed: Duration,
    /// The statement it runs now; `None` for an idle explicit transaction.
    pub query: Option<String>,
    /// How long the current statement has run.
    pub query_elapsed: Option<Duration>,
    /// Whether it was terminated and has not finished yet.
    pub terminated: bool,
    /// How many writes an explicit transaction buffers, as of its last
    /// statement; `None` for a single statement.
    pub buffered_writes: Option<u64>,
}

/// One listed statement or transaction.
#[derive(Debug)]
pub(crate) struct Entry {
    number: u64,
    database: String,
    caller: Caller,
    explicit: bool,
    started_ms: i64,
    started: Instant,
    query: Mutex<Option<(String, Instant)>>,
    terminated: Arc<AtomicBool>,
    /// The writes an explicit transaction buffers (see
    /// [`TransactionInfo::buffered_writes`]).
    writes: std::sync::atomic::AtomicU64,
}

impl Entry {
    /// The flag a running statement watches.
    pub(crate) fn terminated_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.terminated)
    }

    /// Whether it was terminated.
    pub(crate) fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Relaxed)
    }

    /// Record how many writes the explicit transaction buffers.
    pub(crate) fn set_buffered_writes(&self, writes: u64) {
        self.writes.store(writes, Ordering::Relaxed);
    }

    /// Show `query` as its current statement, or none.
    pub(crate) fn set_query(&self, query: Option<String>) {
        *self.query.lock().unwrap_or_else(|e| e.into_inner()) = query.map(|q| (q, Instant::now()));
    }

    fn info(&self) -> TransactionInfo {
        let query = self.query.lock().unwrap_or_else(|e| e.into_inner());
        TransactionInfo {
            id: format!("{}-transaction-{}", self.database, self.number),
            database: self.database.clone(),
            user: self.caller.user.clone(),
            client: self.caller.client.clone(),
            surface: self.caller.surface,
            connection: self.caller.connection.clone(),
            explicit: self.explicit,
            started_ms: self.started_ms,
            elapsed: self.started.elapsed(),
            query: query.as_ref().map(|(q, _)| q.clone()),
            query_elapsed: query.as_ref().map(|(_, at)| at.elapsed()),
            terminated: self.is_terminated(),
            buffered_writes: self.explicit.then(|| self.writes.load(Ordering::Relaxed)),
        }
    }
}

/// The running statements, open transactions and connections of one server.
#[derive(Default)]
pub struct Transactions {
    next: AtomicU64,
    running: Mutex<BTreeMap<u64, Arc<Entry>>>,
    next_connection: AtomicU64,
    connections: Mutex<BTreeMap<u64, Arc<Connection>>>,
    http_sessions: std::sync::RwLock<Option<Arc<dyn HttpSessions>>>,
}

impl std::fmt::Debug for Transactions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transactions")
            .field("running", &self.lock().len())
            .field("connections", &self.connections_lock().len())
            .finish_non_exhaustive()
    }
}

/// A listed client connection, as `SHOW CONNECTIONS` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// `bolt-7`, `http-3`.
    pub id: String,
    /// `bolt` or `http`.
    pub connector: &'static str,
    /// When it was opened, in milliseconds since the Unix epoch.
    pub connected_ms: i64,
    /// The user it is logged in as.
    pub user: Option<String>,
    /// The driver or browser that opened it.
    pub user_agent: Option<String>,
    /// The client's address.
    pub client: Option<String>,
}

/// The Web UI's signed-in sessions, listed as `http` connections.
pub trait HttpSessions: Send + Sync {
    /// The live sessions, by number.
    fn list(&self) -> Vec<ConnectionInfo>;
    /// End session `number`; its user, or `None` when there is no such
    /// session.
    fn end(&self, number: u64) -> Option<Option<String>>;
}

/// One open connection.
pub(crate) struct Connection {
    number: u64,
    connector: &'static str,
    connected_ms: i64,
    details: Mutex<ConnectionDetails>,
    closed: AtomicBool,
    /// Wakes the connection's loop to close it.
    closer: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

#[derive(Default)]
struct ConnectionDetails {
    user: Option<String>,
    user_agent: Option<String>,
    client: Option<String>,
}

impl Connection {
    /// `bolt-7`.
    pub(crate) fn id(&self) -> String {
        format!("{}-{}", self.connector, self.number)
    }

    /// Record who logged in on it (`None` after a log-off).
    pub(crate) fn set_user(&self, user: Option<String>) {
        self.details().user = user;
    }

    /// Record the driver's user agent.
    pub(crate) fn set_user_agent(&self, agent: Option<String>) {
        self.details().user_agent = agent;
    }

    /// Record the client's address.
    pub(crate) fn set_client(&self, client: Option<String>) {
        self.details().client = client;
    }

    /// Call `close` when the connection is terminated.
    #[cfg(feature = "http")]
    pub(crate) fn on_close(&self, close: impl Fn() + Send + Sync + 'static) {
        *self.closer.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(close));
        if self.is_closed() {
            self.wake();
        }
    }

    /// Whether `TERMINATE CONNECTIONS` closed it.
    #[cfg(feature = "http")]
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    fn wake(&self) {
        if let Some(close) = self
            .closer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            close();
        }
    }

    fn details(&self) -> std::sync::MutexGuard<'_, ConnectionDetails> {
        self.details.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn info(&self) -> ConnectionInfo {
        let d = self.details();
        ConnectionInfo {
            id: self.id(),
            connector: self.connector,
            connected_ms: self.connected_ms,
            user: d.user.clone(),
            user_agent: d.user_agent.clone(),
            client: d.client.clone(),
        }
    }
}

/// Removes its connection from the list when dropped.
pub(crate) struct ListedConnection {
    owner: Arc<Transactions>,
    connection: Arc<Connection>,
}

impl ListedConnection {
    pub(crate) fn connection(&self) -> &Arc<Connection> {
        &self.connection
    }
}

impl Drop for ListedConnection {
    fn drop(&mut self) {
        self.owner
            .connections_lock()
            .remove(&self.connection.number);
    }
}

/// Removes its entry from the list when dropped.
pub(crate) struct Listed {
    owner: Arc<Transactions>,
    entry: Arc<Entry>,
}

impl Listed {
    pub(crate) fn entry(&self) -> &Arc<Entry> {
        &self.entry
    }
}

impl Drop for Listed {
    fn drop(&mut self) {
        self.owner.remove(self.entry.number);
    }
}

impl Transactions {
    /// An empty list.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// List a statement (or, `explicit`, a transaction) the current
    /// [`scope`]'s caller starts on `database`, until the returned guard
    /// drops. A statement shows the scope's query text.
    pub(crate) fn list_new(self: &Arc<Self>, database: &str, explicit: bool) -> Listed {
        let (caller, query) = current();
        let number = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let entry = Arc::new(Entry {
            number,
            database: database.to_string(),
            caller,
            explicit,
            started_ms: now_ms(),
            started: Instant::now(),
            query: Mutex::new(None),
            terminated: Arc::new(AtomicBool::new(false)),
            writes: std::sync::atomic::AtomicU64::new(0),
        });
        if !explicit {
            entry.set_query(query);
        }
        self.lock().insert(number, Arc::clone(&entry));
        Listed {
            owner: Arc::clone(self),
            entry,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Arc<Entry>>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn remove(&self, number: u64) {
        self.lock().remove(&number);
    }

    /// Everything running now, oldest first.
    #[must_use]
    pub fn list(&self) -> Vec<TransactionInfo> {
        let entries: Vec<Arc<Entry>> = self.lock().values().cloned().collect();
        entries.iter().map(|e| e.info()).collect()
    }

    /// The statement or transaction `id`, if it runs.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<TransactionInfo> {
        let number = parse_id(id)?;
        let entry = self.lock().get(&number).cloned()?;
        let info = entry.info();
        (info.id == id).then_some(info)
    }

    /// List a `connector` connection (`bolt`) until the returned guard drops.
    pub(crate) fn open_connection(self: &Arc<Self>, connector: &'static str) -> ListedConnection {
        let number = self.next_connection.fetch_add(1, Ordering::Relaxed) + 1;
        let connection = Arc::new(Connection {
            number,
            connector,
            connected_ms: now_ms(),
            details: Mutex::new(ConnectionDetails::default()),
            closed: AtomicBool::new(false),
            closer: Mutex::new(None),
        });
        self.connections_lock()
            .insert(number, Arc::clone(&connection));
        ListedConnection {
            owner: Arc::clone(self),
            connection,
        }
    }

    /// List the Web UI's signed-in sessions as `http` connections.
    pub fn set_http_sessions(&self, sessions: Arc<dyn HttpSessions>) {
        *self
            .http_sessions
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(sessions);
    }

    fn http(&self) -> Option<Arc<dyn HttpSessions>> {
        self.http_sessions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn connections_lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Arc<Connection>>> {
        self.connections.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The open connections: Bolt connections oldest first, then the Web
    /// UI's sessions.
    #[must_use]
    pub fn connections(&self) -> Vec<ConnectionInfo> {
        let open: Vec<Arc<Connection>> = self.connections_lock().values().cloned().collect();
        let mut out: Vec<ConnectionInfo> = open.iter().map(|c| c.info()).collect();
        if let Some(http) = self.http() {
            out.extend(http.list());
        }
        out
    }

    /// The connection `id`, if it is open.
    #[must_use]
    pub fn connection(&self, id: &str) -> Option<ConnectionInfo> {
        self.connections().into_iter().find(|c| c.id == id)
    }

    /// Close connection `id`: a Bolt connection is closed and its
    /// transaction rolled back (a running statement stops), a Web UI session
    /// ends. Returns what it was, or `None` when no connection has that id.
    pub fn terminate_connection(&self, id: &str) -> Option<ConnectionInfo> {
        let (connector, number) = id.rsplit_once('-')?;
        let number: u64 = number.parse().ok()?;
        if connector == "http" {
            let info = self.connection(id)?;
            self.http()?.end(number)?;
            return Some(info);
        }
        let connection = self.connections_lock().get(&number).cloned()?;
        if connection.id() != id {
            return None;
        }
        connection.closed.store(true, Ordering::Relaxed);
        // Its running statement and open transaction stop too.
        let entries: Vec<Arc<Entry>> = self.lock().values().cloned().collect();
        for entry in entries {
            if entry.caller.connection.as_deref() == Some(id) {
                entry.terminated.store(true, Ordering::Relaxed);
            }
        }
        connection.wake();
        Some(connection.info())
    }

    /// Terminate `id`: a running statement stops, an explicit transaction is
    /// rolled back. Returns what it was, or `None` when nothing runs under
    /// that id.
    pub fn terminate(&self, id: &str) -> Option<TransactionInfo> {
        let number = parse_id(id)?;
        let entry = self.lock().get(&number).cloned()?;
        if entry.info().id != id {
            return None;
        }
        entry.terminated.store(true, Ordering::Relaxed);
        Some(entry.info())
    }
}

/// The number at the end of `<database>-transaction-<n>`.
fn parse_id(id: &str) -> Option<u64> {
    let (_, number) = id.rsplit_once("-transaction-")?;
    number.parse().ok()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
