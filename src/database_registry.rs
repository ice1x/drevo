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
//! # Scope of this slice
//!
//! This is the catalog *foundation* plus its HTTP lifecycle (`GET`/`POST`
//! `/databases`, `DELETE /databases/{name}`): create, list, and drop named
//! databases, with the default protected from removal. **Routing a query to a
//! chosen database** — the HTTP path selector and the Bolt `db` field / Cypher
//! `USE` — is a follow-up slice; a freshly created database therefore exists in
//! the catalog and accepts lifecycle operations, but is not yet a query target
//! (mirroring Neo4j's split between `CREATE DATABASE` and `USE`). Non-default
//! databases are in-memory for now; per-database durable WAL directories land
//! with the routing slice.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::native_service::NativeService;

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
        let default_name = crate::native_api::DEFAULT_DB;
        let mut dbs = BTreeMap::new();
        dbs.insert(default_name.to_string(), Arc::clone(&default));
        Self {
            default_name,
            default,
            dbs: RwLock::new(dbs),
        }
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

    /// Remove (drop) the database named `name`, discarding its service.
    ///
    /// Named `remove` rather than `drop` so it does not shadow the destructor
    /// method name on `Arc<DatabaseRegistry>` (which the compiler rejects as an
    /// explicit destructor call).
    ///
    /// # Errors
    ///
    /// - [`RegistryError::ProtectedDefault`] if `name` is the default database.
    /// - [`RegistryError::NotFound`] if no database named `name` is registered.
    pub fn remove(&self, name: &str) -> Result<(), RegistryError> {
        if name == self.default_name {
            return Err(RegistryError::ProtectedDefault(name.to_string()));
        }
        let mut dbs = self.write();
        if dbs.remove(name).is_none() {
            return Err(RegistryError::NotFound(name.to_string()));
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
}
