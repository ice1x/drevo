//! Cross-engine data migration over the [`GraphEngine`](crate::engine::GraphEngine)
//! seam (RFC `docs/rfc-native-core.md`, #307).
//!
//! Moving a live graph between the KV-backed [`crate::db::Drevo`] and the
//! native [`crate::native::NativeGraph`] is the prerequisite for adopting (or
//! rolling back from) the native engine in a running deployment: the topology
//! has to carry over with every node/edge **id** intact, or the edges would
//! point at nothing.
//!
//! Both engines already speak the `drevo-json-v1` [`crate::dump::Dump`]
//! interchange — the same format that backs JSON / GraphML export/import — so a
//! migration is just [`GraphEngine::export_dump`](crate::engine::GraphEngine::export_dump)
//! on the source piped into
//! [`GraphEngine::apply_dump`](crate::engine::GraphEngine::apply_dump) on the
//! destination. `apply_dump` inserts every
//! record verbatim (ids, uuids, timestamps, properties preserved) and clamps
//! the destination's id counters above every imported id.
//!
//! Because it is expressed purely against the trait, any future engine gains
//! migration to/from every existing one for free.
//!
//! ```no_run
//! use drevo::db::Drevo;
//! use drevo::native::NativeGraph;
//! use drevo::migrate::migrate;
//!
//! # fn main() -> drevo::error::Result<()> {
//! let kv = Drevo::open_in_memory()?;
//! // … populate `kv` …
//! let native = NativeGraph::new();
//! let report = migrate(&kv, &native)?; // KV → native, ids preserved
//! println!("moved {} nodes, {} edges", report.nodes_imported, report.edges_imported);
//! # Ok(())
//! # }
//! ```

use crate::dump::ImportReport;
use crate::engine::GraphEngine;
use crate::error::Result;

/// Copy the entire graph from `src` into `dst`, preserving ids and adjacency.
///
/// This is a *copy*, not a move: `src` is read-only and left untouched, so a
/// failed or partial migration never damages the source. `dst` is typically
/// empty; migrating into a populated destination is idempotent for rows that
/// already match byte-for-byte and an error on an id that collides with
/// different content (see [`GraphEngine::apply_dump`]).
///
/// The returned [`ImportReport`] separates freshly-inserted rows from rows
/// skipped because they already existed identically.
///
/// # Errors
/// Propagates any [`crate::error::DrevoError`] from either engine — a source
/// read failure, or a destination id collision / missing endpoint / backend
/// write failure.
pub fn migrate(src: &dyn GraphEngine, dst: &dyn GraphEngine) -> Result<ImportReport> {
    dst.apply_dump(src.export_dump()?)
}
