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

/// The `drevo-json-v1` dump wire-format types ([`crate::dump::Dump`],
/// [`crate::dump::ImportReport`], [`crate::dump::DumpError`],
/// [`crate::dump::FORMAT_V1`]) — the storage-agnostic interchange the native
/// engine produces and consumes, and the cross-engine migration seam moves.
/// Re-exported from `drevo::dump`.
pub mod dump;
/// The storage-agnostic error type ([`crate::error::CoreError`]) shared by the
/// native engine, its indexes, and the dump seam. Converts structurally to and
/// from the main crate's `DrevoError`.
pub mod error;
/// The `_labels` secondary-label convention
/// ([`crate::labels::secondary_labels`] / [`crate::labels::SECONDARY_LABELS_KEY`])
/// — the single source of truth for parsing a node's extra labels, shared by the
/// Cypher executor and the native label index. Re-exported into the main crate.
pub mod labels;
pub mod model;
/// Pure text tokenization — normalization plus trigram/word extraction, with no
/// storage or error dependencies. Shared by the KV full-text index and the
/// native `NativeFtsIndex`; re-exported from `drevo::fts::tokenizer`.
pub mod tokenizer;
/// Canonical byte encoding of property values
/// ([`crate::value_encoding::encode_value`]) shared by the KV and native property
/// indexes. Re-exported into the main crate.
pub mod value_encoding;
