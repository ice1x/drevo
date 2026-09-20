//! The `GraphEngine` seam.
//!
//! The trait was extracted to the [`drevo-core`](drevo_core) crate (Phase 7
//! slice 6, RFC `docs/rfc-native-core.md`, #307) so the native engine and the
//! query layers can depend on it without any storage engine. It is re-exported
//! here (`pub use drevo_core::engine::GraphEngine`) so existing
//! `crate::engine::GraphEngine` / `drevo::engine::GraphEngine` paths keep
//! resolving.
//!
//! The sole implementor is the native engine (`drevo_core::native::NativeGraph`,
//! served through [`crate::native_service::NativeService`]); its impl lives in
//! `drevo-core`. The former KV `Drevo` implementation was removed with the KV
//! engine (epic #444).

pub use drevo_core::engine::GraphEngine;
