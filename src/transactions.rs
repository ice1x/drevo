//! The running statements and open transactions of a server, for `SHOW
//! TRANSACTIONS` and `TERMINATE TRANSACTIONS`.
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
//! address and the signed-in user.
//!
//! ```
//! use std::collections::HashMap;
//! use drevo::native_service::NativeService;
//! use drevo::transactions::{scope, Caller};
//!
//! let svc = NativeService::in_memory();
//! let q = "SHOW TRANSACTIONS YIELD username, currentQuery";
//! let caller = Caller { surface: "http", client: None, user: Some("ann".into()) };
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
            explicit: self.explicit,
            started_ms: self.started_ms,
            elapsed: self.started.elapsed(),
            query: query.as_ref().map(|(q, _)| q.clone()),
            query_elapsed: query.as_ref().map(|(_, at)| at.elapsed()),
            terminated: self.is_terminated(),
        }
    }
}

/// The running statements and open transactions of one server.
#[derive(Debug, Default)]
pub struct Transactions {
    next: AtomicU64,
    running: Mutex<BTreeMap<u64, Arc<Entry>>>,
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
