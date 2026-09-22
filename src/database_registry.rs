//! In-process registry of named native databases — the multi-database catalog
//! (issue #523, RFC `docs/rfc-native-core.md` #307 Phase 2).
//!
//! One drevo process serves a single durable graph today; this registry lets it
//! hold several named [`NativeService`](crate::native_service::NativeService)
//! instances side by side, each with its own isolated node/edge/id space. The
//! always-present **default** database
//! ([`crate::native_api::DEFAULT_DB`]) is the durable graph the process was
//! pointed at, so a client that never names a database sees exactly the old
//! single-database behaviour.
//!
//! # Durability
//!
//! A registry can be **in-memory** ([`new`](crate::database_registry::DatabaseRegistry::new) — used by
//! tests and the in-memory server) or **durable**
//! ([`with_durable_dir`](crate::database_registry::DatabaseRegistry::with_durable_dir) — used by the
//! shipping WAL-backed server). In durable mode each non-default database lives
//! in its own WAL directory `<data_dir>/databases/<name>/native.wal`, so a
//! `CREATE DATABASE` survives a restart: `with_durable_dir` re-opens every such
//! directory on startup, and [`remove`](crate::database_registry::DatabaseRegistry::remove) deletes the
//! directory so a dropped database does not resurrect. The default database is
//! whatever store the process was pointed at (`<data_dir>/native.wal`), one
//! level up from the per-database subtree, so the two never collide.
//!
//! [`create`](crate::database_registry::DatabaseRegistry::create) picks the mode from the registry:
//! durable registries open a WAL, in-memory registries stay in memory. Query
//! routing to a chosen database — the HTTP selector, the Bolt `db` field, and
//! Cypher `USE` — landed in the routing slices; this closes the persistence gap
//! that made non-default databases ephemeral.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::native_service::NativeService;

/// Subdirectory under a durable registry's data dir that holds one child
/// directory per non-default database. Kept one level below the default
/// database's own `native.wal` so the two never collide.
const DATABASES_SUBDIR: &str = "databases";

/// WAL filename inside each per-database directory — the same basename the
/// default store uses under the data dir, so the layout is uniform.
const DB_WAL_FILE: &str = "native.wal";

/// Longest permitted database name, in bytes. Matches the conservative bound a
/// per-database on-disk directory (a later slice) can carry on every target
/// filesystem without escaping or truncation surprises.
const MAX_DB_NAME_LEN: usize = 63;

/// A failure from a [`DatabaseRegistry`] catalog operation.
///
/// The HTTP layer lifts these into an `ApiError` (see the `From` impl in
/// [`crate::native_api`]): invalid name → 400, already-exists → 409,
/// not-found → 404, protected-default → 409.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// The requested name is not a legal database name — see
    /// [`validate_db_name`] for the rule.
    #[error(
        "invalid database name '{0}': names must be 1–63 characters of \
         [A-Za-z0-9_-] and begin with a letter, digit, or underscore"
    )]
    InvalidName(String),

    /// A database with this name is already in the catalog.
    #[error("database '{0}' already exists")]
    AlreadyExists(String),

    /// No database with this name is in the catalog.
    #[error("database '{0}' not found")]
    NotFound(String),

    /// The default database cannot be dropped — it is the store the process was
    /// pointed at and the fallback for un-named requests.
    #[error("the default database '{0}' cannot be dropped")]
    ProtectedDefault(String),

    /// A durable-storage operation failed — opening a per-database WAL on
    /// create / restore, or deleting a dropped database's directory. The
    /// `String` is the underlying error rendered for the log / HTTP body (kept
    /// as a `String` so the variant stays `Clone`/`Eq` like its siblings).
    #[error("storage error for database '{name}': {detail}")]
    Storage {
        /// The database whose durable directory the operation targeted.
        name: String,
        /// The underlying I/O / WAL error, rendered.
        detail: String,
    },
}

/// Validate a database name.
///
/// A legal name is 1–63 bytes, contains only ASCII `[A-Za-z0-9_-]`, and begins
/// with a letter, digit, or underscore (not `-`). This is a pure syntactic
/// check; it says nothing about whether the name is free or reserved — the
/// registry's [`create_in_memory`](DatabaseRegistry::create_in_memory) and
/// [`remove`](DatabaseRegistry::remove) enforce uniqueness and default
/// protection.
///
/// # Errors
///
/// Returns [`RegistryError::InvalidName`] if `name` breaks any of the rules.
pub fn validate_db_name(name: &str) -> Result<(), RegistryError> {
    let len_ok = (1..=MAX_DB_NAME_LEN).contains(&name.len());
    let first_ok = matches!(
        name.chars().next(),
        Some(c) if c.is_ascii_alphanumeric() || c == '_'
    );
    let rest_ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if len_ok && first_ok && rest_ok {
        Ok(())
    } else {
        Err(RegistryError::InvalidName(name.to_string()))
    }
}

/// A catalog of named [`NativeService`] databases served by one process.
///
/// Cheap to share behind an [`Arc`]; every method takes `&self` and locks
/// internally. Names sort ascending in [`list`](Self::list) because the backing
/// map is a [`BTreeMap`].
pub struct DatabaseRegistry {
    /// The reserved default database name (always present in `dbs`).
    default_name: &'static str,
    /// A direct handle to the default database, so [`default_service`] is
    /// lock-free and infallible even though `dbs` also holds the same `Arc`.
    ///
    /// [`default_service`]: Self::default_service
    default: Arc<NativeService>,
    /// The catalog: database name → service. Sorted by name.
    dbs: RwLock<BTreeMap<String, Arc<NativeService>>>,
    /// Base data directory for durable per-database WAL directories, or `None`
    /// for an in-memory registry. When `Some`, [`create`](Self::create) opens a
    /// WAL under `<data_dir>/databases/<name>/` and [`remove`](Self::remove)
    /// deletes it.
    data_dir: Option<PathBuf>,
}

// `NativeService` is not `Debug`, so derive would not apply; print the catalog
// shape (names) rather than the service internals.
impl std::fmt::Debug for DatabaseRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseRegistry")
            .field("default_name", &self.default_name)
            .field("databases", &self.list())
            .finish()
    }
}

impl DatabaseRegistry {
    /// Build a registry whose default database is `default`, registered under
    /// [`crate::native_api::DEFAULT_DB`].
    #[must_use]
    pub fn new(default: Arc<NativeService>) -> Self {
        Self::build(default, BTreeMap::new(), None)
    }

    /// Build a **durable** registry whose non-default databases persist under
    /// `data_dir`, re-opening every database already on disk from a previous run.
    ///
    /// The default database is `default` (the store the process was pointed at,
    /// `<data_dir>/native.wal`); every child directory of
    /// `<data_dir>/databases/` is re-opened as a durable [`NativeService`] and
    /// registered under its directory name, so a `CREATE DATABASE` from an
    /// earlier run comes back. Directory names that are not legal database names
    /// (or that collide with the default) are skipped defensively rather than
    /// aborting startup.
    ///
    /// # Errors
    ///
    /// [`RegistryError::Storage`] if the databases directory cannot be read or a
    /// per-database WAL fails to open (a corrupt/locked store is a hard startup
    /// failure, not a silently-dropped database).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_durable_dir(
        default: Arc<NativeService>,
        data_dir: impl Into<PathBuf>,
    ) -> Result<Self, RegistryError> {
        let data_dir = data_dir.into();
        let default_name = crate::native_api::DEFAULT_DB;
        let mut restored = BTreeMap::new();
        let root = data_dir.join(DATABASES_SUBDIR);
        if root.is_dir() {
            let entries = std::fs::read_dir(&root).map_err(|e| RegistryError::Storage {
                name: DATABASES_SUBDIR.to_string(),
                detail: e.to_string(),
            })?;
            for entry in entries {
                let entry = entry.map_err(|e| RegistryError::Storage {
                    name: DATABASES_SUBDIR.to_string(),
                    detail: e.to_string(),
                })?;
                if !entry.path().is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                // Skip anything that could not have been created through the
                // registry, and never shadow the default entry.
                if name == default_name || validate_db_name(&name).is_err() {
                    continue;
                }
                let wal = entry.path().join(DB_WAL_FILE);
                let service = NativeService::open(&wal).map_err(|e| RegistryError::Storage {
                    name: name.clone(),
                    detail: e.to_string(),
                })?;
                restored.insert(name, Arc::new(service));
            }
        }
        Ok(Self::build(default, restored, Some(data_dir)))
    }

    /// Shared constructor: seed the catalog with the default plus any
    /// already-restored databases, and record the durability mode.
    fn build(
        default: Arc<NativeService>,
        mut dbs: BTreeMap<String, Arc<NativeService>>,
        data_dir: Option<PathBuf>,
    ) -> Self {
        let default_name = crate::native_api::DEFAULT_DB;
        dbs.insert(default_name.to_string(), Arc::clone(&default));
        Self {
            default_name,
            default,
            dbs: RwLock::new(dbs),
            data_dir,
        }
    }

    /// The on-disk directory for the database named `name` in a durable
    /// registry: `<data_dir>/databases/<name>`. `name` is assumed already
    /// validated (a legal name has no path separators or `..`).
    fn db_dir(data_dir: &Path, name: &str) -> PathBuf {
        data_dir.join(DATABASES_SUBDIR).join(name)
    }

    /// The reserved default database name.
    #[must_use]
    pub fn default_name(&self) -> &'static str {
        self.default_name
    }

    /// The default database's service. Always present, never locks the catalog.
    #[must_use]
    pub fn default_service(&self) -> Arc<NativeService> {
        Arc::clone(&self.default)
    }

    /// The service for `name`, or `None` if no such database is registered.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<NativeService>> {
        self.read().get(name).map(Arc::clone)
    }

    /// Whether a database named `name` is registered.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.read().contains_key(name)
    }

    /// Every database name, sorted ascending. Always includes the default.
    #[must_use]
    pub fn list(&self) -> Vec<String> {
        self.read().keys().cloned().collect()
    }

    /// The number of databases in the catalog (always ≥ 1 for the default).
    #[must_use]
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Always `false` — the default database is never absent. Present so
    /// Clippy's `len_without_is_empty` stays satisfied and callers read
    /// naturally.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Create a new in-memory database named `name` and register it.
    ///
    /// Returns the new service (also retrievable via [`get`](Self::get)).
    ///
    /// # Errors
    ///
    /// - [`RegistryError::InvalidName`] if `name` is not a legal database name.
    /// - [`RegistryError::AlreadyExists`] if a database named `name` (including
    ///   the default) is already registered.
    pub fn create_in_memory(&self, name: &str) -> Result<Arc<NativeService>, RegistryError> {
        validate_db_name(name)?;
        let mut dbs = self.write();
        if dbs.contains_key(name) {
            return Err(RegistryError::AlreadyExists(name.to_string()));
        }
        let service = Arc::new(NativeService::in_memory());
        dbs.insert(name.to_string(), Arc::clone(&service));
        Ok(service)
    }

    /// Create a new database named `name`, durable or in-memory per the
    /// registry's mode, and register it.
    ///
    /// A durable registry ([`with_durable_dir`](Self::with_durable_dir)) opens a
    /// WAL at `<data_dir>/databases/<name>/native.wal` — so the database
    /// survives a restart; an in-memory registry ([`new`](Self::new)) creates an
    /// ephemeral service. This is the entry point the HTTP `POST /databases` and
    /// Cypher `CREATE DATABASE` handlers call so behaviour follows the server's
    /// own durability.
    ///
    /// # Errors
    ///
    /// - [`RegistryError::InvalidName`] if `name` is not a legal database name.
    /// - [`RegistryError::AlreadyExists`] if a database named `name` (including
    ///   the default) is already registered.
    /// - [`RegistryError::Storage`] if a durable WAL cannot be created / opened.
    pub fn create(&self, name: &str) -> Result<Arc<NativeService>, RegistryError> {
        validate_db_name(name)?;
        let mut dbs = self.write();
        if dbs.contains_key(name) {
            return Err(RegistryError::AlreadyExists(name.to_string()));
        }
        let service = match &self.data_dir {
            Some(data_dir) => Arc::new(Self::open_durable(data_dir, name)?),
            None => Arc::new(NativeService::in_memory()),
        };
        dbs.insert(name.to_string(), Arc::clone(&service));
        Ok(service)
    }

    /// Open (creating its directory) the durable WAL for `name`. Split out so
    /// the wasm build — which has no `NativeService::open` — never references it.
    #[cfg(not(target_arch = "wasm32"))]
    fn open_durable(data_dir: &Path, name: &str) -> Result<NativeService, RegistryError> {
        let dir = Self::db_dir(data_dir, name);
        std::fs::create_dir_all(&dir).map_err(|e| RegistryError::Storage {
            name: name.to_string(),
            detail: e.to_string(),
        })?;
        NativeService::open(dir.join(DB_WAL_FILE)).map_err(|e| RegistryError::Storage {
            name: name.to_string(),
            detail: e.to_string(),
        })
    }

    /// The wasm counterpart: a durable registry cannot exist on wasm
    /// (`with_durable_dir` is not compiled there), so `create` never reaches a
    /// `Some(data_dir)` arm — this stub keeps the non-wasm call site typed
    /// without pulling in the (absent) durable engine.
    #[cfg(target_arch = "wasm32")]
    fn open_durable(_data_dir: &Path, name: &str) -> Result<NativeService, RegistryError> {
        Err(RegistryError::Storage {
            name: name.to_string(),
            detail: "durable databases are not available on wasm".to_string(),
        })
    }

    /// Remove (drop) the database named `name`, discarding its service.
    ///
    /// Named `remove` rather than `drop` so it does not shadow the destructor
    /// method name on `Arc<DatabaseRegistry>` (which the compiler rejects as an
    /// explicit destructor call).
    ///
    /// In a durable registry the database's WAL directory is deleted so a
    /// dropped database does not come back on the next restart; the in-catalog
    /// service handle is dropped first so its WAL file is closed before the
    /// directory is removed.
    ///
    /// # Errors
    ///
    /// - [`RegistryError::ProtectedDefault`] if `name` is the default database.
    /// - [`RegistryError::NotFound`] if no database named `name` is registered.
    /// - [`RegistryError::Storage`] if the database's directory cannot be
    ///   deleted (the catalog entry is already gone at that point — the database
    ///   is unreachable, but its files lingered).
    pub fn remove(&self, name: &str) -> Result<(), RegistryError> {
        if name == self.default_name {
            return Err(RegistryError::ProtectedDefault(name.to_string()));
        }
        let mut dbs = self.write();
        let Some(service) = dbs.remove(name) else {
            return Err(RegistryError::NotFound(name.to_string()));
        };
        // Close this handle before touching the files. Other threads may still
        // hold an `Arc` (an in-flight routed query); on Unix that keeps the
        // now-unlinked WAL readable until they finish, which is the intended
        // drop semantics.
        drop(service);
        drop(dbs);
        if let Some(data_dir) = &self.data_dir {
            let dir = Self::db_dir(data_dir, name);
            if dir.exists() {
                std::fs::remove_dir_all(&dir).map_err(|e| RegistryError::Storage {
                    name: name.to_string(),
                    detail: e.to_string(),
                })?;
            }
        }
        Ok(())
    }

    /// Read-lock the catalog, recovering from a poisoned lock rather than
    /// panicking (a reader observing a writer's panic still gets a consistent
    /// map; the crosscut audit forbids `unwrap()` here).
    fn read(&self) -> RwLockReadGuard<'_, BTreeMap<String, Arc<NativeService>>> {
        self.dbs.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Write-lock the catalog, recovering from a poisoned lock (see
    /// [`read`](Self::read)).
    fn write(&self) -> RwLockWriteGuard<'_, BTreeMap<String, Arc<NativeService>>> {
        self.dbs.write().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NewNode;
    use crate::native_api::DEFAULT_DB;

    fn default_registry() -> DatabaseRegistry {
        DatabaseRegistry::new(Arc::new(NativeService::in_memory()))
    }

    #[test]
    fn new_registry_holds_only_the_default() {
        let reg = default_registry();
        assert_eq!(reg.list(), vec![DEFAULT_DB.to_string()]);
        assert_eq!(reg.default_name(), DEFAULT_DB);
        assert!(reg.contains(DEFAULT_DB));
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }

    #[test]
    fn default_service_is_the_same_handle_as_the_catalog_entry() {
        let reg = default_registry();
        let via_field = reg.default_service();
        let via_map = reg.get(DEFAULT_DB).expect("default present");
        assert!(Arc::ptr_eq(&via_field, &via_map));
    }

    #[test]
    fn create_registers_and_sorts_names() {
        let reg = default_registry();
        reg.create_in_memory("zebra").expect("create zebra");
        reg.create_in_memory("alpha").expect("create alpha");
        // BTreeMap keys come back ascending: alpha, drevo, zebra.
        assert_eq!(
            reg.list(),
            vec![
                "alpha".to_string(),
                DEFAULT_DB.to_string(),
                "zebra".to_string()
            ]
        );
        assert!(reg.contains("alpha"));
        assert!(reg.get("zebra").is_some());
    }

    #[test]
    fn create_duplicate_is_conflict() {
        let reg = default_registry();
        reg.create_in_memory("dup").expect("first create");
        assert_eq!(
            reg.create_in_memory("dup").map(|_| ()),
            Err(RegistryError::AlreadyExists("dup".to_string()))
        );
        // Re-creating the default name is a conflict too.
        assert_eq!(
            reg.create_in_memory(DEFAULT_DB).map(|_| ()),
            Err(RegistryError::AlreadyExists(DEFAULT_DB.to_string()))
        );
    }

    #[test]
    fn create_rejects_invalid_names() {
        let reg = default_registry();
        for bad in [
            "",
            "-leading",
            "has space",
            "punct!",
            "slash/name",
            &"x".repeat(64),
        ] {
            assert_eq!(
                reg.create_in_memory(bad).map(|_| ()),
                Err(RegistryError::InvalidName(bad.to_string())),
                "expected {bad:?} to be rejected"
            );
        }
        // The catalog is unchanged by rejected creates.
        assert_eq!(reg.list(), vec![DEFAULT_DB.to_string()]);
    }

    #[test]
    fn validate_accepts_reasonable_names() {
        for good in ["a", "Bar_2", "my-db", "_hidden", "A1-b_2", &"x".repeat(63)] {
            assert!(
                validate_db_name(good).is_ok(),
                "expected {good:?} to be valid"
            );
        }
    }

    #[test]
    fn drop_removes_a_non_default_database() {
        let reg = default_registry();
        reg.create_in_memory("scratch").expect("create");
        assert!(reg.contains("scratch"));
        reg.remove("scratch").expect("drop");
        assert!(!reg.contains("scratch"));
        assert_eq!(reg.list(), vec![DEFAULT_DB.to_string()]);
    }

    #[test]
    fn drop_default_is_protected() {
        let reg = default_registry();
        assert_eq!(
            reg.remove(DEFAULT_DB),
            Err(RegistryError::ProtectedDefault(DEFAULT_DB.to_string()))
        );
        assert!(reg.contains(DEFAULT_DB));
    }

    #[test]
    fn drop_missing_is_not_found() {
        let reg = default_registry();
        assert_eq!(
            reg.remove("ghost"),
            Err(RegistryError::NotFound("ghost".to_string()))
        );
    }

    #[test]
    fn databases_have_isolated_id_and_node_spaces() {
        let reg = default_registry();
        let a = reg.create_in_memory("a").expect("create a");
        let b = reg.create_in_memory("b").expect("create b");

        // Write a node into `a` only.
        a.create_node(NewNode {
            kind: "note".to_string(),
            title: "in-a".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: crate::model::Properties::default(),
        })
        .expect("create node in a");

        // `a` sees it; `b` and the default are untouched — isolation.
        assert_eq!(a.list_nodes_by_kind("note", 10, 0).len(), 1);
        assert_eq!(b.list_nodes_by_kind("note", 10, 0).len(), 0);
        assert_eq!(
            reg.default_service()
                .list_nodes_by_kind("note", 10, 0)
                .len(),
            0
        );
    }

    #[test]
    fn create_on_in_memory_registry_stays_in_memory() {
        // `create` on a `new()` registry (no data dir) is the in-memory path —
        // no filesystem, same result as `create_in_memory`.
        let reg = default_registry();
        reg.create("mem").expect("create in-memory via create()");
        assert!(reg.contains("mem"));
        assert_eq!(
            reg.list(),
            vec!["drevo".to_string(), "mem".to_string()] // DEFAULT_DB sorts before "mem"
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn durable_registry_at(data_dir: &std::path::Path) -> (Arc<NativeService>, DatabaseRegistry) {
        let default =
            Arc::new(NativeService::open(data_dir.join("native.wal")).expect("open default store"));
        let reg = DatabaseRegistry::with_durable_dir(Arc::clone(&default), data_dir)
            .expect("build durable registry");
        (default, reg)
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn durable_create_persists_across_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path();

        // Create a durable database and write a node into it.
        let (default, reg) = durable_registry_at(data_dir);
        let foo = reg.create("foo").expect("create foo");
        assert!(
            data_dir
                .join("databases")
                .join("foo")
                .join("native.wal")
                .exists(),
            "durable create must open a per-database WAL on disk"
        );
        foo.create_node(NewNode {
            kind: "note".to_string(),
            title: "persisted".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: crate::model::Properties::default(),
        })
        .expect("write node into foo");

        // Close everything (drop the WAL handles).
        drop(foo);
        drop(reg);
        drop(default);

        // A fresh registry over the same data dir re-opens `foo` with its data —
        // the restart-recovery contract.
        let (_default2, reg2) = durable_registry_at(data_dir);
        assert!(
            reg2.contains("foo"),
            "a durable database must survive a registry re-open"
        );
        let foo2 = reg2.get("foo").expect("foo present after reopen");
        assert_eq!(
            foo2.list_nodes_by_kind("note", 10, 0).len(),
            1,
            "the node written before the reopen must persist"
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn durable_drop_deletes_the_directory_and_does_not_resurrect() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path();

        let (default, reg) = durable_registry_at(data_dir);
        reg.create("scratch").expect("create scratch");
        let scratch_dir = data_dir.join("databases").join("scratch");
        assert!(scratch_dir.exists(), "create made the directory");

        reg.remove("scratch").expect("drop scratch");
        assert!(
            !scratch_dir.exists(),
            "drop must delete the database's WAL directory"
        );
        assert!(!reg.contains("scratch"));

        // A re-open does not bring the dropped database back.
        drop(reg);
        drop(default);
        let (_default2, reg2) = durable_registry_at(data_dir);
        assert!(
            !reg2.contains("scratch"),
            "a dropped durable database must not resurrect on reopen"
        );
    }
}
