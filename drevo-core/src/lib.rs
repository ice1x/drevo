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

pub mod model;
/// Pure text tokenization — normalization plus trigram/word extraction, with no
/// storage or error dependencies. Shared by the KV full-text index and the
/// native `NativeFtsIndex`; re-exported from `drevo::fts::tokenizer`.
pub mod tokenizer;
