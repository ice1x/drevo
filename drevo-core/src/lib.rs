//! `drevo-core` — the storage-agnostic core of [drevo](https://github.com/ice1x/drevo).
//!
//! This crate holds the pieces of drevo that do not depend on the KV store,
//! HTTP, Bolt, or the Python bindings — starting with the domain
//! [`model`] and growing, in later extraction slices, to include the native
//! graph engine and its indexes (RFC `docs/rfc-native-core.md`, #307).
//!
//! The main `drevo` crate re-exports everything here (`pub use drevo_core::…`),
//! so existing `drevo::model::…` / `crate::model::…` paths keep resolving
//! unchanged; downstream projects that only need the engine can depend on
//! `drevo-core` directly.

/// The storage-agnostic error type ([`error::CoreError`]) shared by the native
/// engine, its indexes, and the dump seam. Converts structurally to and from the
/// main crate's `DrevoError`.
/// The `drevo-json-v1` dump wire-format types ([`dump::Dump`],
/// [`dump::ImportReport`], [`dump::DumpError`], [`dump::FORMAT_V1`]) — the
/// storage-agnostic interchange the native engine produces and consumes, and the
/// cross-engine migration seam moves. Re-exported from `drevo::dump`.
pub mod dump;
pub mod error;
pub mod model;
/// Pure text tokenization — normalization plus trigram/word extraction, with no
/// storage or error dependencies. Shared by the KV full-text index and the
/// native `NativeFtsIndex`; re-exported from `drevo::fts::tokenizer`.
pub mod tokenizer;
