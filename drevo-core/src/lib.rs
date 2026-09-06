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

/// Okapi BM25 scoring primitives ([`bm25::bm25_idf`]) shared by the KV and
/// native full-text indexes. Re-exported into the main crate.
pub mod bm25;
/// The `drevo-json-v1` dump wire-format types ([`crate::dump::Dump`],
/// [`crate::dump::ImportReport`], [`crate::dump::DumpError`],
/// [`crate::dump::FORMAT_V1`]) — the storage-agnostic interchange the native
/// engine produces and consumes, and the cross-engine migration seam moves.
/// Re-exported from `drevo::dump`.
pub mod dump;
/// The storage-agnostic error type ([`crate::error::CoreError`]) shared by the
/// native engine, its indexes, and the dump seam. Converts structurally to and
/// from the main crate's `DrevoError`.
/// The [`engine::GraphEngine`] seam — the graph-level trait the query layers
/// depend on, implemented by both the KV store (main crate) and the native
/// engine. Re-exported from `drevo::engine`.
pub mod engine;
pub mod error;
/// Hybrid Logical Clock ([`crate::hlc::Hlc`] / [`crate::hlc::HlcClock`]) — the
/// causal versioning primitive for multi-writer convergence (issue #389).
/// Re-exported into the main crate as `drevo::hlc`.
pub mod hlc;
/// The `_labels` secondary-label convention
/// ([`crate::labels::secondary_labels`] / [`crate::labels::SECONDARY_LABELS_KEY`])
/// — the single source of truth for parsing a node's extra labels, shared by the
/// Cypher executor and the native label index. Re-exported into the main crate.
pub mod labels;
/// Last-Writer-Wins register + map CRDTs ([`crate::lww::LwwRegister`] /
/// [`crate::lww::LwwMap`]) built on the HLC — the convergence primitive for
/// multi-writer records (issue #389). Re-exported as `drevo::lww`.
pub mod lww;
pub mod model;
/// The native in-memory graph engine ([`native::NativeGraph`]) — index-free
/// adjacency, the KV store's observable semantics without key encoding, and a
/// change-feed of [`native::WalOp`] ops. Re-exported from `drevo::native`.
pub mod native;
/// In-memory full-text index ([`native_fts::NativeFtsIndex`]) that tails a
/// [`native::NativeGraph`] change-feed, matching the KV trigram BM25 semantics.
/// Re-exported from `drevo::native_fts`.
pub mod native_fts;
/// In-memory secondary-label index ([`native_label_index::NativeLabelIndex`])
/// that tails a [`native::NativeGraph`] change-feed. Re-exported from
/// `drevo::native_label_index`.
pub mod native_label_index;
/// In-memory property-value index
/// ([`native_property_index::NativePropertyIndex`]) that tails a
/// [`native::NativeGraph`] change-feed. Re-exported from
/// `drevo::native_property_index`.
pub mod native_property_index;
/// Pure text tokenization — normalization plus trigram/word extraction, with no
/// storage or error dependencies. Shared by the KV full-text index and the
/// native `NativeFtsIndex`; re-exported from `drevo::fts::tokenizer`.
pub mod tokenizer;
/// Canonical byte encoding of property values
/// ([`crate::value_encoding::encode_value`]) shared by the KV and native property
/// indexes. Re-exported into the main crate.
pub mod value_encoding;
