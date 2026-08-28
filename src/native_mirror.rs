//! Native read mirror — the engine-flip execution router (RFC
//! `docs/rfc-native-core.md` #307, Phase 6 slice A).
//!
//! [`crate::native_mirror::NativeMirror`] keeps an immutable snapshot of the KV graph inside a
//! [`crate::native::NativeGraph`] with the native label / property indexes
//! and the [`crate::native_value_cache::NativeValueCache`] synced — the
//! flip-target execution stack the real-data baseline measured at 100–600×
//! the KV scan speed (`docs/native-core-baseline.md`). Durability stays with
//! the KV store: **every write executes on KV**, and the mirror only serves
//! reads.
//!
//! # Correctness model
//!
//! The mirror is stamped with the KV store's mutation epoch
//! ([`crate::db::Drevo::mutation_epoch`]) taken under a write quiesce, so the
//! stamp vouches for the whole snapshot. A read is served natively **only
//! when the stamp still equals the live epoch** — i.e. not a single mutating
//! storage call has completed since the snapshot. Otherwise the read falls
//! back to the KV engine (always correct, merely slower) and a background
//! rebuild is kicked off; once the rebuild lands, native serving resumes.
//! A stale or failed mirror can therefore only ever cost speed, never
//! answers — the same design rule the value cache's `Arc::ptr_eq` validity
//! follows.
//!
//! Result parity between the two execution paths is pinned by the
//! differential corpus (`tests/cypher_kv_native_differential_tests.rs`);
//! which queries the mirror may serve at all is decided by
//! [`crate::cypher::read_only::mirror_can_serve`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::cypher::ast::Query;
use crate::cypher::executor::{
    self, execute_on_engine_with_indexes_and_values, ExecError, ExecResult, Value,
};
use crate::cypher::read_only::mirror_can_serve;
use crate::db::Drevo;
use crate::engine::GraphEngine;
use crate::error::DrevoError;
use crate::native::NativeGraph;
use crate::native_label_index::NativeLabelIndex;
use crate::native_property_index::NativePropertyIndex;
use crate::native_value_cache::NativeValueCache;

/// One built snapshot: the native graph plus its synced indexes, stamped
/// with the KV mutation epoch it was exported at. Immutable once built —
/// a rebuild publishes a fresh instance instead of mutating in place.
struct MirrorState {
    /// [`crate::db::Drevo::mutation_epoch`] at export time.
    epoch: u64,
    /// The mirrored graph.
    graph: NativeGraph,
    /// Label index synced over the full mirror.
    labels: NativeLabelIndex,
    /// Property index synced over the full mirror.
    props: NativePropertyIndex,
    /// Value cache synced over the full mirror.
    values: NativeValueCache,
}

/// Counters describing how the mirror has routed queries so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MirrorStats {
    /// Reads served from the native snapshot.
    pub native_hits: u64,
    /// Reads that fell back to the KV engine (mirror absent or stale).
    pub kv_fallbacks: u64,
    /// Writes (and non-mirrorable reads) routed to the KV engine.
    pub kv_routed: u64,
    /// Background rebuilds that failed (the mirror stays stale; reads keep
    /// falling back).
    pub rebuild_errors: u64,
}

/// The per-database native read mirror. See the [module docs](self).
#[derive(Default)]
pub struct NativeMirror {
    /// The latest built snapshot, if any.
    state: RwLock<Option<Arc<MirrorState>>>,
    /// Guards against overlapping background rebuilds.
    rebuilding: AtomicBool,
    /// Reads served natively.
    native_hits: AtomicU64,
    /// Reads that fell back to KV.
    kv_fallbacks: AtomicU64,
    /// Queries routed to KV because the mirror may not serve them.
    kv_routed: AtomicU64,
    /// Failed background rebuilds.
    rebuild_errors: AtomicU64,
}

impl NativeMirror {
    /// An empty mirror; the first read falls back to KV and triggers a
    /// build.
    pub fn new() -> Self {
        Self::default()
    }

    /// Execute `query`, routing it to the native snapshot when it is a
    /// mirrorable read and the snapshot is fresh, and to the KV engine
    /// otherwise. Writes always execute on KV.
    ///
    /// # Errors
    ///
    /// Returns whatever [`crate::cypher::executor::ExecError`] the chosen
    /// execution path produces. The two paths agree on results and errors
    /// for the mirrorable query surface (pinned by the differential
    /// corpus), so routing never changes semantics.
    pub fn execute(
        self: &Arc<Self>,
        db: &Arc<Drevo>,
        query: &Query,
        params: HashMap<String, Value>,
    ) -> Result<ExecResult, ExecError> {
        if mirror_can_serve(query) {
            if let Some(state) = self.fresh_state(db) {
                self.native_hits.fetch_add(1, Ordering::SeqCst);
                return execute_on_engine_with_indexes_and_values(
                    query,
                    &state.graph,
                    None,
                    Some(&state.labels),
                    Some(&state.props),
                    Some(&state.values),
                    params,
                );
            }
            self.kv_fallbacks.fetch_add(1, Ordering::SeqCst);
            self.spawn_rebuild(db);
        } else {
            self.kv_routed.fetch_add(1, Ordering::SeqCst);
        }
        executor::execute(query, db, params)
    }

    /// Build (or rebuild) the snapshot synchronously from `db` and publish
    /// it. Used at server startup for a warm first read and by tests that
    /// need deterministic freshness; the read path itself rebuilds in the
    /// background instead.
    ///
    /// # Errors
    ///
    /// Propagates export/apply failures; the previously published snapshot
    /// (if any) stays installed on error.
    pub fn rebuild_blocking(&self, db: &Drevo) -> Result<(), DrevoError> {
        let state = Self::build_state(db)?;
        *self.state.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(state));
        Ok(())
    }

    /// Routing counters so far.
    pub fn stats(&self) -> MirrorStats {
        MirrorStats {
            native_hits: self.native_hits.load(Ordering::SeqCst),
            kv_fallbacks: self.kv_fallbacks.load(Ordering::SeqCst),
            kv_routed: self.kv_routed.load(Ordering::SeqCst),
            rebuild_errors: self.rebuild_errors.load(Ordering::SeqCst),
        }
    }

    /// `true` when a snapshot is installed and its stamp matches the live
    /// mutation epoch.
    pub fn is_fresh(&self, db: &Drevo) -> bool {
        self.state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|s| s.epoch == db.mutation_epoch())
    }

    /// The installed snapshot, only if still stamped with the live epoch.
    fn fresh_state(&self, db: &Drevo) -> Option<Arc<MirrorState>> {
        let guard = self.state.read().unwrap_or_else(|e| e.into_inner());
        guard
            .as_ref()
            .filter(|s| s.epoch == db.mutation_epoch())
            .cloned()
    }

    /// Export the KV graph under quiesce and build the native snapshot.
    fn build_state(db: &Drevo) -> Result<MirrorState, DrevoError> {
        let (dump, epoch) = db.export_dump_consistent()?;
        let graph = NativeGraph::new();
        graph.apply_dump(dump)?;
        let mut labels = NativeLabelIndex::new();
        let mut props = NativePropertyIndex::new();
        let mut values = NativeValueCache::new();
        labels.sync(&graph);
        props.sync(&graph);
        values.sync(&graph);
        Ok(MirrorState {
            epoch,
            graph,
            labels,
            props,
            values,
        })
    }

    /// Kick off at most one background rebuild. If the epoch moves again
    /// while a rebuild runs, the published snapshot is simply stale and the
    /// next fallback read triggers another round — convergence without
    /// coordination.
    fn spawn_rebuild(self: &Arc<Self>, db: &Arc<Drevo>) {
        if self.rebuilding.swap(true, Ordering::SeqCst) {
            return;
        }
        let mirror = Arc::clone(self);
        let db = Arc::clone(db);
        std::thread::spawn(move || {
            if mirror.rebuild_blocking(&db).is_err() {
                mirror.rebuild_errors.fetch_add(1, Ordering::SeqCst);
            }
            mirror.rebuilding.store(false, Ordering::SeqCst);
        });
    }
}
