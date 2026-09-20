//! Storage backends — abstraction over the on-disk / in-memory KV layer.
//!
//! One concrete backend ships with drevo:
//!
//! - [`memory::MemoryBackend`] — a `BTreeMap`-backed in-process backend.
//!   Usable everywhere, including `wasm32-unknown-unknown` where filesystem
//!   access is unavailable. It also backs the in-memory KV `Drevo`
//!   handle that survives as the native engine's differential-test oracle.
//!
//! It implements [`backend::StorageBackend`], the trait every higher-level
//! component (`Drevo`, [`crate::fts`], [`crate::traversal`])
//! takes a reference to. Errors funnel through [`error::StorageError`].
//!
//! The durable serving path is the native engine (WAL), not this KV layer;
//! the legacy redb backend was removed in epic #444 P8.

/// The `StorageBackend` trait — the abstraction every higher-level
/// drevo component takes a reference to.
pub mod backend;
/// Mutation-epoch decorator over any backend — staleness detection +
/// quiesce gate for the native read mirror (engine flip, #307 Phase 6).
pub mod epoch;
/// Typed error hierarchy for the storage layer (`NotFound`, `Io`,
/// `Encode`, `Decode`, `LockPoisoned`).
pub mod error;
/// In-memory, `BTreeMap`-backed storage backend (also the WASM target).
pub mod memory;

pub use backend::StorageBackend;
pub use epoch::EpochBackend;
pub use error::{Result, StorageError};
pub use memory::MemoryBackend;
