//! Named-database catalog — multiple [`Drevo`](crate::db::Drevo) databases
//! in one process.
//!
//! Each *named* database is an independent in-memory [`Drevo`](crate::db::Drevo)
//! handle; the catalog owns them and multiplexes lookups by name. One process,
//! many databases, one handle each.
//!
//! ## Naming
//!
//! Names are validated ([`is_valid_name`](crate::catalog::is_valid_name)):
//! non-empty, at most [`MAX_NAME_LEN`](crate::catalog::MAX_NAME_LEN) bytes,
//! and drawn from `[A-Za-z0-9_-]`. That character set is deliberately narrow —
//! it is exactly what is safe as an HTTP header / query value the Web UI passes
//! back unescaped.
//!
//! ## Lifecycle
//!
//! [`Catalog::from_default`](crate::catalog::Catalog::from_default) wraps an
//! already-open handle as the `default` database;
//! [`Catalog::open_in_memory`](crate::catalog::Catalog::open_in_memory) gives an
//! all-ephemeral catalog. Handles open lazily on first
//! [`Catalog::get`](crate::catalog::Catalog::get) /
//! [`Catalog::create`](crate::catalog::Catalog::create) and are cached.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};

use crate::db::Drevo;
use crate::error::DrevoError;

/// The always-present database. Named `drevo`.
pub const DEFAULT_DB: &str = "drevo";

/// Maximum database-name length in bytes.
pub const MAX_NAME_LEN: usize = 64;

/// Errors surfaced by the [`Catalog`]. Deliberately a small, self-contained
/// enum (not a new [`DrevoError`] variant) so the catalog stays decoupled
/// from the graph error channel; the HTTP layer maps these to status codes.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The name is empty, too long, or contains characters outside
    /// `[A-Za-z0-9_-]`.
    #[error("invalid database name {0:?} — use 1..={max} chars of [A-Za-z0-9_-]", max = MAX_NAME_LEN)]
    InvalidName(String),

    /// No database with this name is registered in the catalog.
    #[error("database not found: {0}")]
    NotFound(String),

    /// [`Catalog::create`] was asked for a name that already exists.
    #[error("database already exists: {0}")]
    AlreadyExists(String),

    /// Opening the underlying [`Drevo`] handle failed.
    #[error("failed to open database {name}: {source}")]
    Open {
        /// The database name whose handle could not be opened.
        name: String,
        /// The underlying storage/database error.
        #[source]
        source: DrevoError,
    },
}

/// True when `name` is a legal database name: 1..=[`MAX_NAME_LEN`] bytes of
/// ASCII letters, digits, `_`, or `-`.
#[must_use]
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// How a catalog opens the databases it manages.
enum Backing {
    /// Ephemeral: every database is a fresh in-memory [`Drevo`].
    Memory,
}

/// A registry of named [`Drevo`] databases sharing one process.
///
/// Cloneable-by-`Arc`: wrap in `Arc<Catalog>` and share across the HTTP and
/// Bolt servers. Internally guarded by `RwLock`s, so `&self` is all any
/// method needs.
pub struct Catalog {
    backing: Backing,
    /// The always-present [`DEFAULT_DB`] handle, held in its own field (not
    /// the `open` cache) so [`Catalog::default_db`] is infallible — no lock,
    /// no `Option`, no panic path. [`Catalog::get`] special-cases the default
    /// name to return this.
    default: Arc<Drevo>,
    /// Every registered database name (whether its handle is open yet or
    /// not). Sorted iteration comes for free from `BTreeSet`.
    known: RwLock<BTreeSet<String>>,
    /// Cache of opened handles, keyed by name. Populated lazily so the
    /// single-handle-per-file invariant is never violated by a double open.
    open: RwLock<HashMap<String, Arc<Drevo>>>,
}

/// A `known` set seeded with just the default database. The default handle
/// itself lives in the [`Catalog::default`] field, not the `open` cache, so
/// there is exactly one extra reference to it (see [`Catalog::get`], which
/// special-cases the default name).
fn seed_known() -> BTreeSet<String> {
    let mut known = BTreeSet::new();
    known.insert(DEFAULT_DB.to_string());
    known
}

impl Catalog {
    /// Build a catalog around an already-open [`Drevo`] handle, installed as
    /// [`DEFAULT_DB`]. Any databases [`create`](Self::create)d afterwards are
    /// in-memory. This is the back-compat path for callers (and tests) that
    /// already hold a single handle and just want it exposed as the default
    /// database of a one-entry catalog.
    #[must_use]
    pub fn from_default(db: Arc<Drevo>) -> Self {
        Self {
            backing: Backing::Memory,
            default: db,
            known: RwLock::new(seed_known()),
            open: RwLock::new(HashMap::new()),
        }
    }

    /// Open an all-ephemeral catalog: every database (including `default`)
    /// is a fresh in-memory [`Drevo`]. For tests and `wasm32`.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Open`] if the `default` in-memory database
    /// cannot be constructed (effectively never).
    pub fn open_in_memory() -> Result<Self, CatalogError> {
        let default = Arc::new(
            Drevo::open_in_memory().map_err(|source| CatalogError::Open {
                name: DEFAULT_DB.to_string(),
                source,
            })?,
        );
        Ok(Self {
            backing: Backing::Memory,
            default,
            known: RwLock::new(seed_known()),
            open: RwLock::new(HashMap::new()),
        })
    }

    /// The always-present [`DEFAULT_DB`] handle. Infallible.
    #[must_use]
    pub fn default_db(&self) -> Arc<Drevo> {
        Arc::clone(&self.default)
    }

    /// All registered database names, sorted ascending. Always includes
    /// [`DEFAULT_DB`].
    #[must_use]
    pub fn list(&self) -> Vec<String> {
        self.known
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// True if `name` is a registered database.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.known
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(name)
    }

    /// Fetch the handle for `name`, opening and caching it on first use.
    ///
    /// # Errors
    ///
    /// [`CatalogError::NotFound`] if `name` is not registered, or
    /// [`CatalogError::Open`] if the handle cannot be opened.
    pub fn get(&self, name: &str) -> Result<Arc<Drevo>, CatalogError> {
        // The default handle lives in its own field, not the `open` cache.
        if name == DEFAULT_DB {
            return Ok(self.default_db());
        }
        if !self.contains(name) {
            return Err(CatalogError::NotFound(name.to_string()));
        }
        // Fast path: already open.
        if let Some(db) = self
            .open
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
        {
            return Ok(db);
        }
        // Slow path: open under the write lock, re-checking in case another
        // thread opened it while we waited for the lock.
        let mut open = self.open.write().unwrap_or_else(|e| e.into_inner());
        if let Some(db) = open.get(name).cloned() {
            return Ok(db);
        }
        let db = Arc::new(self.open_handle(name)?);
        open.insert(name.to_string(), Arc::clone(&db));
        Ok(db)
    }

    /// Create a new database named `name` and return its freshly opened
    /// handle.
    ///
    /// # Errors
    ///
    /// - [`CatalogError::InvalidName`] if `name` fails [`is_valid_name`].
    /// - [`CatalogError::AlreadyExists`] if `name` is already registered.
    /// - [`CatalogError::Open`] if the underlying handle cannot be opened.
    pub fn create(&self, name: &str) -> Result<Arc<Drevo>, CatalogError> {
        if !is_valid_name(name) {
            return Err(CatalogError::InvalidName(name.to_string()));
        }
        // Register atomically under the `known` write lock so two concurrent
        // creates cannot both believe they won.
        {
            let mut known = self.known.write().unwrap_or_else(|e| e.into_inner());
            if known.contains(name) {
                return Err(CatalogError::AlreadyExists(name.to_string()));
            }
            known.insert(name.to_string());
        }
        // Open (and thereby create the file). If this fails, roll the name
        // back out of `known` so a retry is possible.
        match self.get(name) {
            Ok(db) => Ok(db),
            Err(err) => {
                self.known
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(name);
                Err(err)
            }
        }
    }

    /// Open the backing handle for `name` without touching the caches.
    fn open_handle(&self, name: &str) -> Result<Drevo, CatalogError> {
        match &self.backing {
            Backing::Memory => Drevo::open_in_memory().map_err(|source| CatalogError::Open {
                name: name.to_string(),
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── name validation ────────────────────────────────────────────────
    #[test]
    fn valid_names_accept_alnum_dash_underscore() {
        assert!(is_valid_name("default"));
        assert!(is_valid_name("projectA"));
        assert!(is_valid_name("my-db_2"));
        assert!(is_valid_name("A"));
    }

    #[test]
    fn invalid_names_reject_empty_path_and_specials() {
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("has space"));
        assert!(!is_valid_name("../etc/passwd"));
        assert!(!is_valid_name("dot.name"));
        assert!(!is_valid_name("slash/name"));
        assert!(!is_valid_name("emoji😀"));
        assert!(!is_valid_name(&"x".repeat(MAX_NAME_LEN + 1)));
    }

    // ── in-memory catalog behaviour ─────────────────────────────────────
    #[test]
    fn fresh_catalog_has_only_default() {
        let cat = Catalog::open_in_memory().unwrap();
        assert_eq!(cat.list(), vec!["drevo".to_string()]);
        assert!(cat.contains(DEFAULT_DB));
        assert!(cat.get(DEFAULT_DB).is_ok());
    }

    #[test]
    fn create_registers_and_returns_distinct_handle() {
        let cat = Catalog::open_in_memory().unwrap();
        let a = cat.create("alpha").unwrap();
        assert!(cat.contains("alpha"));
        assert_eq!(cat.list(), vec!["alpha".to_string(), "drevo".to_string()]);
        // The new database is independent of default: a write to one is not
        // visible in the other.
        let node = crate::model::NewNode {
            kind: "k".into(),
            title: "t".into(),
            body: String::new(),
            body_html: String::new(),
            properties: crate::model::Properties::default(),
        };
        a.create_node(node).unwrap();
        let default = cat.get(DEFAULT_DB).unwrap();
        assert_eq!(default.list_nodes_by_kind("k", 10, 0).unwrap().len(), 0);
        assert_eq!(a.list_nodes_by_kind("k", 10, 0).unwrap().len(), 1);
    }

    #[test]
    fn get_returns_same_cached_handle() {
        let cat = Catalog::open_in_memory().unwrap();
        cat.create("beta").unwrap();
        let h1 = cat.get("beta").unwrap();
        let h2 = cat.get("beta").unwrap();
        assert!(Arc::ptr_eq(&h1, &h2), "get must cache one handle per name");
    }

    #[test]
    fn create_rejects_duplicate() {
        let cat = Catalog::open_in_memory().unwrap();
        cat.create("gamma").unwrap();
        assert!(matches!(
            cat.create("gamma"),
            Err(CatalogError::AlreadyExists(_))
        ));
        // `default` is pre-registered, so it also collides.
        assert!(matches!(
            cat.create(DEFAULT_DB),
            Err(CatalogError::AlreadyExists(_))
        ));
    }

    #[test]
    fn create_rejects_invalid_name() {
        let cat = Catalog::open_in_memory().unwrap();
        assert!(matches!(
            cat.create("bad name"),
            Err(CatalogError::InvalidName(_))
        ));
    }

    #[test]
    fn get_unknown_is_not_found() {
        let cat = Catalog::open_in_memory().unwrap();
        assert!(matches!(cat.get("nope"), Err(CatalogError::NotFound(_))));
    }
}
