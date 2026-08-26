//! The `GraphEngine` seam for the KV-backed [`crate::db::Drevo`] store.
//!
//! The trait itself was extracted to the [`drevo-core`](drevo_core) crate
//! (Phase 7 slice 6, RFC `docs/rfc-native-core.md`, #307) so the native engine
//! and the query layers can depend on it without the KV store. It is re-exported
//! here (`pub use drevo_core::engine::GraphEngine`) so existing
//! `crate::engine::GraphEngine` / `drevo::engine::GraphEngine` paths keep
//! resolving.
//!
//! This module now carries only the KV implementation: [`crate::db::Drevo`] implements the
//! core trait by delegating to its existing inherent methods, mapping its richer
//! [`crate::error::DrevoError`] into the core's
//! [`CoreError`](drevo_core::error::CoreError) at the boundary (via the `?`
//! operator and the `From<DrevoError> for CoreError` impl). The shared graph
//! variants map one-to-one, so a `NodeNotFound` still surfaces as such through
//! the seam.

pub use drevo_core::engine::GraphEngine;

use std::sync::Arc;

use crate::db::Drevo;
use crate::dump::{Dump, ImportReport};
use crate::model::{Direction, Edge, EdgePatch, NewEdge, NewNode, Node, NodePatch};
use drevo_core::error::Result as CoreResult;

/// The KV-backed store is a `GraphEngine`. Every method delegates to the
/// inherent method of the same name; the `?` operator lifts its
/// [`crate::error::DrevoError`] into the core's
/// [`CoreError`](drevo_core::error::CoreError), preserving the shared variants.
impl GraphEngine for Drevo {
    fn create_node(&self, new_node: NewNode) -> CoreResult<Node> {
        Ok(Drevo::create_node(self, new_node)?)
    }

    fn get_node(&self, id: u64) -> CoreResult<Option<Arc<Node>>> {
        // The KV store materialises an owned decode; wrap it so the seam is
        // zero-copy where the engine can share (the native engine) and a cheap
        // single allocation where it cannot (here).
        Ok(Drevo::get_node(self, id)?.map(Arc::new))
    }

    fn update_node(&self, id: u64, patch: NodePatch) -> CoreResult<Node> {
        Ok(Drevo::update_node(self, id, patch)?)
    }

    fn delete_node(&self, id: u64) -> CoreResult<()> {
        Ok(Drevo::delete_node(self, id)?)
    }

    fn create_edge(&self, new_edge: NewEdge) -> CoreResult<Edge> {
        Ok(Drevo::create_edge(self, new_edge)?)
    }

    fn get_edge(&self, id: u64) -> CoreResult<Option<Edge>> {
        Ok(Drevo::get_edge(self, id)?)
    }

    fn update_edge(&self, id: u64, patch: EdgePatch) -> CoreResult<Edge> {
        Ok(Drevo::update_edge(self, id, patch)?)
    }

    fn delete_edge(&self, id: u64) -> CoreResult<()> {
        Ok(Drevo::delete_edge(self, id)?)
    }

    fn neighbor_ids(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> CoreResult<Vec<u64>> {
        Ok(Drevo::neighbor_ids(self, node_id, direction, kind)?)
    }

    fn neighbors(
        &self,
        node_id: u64,
        direction: Direction,
        kind: Option<&str>,
    ) -> CoreResult<Vec<Arc<Node>>> {
        Ok(Drevo::neighbors(self, node_id, direction, kind)?
            .into_iter()
            .map(Arc::new)
            .collect())
    }

    fn edges_of(&self, node_id: u64, direction: Direction) -> CoreResult<Vec<Edge>> {
        Ok(Drevo::edges_of(self, node_id, direction)?)
    }

    fn all_nodes(&self) -> CoreResult<Vec<Arc<Node>>> {
        Ok(Drevo::collect_all_nodes(self)?
            .into_iter()
            .map(Arc::new)
            .collect())
    }

    fn all_edges(&self) -> CoreResult<Vec<Edge>> {
        Ok(Drevo::collect_all_edges(self)?)
    }

    fn nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> CoreResult<Vec<Arc<Node>>> {
        Ok(Drevo::list_nodes_by_kind(self, kind, limit, offset)?
            .into_iter()
            .map(Arc::new)
            .collect())
    }

    fn export_dump(&self) -> CoreResult<Dump> {
        Ok(self.build_dump()?)
    }

    fn apply_dump(&self, dump: Dump) -> CoreResult<ImportReport> {
        Ok(self.apply_dump_records(dump)?)
    }
}
