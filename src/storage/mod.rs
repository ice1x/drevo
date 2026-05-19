//! Storage backends — abstraction over the on-disk / in-memory KV layer.
//!
//! Two concrete backends ship with drevo:
//!
//! - [`memory::MemoryBackend`] — a `BTreeMap`-backed in-process backend.
//!   The only backend usable from `wasm32-unknown-unknown`, where
//!   filesystem access is unavailable.
//! - [`redb::RedbBackend`] — an ACID, B-tree-backed backend powered by
//!   [`redb`](https://github.com/cberner/redb). The default on native
//!   targets; gated behind the `redb-backend` Cargo feature so a WASM
//!   build can opt out of the dependency entirely.
//!
//! Both backends implement [`backend::StorageBackend`], the trait every
//! higher-level component ([`crate::db::Drevo`], [`crate::fts`],
//! [`crate::traversal`]) takes a reference to. Errors funnel through
//! [`error::StorageError`].

/// The `StorageBackend` trait — the abstraction every higher-level
/// drevo component takes a reference to.
pub mod backend;
/// Typed error hierarchy for the storage layer (`NotFound`, `Io`,
/// `Encode`, `Decode`, `Redb`, `LockPoisoned`).
pub mod error;
/// In-memory, `BTreeMap`-backed storage backend (also the WASM target).
pub mod memory;
/// [`redb`]-backed, ACID, B-tree storage backend (native, default).
#[cfg(feature = "redb-backend")]
pub mod redb;

pub use backend::StorageBackend;
pub use error::{Result, StorageError};
pub use memory::MemoryBackend;
#[cfg(feature = "redb-backend")]
pub use redb::RedbBackend;
