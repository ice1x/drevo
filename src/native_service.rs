//! Durable-native serving layer (RFC `docs/rfc-native-core.md` #307,
//! Phase 4/7 — the track toward retiring redb).
//!
//! [`crate::native_service::NativeService`] IS the store of record: a
//! WAL-backed [`crate::native::NativeGraph`]
//! (crash-recovering, fsync-per-statement — see
//! [`crate::native::NativeGraph::open_durable`]) plus the full native index
//! stack — label, property, value cache, **and full-text** — kept current by
//! tailing the engine's change-feed between statements.
//!
//! # Consistency model
//!
//! Indexes are synced *between* statements: before a query runs, the service
//! catches the indexes up to the change-feed head; within a statement that
//! writes, the executor already distrusts index narrowing (the in-statement
//! staleness gate), so a query always sees its own writes. A write committed
//! by a *concurrent* statement mid-query may or may not be observed — the
//! same read-committed-style race the KV engine has today; snapshot-isolated
//! serving is the MVCC phase's business (RFC Phase 3 knob), not this layer's.
//!
//! The service is `Sync`: reads run concurrently under a shared lock; only
//! the first query after a write pays the (incremental) index re-sync under
//! the exclusive lock.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::RwLock;

use crate::cypher::ast::Query;
use crate::cypher::executor::{
    execute_on_engine_with_context, ExecError, ExecResult, NativeQueryContext, Restriction, Value,
};
use crate::error::DrevoError;
use crate::lww::{OriginId, Stamp};
use crate::native::NativeGraph;
use crate::native_fts::{NativeFtsIndex, NativeFtsRelIndex};
use crate::native_label_index::NativeLabelIndex;
use crate::native_path_index::{NativePathIndex, PathIndexSpec};
use crate::native_property_index::NativePropertyIndex;
use crate::native_text_index::{NativeTextIndex, TextIndexSpec};
use crate::native_value_cache::NativeValueCache;

/// The index stack plus the change-feed head it is synced to.
struct ServiceIndexes {
    /// Label index (secondary `_labels`).
    labels: NativeLabelIndex,
    /// Property-equality index.
    props: NativePropertyIndex,
    /// Opt-in nested-path index (issue #578), over the registered specs.
    paths: NativePathIndex,
    /// Opt-in trigram text index (issue #589), over the registered specs.
    texts: NativeTextIndex,
    /// The [`NativeService::path_specs_version`] `paths` and `texts` were
    /// built for.
    paths_version: u64,
    /// Executor `NodeValue` projection cache.
    values: NativeValueCache,
    /// Trigram BM25 full-text index — `fts.search` served natively.
    fts: NativeFtsIndex,
    /// Trigram BM25 relationship full-text index — `fts.searchRelationships`
    /// served natively.
    fts_rel: NativeFtsRelIndex,
    /// [`crate::native::NativeGraph::change_head`] at the last sync.
    synced_head: u64,
    /// Whether the secondary indexes — `props`, `values`, `fts`, `fts_rel` —
    /// are kept. Off, they stay empty and statements scan instead.
    secondary: bool,
}

impl ServiceIndexes {
    /// The property-equality index, when secondary indexes are kept.
    fn props(&self) -> Option<&NativePropertyIndex> {
        self.secondary.then_some(&self.props)
    }

    /// The value cache, when secondary indexes are kept.
    fn values(&self) -> Option<&NativeValueCache> {
        self.secondary.then_some(&self.values)
    }

    /// The node full-text index, when secondary indexes are kept.
    fn fts(&self) -> Option<&NativeFtsIndex> {
        self.secondary.then_some(&self.fts)
    }

    /// The relationship full-text index, when secondary indexes are kept.
    fn fts_rel(&self) -> Option<&NativeFtsRelIndex> {
        self.secondary.then_some(&self.fts_rel)
    }

    /// Keep (`true`) or drop (`false`) the secondary indexes. Dropping frees
    /// their memory at once; keeping them again rebuilds them from the graph.
    fn set_secondary(&mut self, on: bool, graph: &NativeGraph) {
        if on == self.secondary {
            return;
        }
        self.props = NativePropertyIndex::new();
        self.values = NativeValueCache::new();
        self.fts = NativeFtsIndex::new();
        self.fts_rel = NativeFtsRelIndex::new();
        self.secondary = on;
        if on {
            self.props.sync(graph);
            self.values.sync(graph);
            self.fts.sync(graph);
            self.fts_rel.sync(graph);
        }
    }

    fn synced_over(
        graph: &NativeGraph,
        path_specs: Vec<PathIndexSpec>,
        text_specs: Vec<TextIndexSpec>,
        secondary: bool,
    ) -> Self {
        let mut idx = ServiceIndexes {
            labels: NativeLabelIndex::new(),
            props: NativePropertyIndex::new(),
            paths: NativePathIndex::new(path_specs),
            texts: NativeTextIndex::new(text_specs),
            paths_version: 0,
            values: NativeValueCache::new(),
            fts: NativeFtsIndex::new(),
            fts_rel: NativeFtsRelIndex::new(),
            synced_head: 0,
            secondary,
        };
        idx.catch_up(graph);
        idx
    }

    fn catch_up(&mut self, graph: &NativeGraph) {
        self.labels.sync(graph);
        self.paths.sync(graph);
        self.texts.sync(graph);
        if self.secondary {
            self.props.sync(graph);
            self.values.sync(graph);
            self.fts.sync(graph);
            self.fts_rel.sync(graph);
        }
        self.synced_head = graph.change_head();
    }
}

/// Prefix of the placeholder title older drevo versions stored for a node
/// created without one through Cypher (#602 stores an empty title instead).
/// Such a title is an identifier, not text: it reads as no title, and semantic
/// auto-embedding never reads it.
pub(crate) const SYNTHETIC_TITLE_PREFIX: &str = "__cypher__:";

/// A node's dedicated text fields. A node keeps `title` and `body` outside its
/// property map (Cypher `CREATE` / `SET` route them there), yet a semantic
/// target may name either as its `text_property` (#536).
#[derive(Debug, Clone, Copy, Default)]
pub struct NodeTextFields<'a> {
    /// The node's title.
    pub title: &'a str,
    /// The node's body text.
    pub body: &'a str,
}

impl<'a> NodeTextFields<'a> {
    /// The text fields of a stored node.
    #[must_use]
    pub fn of(node: &'a crate::model::Node) -> Self {
        Self {
            title: &node.title,
            body: &node.body,
        }
    }
}

/// The non-empty text a semantic target with `text_property` reads from a node:
/// the string property of that name, else — for `title` / `body` — the node's
/// own field. A synthesised placeholder title is not text and reads as `None`.
pub(crate) fn node_text<'a>(
    properties: &'a crate::model::Properties,
    fields: NodeTextFields<'a>,
    text_property: &str,
) -> Option<&'a str> {
    let text = match properties.get(text_property) {
        Some(value) => value.as_str(),
        None => match text_property {
            "title" if !fields.title.starts_with(SYNTHETIC_TITLE_PREFIX) => Some(fields.title),
            "body" => Some(fields.body),
            _ => None,
        },
    };
    text.filter(|s| !s.is_empty())
}

/// The name a service is served under until a
/// [`crate::database_registry::DatabaseRegistry`] names it.
const DEFAULT_DATABASE_NAME: &str = "drevo";

/// A durable native graph serving Cypher with its full index stack. See the
/// [module docs](self).
pub struct NativeService {
    /// The WAL-backed store of record.
    graph: NativeGraph,
    /// Index stack; shared for reads, exclusive to re-sync after writes.
    indexes: RwLock<ServiceIndexes>,
    /// Server-side text embedder, when the operator configured one —
    /// installed once at startup (mirroring the KV handle's embedder), read
    /// lock-free afterwards. Lets `drevo.semantic.embed` / `.query` run on
    /// the durable engine.
    #[cfg(feature = "http")]
    embedder: std::sync::OnceLock<std::sync::Arc<dyn crate::embeddings::TextEmbedder>>,
    /// Compact the WAL (and trim the change-feed) after this many appended
    /// ops since the last compaction — the runtime bound on log growth
    /// (reopen-time compaction alone lets a long-running server's log grow
    /// with history, not state).
    compact_every_ops: u64,
    /// [`crate::native::NativeGraph::change_head`] at the last compaction.
    last_compact_head: AtomicU64,
    /// Guards against overlapping compactions.
    compacting: AtomicBool,
    /// Opt-in statement timeout in milliseconds (#547); `0` = no limit.
    statement_timeout_ms: AtomicU64,
    /// Held while a writing auto-commit statement runs as its transaction,
    /// so such statements take turns instead of failing each other's
    /// commits (#715).
    autocommit_writer: std::sync::Mutex<()>,
    /// How many of the latest committed changes compaction keeps in the
    /// change feed for event subscribers to resume from (`GET /events`).
    event_history: AtomicU64,
    /// Node-side auto-embedding registry — the native home of what the KV
    /// handle keeps in `Drevo::semantic` (issue #447). Authoritative
    /// control-plane state, persisted to the `semantic.json` sidecar.
    semantic: RwLock<crate::semantic_index::SemanticIndexRegistry>,
    /// Relationship-side registry — the edge mirror of [`Self::semantic`],
    /// the native home of `Drevo::rel_semantic`.
    rel_semantic: RwLock<crate::semantic_index::SemanticIndexRegistry>,
    /// Named vector-index registry (issue #532) — binds a Neo4j-style index name
    /// to the `(label, property)` it targets, so `CREATE VECTOR INDEX` /
    /// `db.index.vector.queryNodes` round-trip. Persisted in the same sidecar.
    vector_indexes: RwLock<crate::vector_index_registry::VectorIndexRegistry>,
    /// Named nested-path and text index definitions (issues #578, #589).
    /// Persisted in the same sidecar; the index data lives in
    /// [`ServiceIndexes::paths`] and [`ServiceIndexes::texts`].
    path_indexes: RwLock<crate::path_index_registry::PathIndexRegistry>,
    /// Named UNIQUE constraints (issue #604). The graph enforces them; this
    /// keeps their names and is persisted in the same sidecar.
    constraints: RwLock<crate::constraint_registry::ConstraintRegistry>,
    /// Installed triggers (`apoc.trigger.*`), persisted in the same sidecar.
    triggers: RwLock<crate::trigger::TriggerRegistry>,
    /// Whether the secondary indexes should be kept (persisted in the same
    /// sidecar). The index stack follows it at its next sync, so a statement
    /// can change it while it holds the indexes.
    secondary_wanted: AtomicBool,
    /// Bumped on every path- or text-index DDL, so the next statement rebuilds
    /// [`ServiceIndexes::paths`] and [`ServiceIndexes::texts`] over the new
    /// definitions.
    path_specs_version: AtomicU64,
    /// Sidecar path for the registries (`<wal dir>/semantic.json`), or
    /// `None` for an in-memory service (nothing to persist). Always `None` on
    /// wasm, which has no filesystem to persist to.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    semantic_sidecar: Option<std::path::PathBuf>,
    /// Cumulative auto-embed failures per target (issue #447; native mirror of
    /// `Drevo::embed_failures`), keyed by `(target_kind, name, embedding_property)`.
    /// Auto-embed on write is fail-open — a transient embedder outage never
    /// fails a write — so each swallowed failure is tallied here to feed
    /// `drevo.semantic.status`'s `failed_count` / `last_error`. Runtime-only
    /// (not persisted); `http`-gated like the embedder that produces it.
    #[cfg(feature = "http")]
    embed_failures:
        std::sync::Mutex<std::collections::HashMap<(String, String, String), EmbedFailureStat>>,
    /// The server's running statements and open transactions, shared by
    /// every database of a [`crate::database_registry::DatabaseRegistry`].
    #[cfg(not(target_arch = "wasm32"))]
    transactions: RwLock<std::sync::Arc<crate::transactions::Transactions>>,
    /// The name this database is served under, for transaction ids.
    #[cfg(not(target_arch = "wasm32"))]
    database_name: RwLock<String>,
    /// The listed explicit transactions, by id.
    #[cfg(not(target_arch = "wasm32"))]
    explicit_txs: std::sync::Mutex<
        std::collections::HashMap<crate::native::NativeTxId, crate::transactions::Listed>,
    >,
    /// Named events posted in open explicit transactions, published when
    /// they commit.
    tx_events: std::sync::Mutex<HashMap<crate::native::NativeTxId, Vec<crate::event_bus::Pending>>>,
    /// Named events of prepared transactions, by global transaction id.
    prepared_events: std::sync::Mutex<HashMap<String, Vec<crate::event_bus::Pending>>>,
    /// The change parameters of prepared transactions whose `after` and
    /// `afterAsync` triggers run once they are committed, by gid.
    prepared_triggers: std::sync::Mutex<HashMap<String, HashMap<String, Value>>>,
    /// Runs `afterAsync` triggers off the committing thread, once
    /// [`start_trigger_worker`](Self::start_trigger_worker) has been called.
    #[cfg(not(target_arch = "wasm32"))]
    trigger_worker: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<AsyncTriggers>>>,
    /// This database's published named events.
    event_bus: crate::event_bus::EventBus,
}

/// A per-target tally of swallowed native auto-embed failures (issue #447;
/// mirrors the KV `crate::db::EmbedFailureStat`).
#[cfg(feature = "http")]
#[derive(Debug, Clone, Default)]
struct EmbedFailureStat {
    /// How many embed attempts have been swallowed for this target.
    count: u64,
    /// The most recent failure message, for the `last_error` column.
    last_error: String,
}

/// `afterAsync` triggers queued for the trigger worker, with the change
/// parameters of the transaction that fired them.
#[cfg(not(target_arch = "wasm32"))]
struct AsyncTriggers {
    triggers: Vec<crate::trigger::Trigger>,
    params: HashMap<String, Value>,
}

/// On-disk shape of the `semantic.json` sidecar: both registries in one file,
/// atomically rewritten on every control-plane mutation (the native
/// counterpart of the KV `meta:semantic_registry` / `..._rel_registry` blobs).
#[cfg(not(target_arch = "wasm32"))]
#[derive(serde::Serialize, serde::Deserialize)]
struct SemanticSidecar {
    node: crate::semantic_index::SemanticIndexRegistry,
    rel: crate::semantic_index::SemanticIndexRegistry,
    /// Named vector-index registrations (issue #532). `#[serde(default)]` keeps
    /// sidecars written before #532 (which had only `node`/`rel`) loadable.
    #[serde(default)]
    vector: crate::vector_index_registry::VectorIndexRegistry,
    /// Nested-path and text index definitions (issues #578, #589); absent in
    /// older sidecars.
    #[serde(default)]
    path: crate::path_index_registry::PathIndexRegistry,
    /// UNIQUE constraint definitions (issue #604); absent in older sidecars.
    #[serde(default)]
    constraints: crate::constraint_registry::ConstraintRegistry,
    /// Installed triggers; absent in older sidecars.
    #[serde(default)]
    triggers: crate::trigger::TriggerRegistry,
    /// Whether the secondary indexes are kept; absent (kept) in older
    /// sidecars.
    #[serde(default = "secondary_indexes_default")]
    secondary_indexes: bool,
}

/// Secondary indexes are kept unless a database turned them off.
#[cfg(not(target_arch = "wasm32"))]
fn secondary_indexes_default() -> bool {
    true
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for SemanticSidecar {
    fn default() -> Self {
        Self {
            node: Default::default(),
            rel: Default::default(),
            vector: Default::default(),
            path: Default::default(),
            constraints: Default::default(),
            triggers: Default::default(),
            secondary_indexes: true,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn semantic_sidecar_path(wal_path: &std::path::Path) -> std::path::PathBuf {
    wal_path.with_file_name("semantic.json")
}

/// The context `semantic.json` is sealed under in an encrypted store.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) const SEMANTIC_CONTEXT: &[u8] = b"drevo semantic";

/// Read a sidecar file of the service: in the clear, or sealed with the
/// process's encryption key ([`NativeService::set_encryption_key`]); `None`
/// when there is none.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn read_sidecar(
    path: &std::path::Path,
    context: &[u8],
) -> std::io::Result<Option<Vec<u8>>> {
    #[cfg(feature = "encryption")]
    {
        drevo_core::encryption::read_file(path, NativeService::encryption_key().as_deref(), context)
    }
    #[cfg(not(feature = "encryption"))]
    {
        let _ = context;
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Write a sidecar file of the service atomically, sealed with the process's
/// encryption key when there is one.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn write_sidecar(
    path: &std::path::Path,
    context: &[u8],
    bytes: &[u8],
) -> std::io::Result<()> {
    #[cfg(feature = "encryption")]
    {
        drevo_core::encryption::write_file(
            path,
            NativeService::encryption_key().as_deref(),
            context,
            bytes,
        )
    }
    #[cfg(not(feature = "encryption"))]
    {
        let _ = context;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    }
}

/// `bytes` as a sidecar stores them — for the files a backup carries.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn sidecar_bytes(context: &[u8], bytes: Vec<u8>) -> std::io::Result<Vec<u8>> {
    #[cfg(feature = "encryption")]
    {
        drevo_core::encryption::file_bytes(
            NativeService::encryption_key().as_deref(),
            context,
            &bytes,
        )
    }
    #[cfg(not(feature = "encryption"))]
    {
        let _ = context;
        Ok(bytes)
    }
}

/// The plaintext of sidecar `bytes` read some other way than
/// [`read_sidecar`]: opened with the process's encryption key when there is
/// one.
///
/// # Errors
/// Encrypted bytes without a key, plain ones with a key, or ones that do not
/// decrypt.
#[cfg(all(not(target_arch = "wasm32"), any(feature = "http", feature = "auth")))]
pub(crate) fn decode_sidecar(context: &[u8], bytes: Vec<u8>) -> std::io::Result<Vec<u8>> {
    #[cfg(feature = "encryption")]
    {
        drevo_core::encryption::open_file_bytes(
            NativeService::encryption_key().as_deref(),
            context,
            bytes,
        )
    }
    #[cfg(not(feature = "encryption"))]
    {
        let _ = context;
        Ok(bytes)
    }
}

/// Load both registries from the sidecar, or empty ones if absent or
/// unparseable (best-effort, mirroring the tombstone sidecar). A sidecar that
/// cannot be read — or does not decrypt with the store's key — fails the
/// open: starting with no index definitions would silently drop them.
#[cfg(not(target_arch = "wasm32"))]
fn load_semantic_sidecar(path: &std::path::Path) -> std::io::Result<SemanticSidecar> {
    Ok(match read_sidecar(path, SEMANTIC_CONTEXT)? {
        Some(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        None => SemanticSidecar::default(),
    })
}

/// The key every store this process opens is encrypted with.
#[cfg(all(not(target_arch = "wasm32"), feature = "encryption"))]
static ENCRYPTION_KEY: RwLock<Option<std::sync::Arc<drevo_core::encryption::Key>>> =
    RwLock::new(None);

/// The write-ahead-log format stores are opened with, as
/// [`WalFormat`](crate::wal_format::WalFormat) ⇄ `u8` (issue #582). Process-wide,
/// so every store — the default database, the ones the registry opens, FFI —
/// follows the server's `DREVO_WAL_FORMAT` without threading it through each.
#[cfg(not(target_arch = "wasm32"))]
static DEFAULT_WAL_FORMAT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

impl NativeService {
    /// Set the write-ahead-log format every store opened from now on uses
    /// (issue #582). A store whose log is in another format is read as it is
    /// and rewritten in this one by the compaction [`Self::open`] runs.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_default_wal_format(format: crate::wal_format::WalFormat) {
        use crate::wal_format::WalFormat;
        let v = match format {
            WalFormat::Json => 0,
            WalFormat::Cbor => 1,
            WalFormat::MsgPack => 2,
            WalFormat::CborZstd => 3,
            WalFormat::CborCompact => 4,
            WalFormat::CborCompactZstd => 5,
        };
        DEFAULT_WAL_FORMAT.store(v, Ordering::SeqCst);
    }

    /// Encrypt every store opened from now on with `key` — its log, its
    /// engine sidecars and the service's own `semantic.json` — or none
    /// (`None`, the default). Process-wide, like the WAL format, so the
    /// databases the registry opens follow the server's key. A store must
    /// be opened with the key it was written with.
    #[cfg(all(not(target_arch = "wasm32"), feature = "encryption"))]
    pub fn set_encryption_key(key: Option<drevo_core::encryption::Key>) {
        *ENCRYPTION_KEY.write().unwrap_or_else(|e| e.into_inner()) = key.map(std::sync::Arc::new);
    }

    /// The key set with [`Self::set_encryption_key`], if any.
    #[cfg(all(not(target_arch = "wasm32"), feature = "encryption"))]
    #[must_use]
    pub fn encryption_key() -> Option<std::sync::Arc<drevo_core::encryption::Key>> {
        ENCRYPTION_KEY
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The write-ahead-log format new opens use (see
    /// [`Self::set_default_wal_format`]); JSON unless changed.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn default_wal_format() -> crate::wal_format::WalFormat {
        use crate::wal_format::WalFormat;
        match DEFAULT_WAL_FORMAT.load(Ordering::SeqCst) {
            1 => WalFormat::Cbor,
            2 => WalFormat::MsgPack,
            3 => WalFormat::CborZstd,
            4 => WalFormat::CborCompact,
            5 => WalFormat::CborCompactZstd,
            _ => WalFormat::Json,
        }
    }

    /// Open (or create) the durable store at `path` — the write-ahead log the
    /// graph recovers from and appends to. The log is compacted on open, so a
    /// long overwrite history costs restart time only once, and the indexes
    /// are built before the first query.
    ///
    /// # Errors
    ///
    /// Propagates [`crate::error::DrevoError`] from WAL recovery, compaction,
    /// or index construction.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, DrevoError> {
        Self::open_with_compact_threshold(path, Self::DEFAULT_COMPACT_EVERY_OPS)
    }

    /// [`Self::open`] with an explicit runtime-compaction threshold: after
    /// `compact_every_ops` appended ops, the next statement compacts the
    /// WAL in place (writes are quiesced for the rewrite's duration) and
    /// trims the consumed change-feed history. Exposed for operators and
    /// tests; [`Self::open`] uses [`Self::DEFAULT_COMPACT_EVERY_OPS`].
    ///
    /// # Errors
    ///
    /// Propagates [`crate::error::DrevoError`] from WAL recovery,
    /// compaction, or index construction.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open_with_compact_threshold(
        path: impl AsRef<std::path::Path>,
        compact_every_ops: u64,
    ) -> Result<Self, DrevoError> {
        Self::open_store(path.as_ref(), compact_every_ops, false)
    }

    /// Open the durable store at `path` for reading only: queries, search
    /// and export work, and every write or schema change fails with
    /// [`DrevoError::ReadOnly`]. Nothing next to the log is written, so the
    /// store can sit in a directory the process cannot write to.
    ///
    /// # Errors
    ///
    /// [`DrevoError::Io`] when there is no log at `path`,
    /// [`DrevoError::Locked`] while a writer has it open, and as
    /// [`Self::open`] for a corrupt log.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open_read_only(path: impl AsRef<std::path::Path>) -> Result<Self, DrevoError> {
        Self::open_store(path.as_ref(), Self::DEFAULT_COMPACT_EVERY_OPS, true)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn open_store(
        path: &std::path::Path,
        compact_every_ops: u64,
        read_only: bool,
    ) -> Result<Self, DrevoError> {
        // The replica identity (issue #389) lives on the store of record:
        // `NativeGraph::open_durable` loads/mints+persists `origin.json` next to
        // the WAL, reused across restarts.
        #[cfg(feature = "encryption")]
        let key = Self::encryption_key();
        #[cfg(feature = "encryption")]
        let graph = match (key, read_only) {
            (Some(key), true) => Some(NativeGraph::open_read_only_encrypted(path, (*key).clone())?),
            (Some(key), false) => Some(NativeGraph::open_durable_encrypted(
                path,
                Self::default_wal_format(),
                (*key).clone(),
            )?),
            (None, _) => None,
        };
        #[cfg(not(feature = "encryption"))]
        let graph: Option<NativeGraph> = None;
        let graph = if let Some(graph) = graph {
            if !read_only {
                graph.compact_wal()?;
            }
            graph
        } else if read_only {
            NativeGraph::open_read_only(path)?
        } else {
            let graph = NativeGraph::open_durable_with(path, Self::default_wal_format())?;
            // Compaction also rewrites a log written in another format in the
            // configured one (issue #582).
            graph.compact_wal()?;
            graph
        };
        let last_compact_head = AtomicU64::new(graph.change_head());
        let sidecar = semantic_sidecar_path(path);
        let SemanticSidecar {
            node,
            rel,
            vector,
            path: path_indexes,
            constraints,
            triggers,
            secondary_indexes,
        } = load_semantic_sidecar(&sidecar).map_err(DrevoError::Io)?;
        // Constraints are schema, not WAL data: install them over the replayed
        // graph. Data written through drevo already satisfies them; if a
        // replicated or imported write broke one, keep serving and say so.
        for c in constraints.list() {
            if let Err(e) = graph.add_constraint(c.constraint()) {
                #[cfg(feature = "http")]
                tracing::error!(constraint = %c.name, error = %e, "constraint not enforced: existing data violates it");
                #[cfg(not(feature = "http"))]
                let _ = e;
            }
        }
        let indexes = RwLock::new(ServiceIndexes::synced_over(
            &graph,
            path_indexes.specs(),
            path_indexes.text_specs(),
            secondary_indexes,
        ));
        let service = Self {
            graph,
            indexes,
            #[cfg(feature = "http")]
            embedder: std::sync::OnceLock::new(),
            compact_every_ops,
            last_compact_head,
            compacting: AtomicBool::new(false),
            statement_timeout_ms: AtomicU64::new(0),
            autocommit_writer: std::sync::Mutex::new(()),
            event_history: AtomicU64::new(Self::DEFAULT_EVENT_HISTORY),
            semantic: RwLock::new(node),
            rel_semantic: RwLock::new(rel),
            vector_indexes: RwLock::new(vector),
            path_indexes: RwLock::new(path_indexes),
            constraints: RwLock::new(constraints),
            triggers: RwLock::new(triggers),
            secondary_wanted: AtomicBool::new(secondary_indexes),
            path_specs_version: AtomicU64::new(0),
            semantic_sidecar: Some(sidecar),
            #[cfg(feature = "http")]
            embed_failures: std::sync::Mutex::new(std::collections::HashMap::new()),
            #[cfg(not(target_arch = "wasm32"))]
            transactions: RwLock::new(crate::transactions::Transactions::new()),
            #[cfg(not(target_arch = "wasm32"))]
            database_name: RwLock::new(DEFAULT_DATABASE_NAME.to_string()),
            #[cfg(not(target_arch = "wasm32"))]
            explicit_txs: std::sync::Mutex::new(std::collections::HashMap::new()),
            tx_events: std::sync::Mutex::new(HashMap::new()),
            prepared_events: std::sync::Mutex::new(HashMap::new()),
            prepared_triggers: std::sync::Mutex::new(HashMap::new()),
            #[cfg(not(target_arch = "wasm32"))]
            trigger_worker: std::sync::OnceLock::new(),
            event_bus: crate::event_bus::EventBus::new(),
        };
        // The engine seeds the feed with a copy of the whole graph; the
        // indexes have consumed it, so keep only the event history.
        service.trim_feed();
        Ok(service)
    }

    /// Install the server-side query embedder (once; later calls return
    /// `false` and change nothing) — the durable-engine counterpart of
    /// `Drevo::set_embedder`.
    #[cfg(feature = "http")]
    pub fn set_embedder(
        &self,
        embedder: std::sync::Arc<dyn crate::embeddings::TextEmbedder>,
    ) -> bool {
        self.embedder.set(embedder).is_ok()
    }

    /// An ephemeral (non-durable) service — the same serving stack over an
    /// in-memory graph, for tests and embedded use.
    pub fn in_memory() -> Self {
        let graph = NativeGraph::new();
        let indexes = RwLock::new(ServiceIndexes::synced_over(
            &graph,
            Vec::new(),
            Vec::new(),
            true,
        ));
        Self {
            graph,
            indexes,
            #[cfg(feature = "http")]
            embedder: std::sync::OnceLock::new(),
            compact_every_ops: Self::DEFAULT_COMPACT_EVERY_OPS,
            last_compact_head: AtomicU64::new(0),
            compacting: AtomicBool::new(false),
            statement_timeout_ms: AtomicU64::new(0),
            autocommit_writer: std::sync::Mutex::new(()),
            event_history: AtomicU64::new(Self::DEFAULT_EVENT_HISTORY),
            semantic: RwLock::new(crate::semantic_index::SemanticIndexRegistry::new()),
            rel_semantic: RwLock::new(crate::semantic_index::SemanticIndexRegistry::new()),
            vector_indexes: RwLock::new(crate::vector_index_registry::VectorIndexRegistry::new()),
            path_indexes: RwLock::new(crate::path_index_registry::PathIndexRegistry::default()),
            constraints: RwLock::new(crate::constraint_registry::ConstraintRegistry::default()),
            triggers: RwLock::new(crate::trigger::TriggerRegistry::default()),
            secondary_wanted: AtomicBool::new(true),
            path_specs_version: AtomicU64::new(0),
            semantic_sidecar: None,
            #[cfg(feature = "http")]
            embed_failures: std::sync::Mutex::new(std::collections::HashMap::new()),
            #[cfg(not(target_arch = "wasm32"))]
            transactions: RwLock::new(crate::transactions::Transactions::new()),
            #[cfg(not(target_arch = "wasm32"))]
            database_name: RwLock::new(DEFAULT_DATABASE_NAME.to_string()),
            #[cfg(not(target_arch = "wasm32"))]
            explicit_txs: std::sync::Mutex::new(std::collections::HashMap::new()),
            tx_events: std::sync::Mutex::new(HashMap::new()),
            prepared_events: std::sync::Mutex::new(HashMap::new()),
            prepared_triggers: std::sync::Mutex::new(HashMap::new()),
            #[cfg(not(target_arch = "wasm32"))]
            trigger_worker: std::sync::OnceLock::new(),
            event_bus: crate::event_bus::EventBus::new(),
        }
    }

    /// This replica's stable [`OriginId`] (issue #389) — delegated to the store
    /// of record, which persists it next to the WAL.
    pub fn origin_id(&self) -> OriginId {
        self.graph.origin_id()
    }

    /// Issue the next causal [`Stamp`] `(hlc, origin)` for a write on this
    /// replica (delegated to the store of record). Strictly increasing, so a
    /// sequence of local writes is totally ordered; two replicas' stamps are
    /// ordered by HLC then origin.
    pub fn next_stamp(&self) -> Stamp {
        self.graph.next_stamp()
    }

    /// Default runtime-compaction threshold (appended ops between
    /// compactions). Large enough that steady write loads compact rarely,
    /// small enough that the log never dwarfs the state.
    pub const DEFAULT_COMPACT_EVERY_OPS: u64 = 4096;

    /// How many of the latest committed changes stay in the change feed
    /// after compaction, for event subscribers to resume from.
    pub const DEFAULT_EVENT_HISTORY: u64 = 10_000;

    /// Whether writes and schema changes are refused: opened with
    /// [`Self::open_read_only`] or switched with [`Self::set_read_only`].
    pub fn is_read_only(&self) -> bool {
        self.graph.is_read_only()
    }

    /// Refuse every write and schema change from now on (`true`), or accept
    /// them again (`false`).
    ///
    /// # Errors
    ///
    /// [`DrevoError::ReadOnly`] to make a store opened with
    /// [`Self::open_read_only`] writable.
    pub fn set_read_only(&self, on: bool) -> Result<(), DrevoError> {
        Ok(self.graph.set_read_only(on)?)
    }

    /// The underlying engine — read-side introspection (counts, status).
    pub fn graph(&self) -> &NativeGraph {
        &self.graph
    }

    /// The storage-panel bloat report for the WAL store — the engine-agnostic
    /// counterpart of `Drevo::bloat_report`. `file_bytes` is the
    /// physical WAL size, `logical_bytes`/`stored_bytes` the size a compacted
    /// log would occupy (the append-only WAL accumulates superseded upserts,
    /// tombstones and old versions); `bloat_ratio = file / logical` is the
    /// reclaimable fraction. Secondary indexes are in-memory (rebuilt from the
    /// WAL) so they add no on-disk footprint (`index_bytes = 0`).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn storage_bloat(&self) -> crate::report::BloatReport {
        let g = &self.graph;
        let file_bytes = g.wal_bytes();
        let logical = g.wal_compacted_bytes();
        let bloat_ratio = match file_bytes {
            Some(f) if logical > 0 => Some(f as f64 / logical as f64),
            _ => None,
        };
        crate::report::BloatReport {
            file_bytes,
            stored_bytes: logical,
            logical_bytes: logical,
            index_bytes: 0,
            node_count: g.node_count(),
            edge_count: g.edge_count(),
            bloat_ratio,
        }
    }

    /// Per-keyspace storage breakdown for the panel — the WAL engine's
    /// counterpart of `Drevo::keyspace_stats`. The durable WAL
    /// stores only records on disk; these rows are the live in-memory index
    /// structures (records, adjacency, title, kind), so the panel's Keyspaces
    /// table is populated on both engines. FTS/vector keyspaces are
    /// KV-secondary-only and absent here.
    #[must_use]
    pub fn keyspace_stats(&self) -> Vec<crate::report::KeyspaceStat> {
        self.graph
            .keyspace_stats()
            .into_iter()
            .map(
                |(prefix, entries, content_bytes)| crate::report::KeyspaceStat {
                    prefix,
                    entries,
                    content_bytes,
                },
            )
            .collect()
    }

    /// Compact the WAL — rewrite it as the current state (atomic temp + fsync +
    /// rename) — and report the reclaimed bytes: the engine-agnostic counterpart
    /// of `Drevo::shrink_online` (the storage panel's `shrink`).
    /// Writes are quiesced for the rewrite's duration.
    ///
    /// # Errors
    /// Propagates [`DrevoError`] on a filesystem or encode failure.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn compact(&self) -> Result<crate::report::CompactReport, DrevoError> {
        let stats = self.graph.compact_wal()?;
        // The log now equals the current state, so restart the runtime
        // auto-compaction threshold from here.
        self.last_compact_head
            .store(self.graph.change_head(), Ordering::SeqCst);
        Ok(crate::report::CompactReport {
            bytes_before: stats.bytes_before,
            bytes_after: stats.bytes_after,
            bytes_reclaimed: stats.reclaimed(),
            next_node_id: self.graph.next_node_id(),
            next_edge_id: self.graph.next_edge_id(),
        })
    }

    /// Execute one Cypher statement with the index stack attached. Reads run
    /// concurrently; the first statement after a write re-syncs the indexes
    /// first.
    ///
    /// # Errors
    ///
    /// Returns the executor's [`crate::cypher::executor::ExecError`];
    /// KV-secondary-only features (vector / semantic procedures) surface
    /// [`crate::cypher::executor::ExecError::EngineCapability`].
    pub fn execute(
        &self,
        query: &Query,
        params: HashMap<String, Value>,
    ) -> Result<ExecResult, ExecError> {
        self.execute_as(query, params, None)
    }

    /// [`execute`](Self::execute) for a signed-in user (#636): the statement
    /// runs with `restriction`'s privileges. A writing statement of a user who
    /// may not write everywhere runs as one transaction, so a refused write
    /// leaves nothing of the statement behind.
    ///
    /// # Errors
    ///
    /// As [`execute`](Self::execute), and
    /// [`ExecError::Forbidden`]
    /// for a missing privilege.
    pub fn execute_as(
        &self,
        query: &Query,
        params: HashMap<String, Value>,
        restriction: Option<Restriction<'_>>,
    ) -> Result<ExecResult, ExecError> {
        #[cfg(feature = "http")]
        let watch = crate::problems::SlowWatch::start(&params);
        let result = self.execute_autocommit(query, params, restriction);
        #[cfg(feature = "http")]
        if let Some(watch) = watch {
            watch.finish(&self.database_name(), false);
        }
        result
    }

    /// The body of [`execute_as`](Self::execute_as), without the
    /// slow-statement log.
    fn execute_autocommit(
        &self,
        query: &Query,
        params: HashMap<String, Value>,
        restriction: Option<Restriction<'_>>,
    ) -> Result<ExecResult, ExecError> {
        // Schema lives beside the log, not in it, so the engine cannot refuse
        // it; data writes are refused by the engine itself.
        if self.is_read_only() && query_changes_schema(query) {
            return Err(ExecError::Storage(DrevoError::ReadOnly));
        }
        let checked_writes = restriction.is_some_and(|r| !r.writes_everything());
        let triggers = self.triggers_active();
        let fired = std::cell::RefCell::new(None);
        let (result, events) = crate::event_bus::collect(|| {
            self.listed(|| {
                crate::cypher::executor::deadline::with_statement_timeout(
                    self.statement_timeout(),
                    || {
                        let writes = query_writes(query)
                            || ((checked_writes || triggers)
                                && query_calls_writing_procedure(query));
                        // A writing statement is one transaction: one log
                        // record and fsync, and nothing of it stays when it
                        // fails (#715).
                        if writes {
                            let (rows, params) =
                                self.execute_as_transaction(query, &params, restriction)?;
                            *fired.borrow_mut() = params;
                            Ok(rows)
                        } else {
                            self.with_fresh_indexes(|idx| {
                                self.execute_with(idx, query, params, restriction)
                            })
                        }
                    },
                )
            })
        });
        if result.is_ok() {
            self.publish_events(events);
            if let Some(params) = fired.into_inner() {
                self.fire_after(params);
            }
        }
        self.maybe_compact();
        result
    }

    /// Publish named events whose statement or transaction committed.
    fn publish_events(&self, events: Vec<crate::event_bus::Pending>) {
        self.event_bus.publish(
            events,
            || self.graph.change_head(),
            self.event_history.load(Ordering::Relaxed),
        );
    }

    /// This database's published named events (`drevo.events.post`).
    #[must_use]
    pub fn event_bus(&self) -> &crate::event_bus::EventBus {
        &self.event_bus
    }

    /// Run a writing statement as one transaction, committed at its end
    /// (#606). Used while an `IS NOT NULL` or `NODE KEY` constraint is
    /// declared: those are checked at commit, so `CREATE (n:User) SET
    /// n.email = …` or `MERGE … ON CREATE SET …` satisfy them by the end of
    /// the statement, and a statement that does not writes nothing. Reads in
    /// the statement see its own writes and are not narrowed by the
    /// property, label, path or text indexes, which describe the committed
    /// graph. A commit that lost a race with another writer is retried.
    ///
    /// While triggers are installed every writing statement runs this way:
    /// the `before` triggers run in the transaction ahead of the commit, and
    /// the change parameters are returned for the `after` phases.
    fn execute_as_transaction(
        &self,
        query: &Query,
        params: &HashMap<String, Value>,
        restriction: Option<Restriction<'_>>,
    ) -> Result<(ExecResult, Option<HashMap<String, Value>>), ExecError> {
        const ATTEMPTS: usize = 8;
        let _turn = self
            .autocommit_writer
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut attempt = 0;
        loop {
            attempt += 1;
            // One statement: bounded by the statement timeout, not by the
            // write limit of a client's transaction.
            let tx = self.graph.tx_begin_statement();
            let Some(engine) = self.graph.tx_engine(tx) else {
                return Err(ExecError::Storage(DrevoError::Io(std::io::Error::other(
                    "the statement's transaction closed before it ran",
                ))));
            };
            // Only the attempt that commits posts its events.
            let (result, events) = crate::event_bus::collect(|| {
                self.with_fresh_indexes(|idx| {
                    // A fresh transaction: the indexes describe its snapshot
                    // unless another writer committed since it began.
                    let overlay = self.graph.tx_index_overlay(tx);
                    let indexed = overlay.is_some();
                    let ctx = NativeQueryContext {
                        fts: idx.fts(),
                        fts_rel: idx.fts_rel(),
                        labels: indexed.then_some(&idx.labels),
                        properties: idx.props().filter(|_| indexed),
                        paths: indexed.then_some(&idx.paths),
                        texts: indexed.then_some(&idx.texts),
                        values: idx.values(),
                        #[cfg(feature = "http")]
                        embedder: self.embedder.get(),
                        #[cfg(not(feature = "http"))]
                        embedder: None,
                        semantic: Some(self),
                        in_transaction: false,
                        tx_has_written: false,
                        tx_written: overlay.as_ref(),
                        restriction,
                    };
                    execute_on_engine_with_context(query, &engine, &ctx, params.clone())
                })
            });
            let rows = match result {
                Ok(rows) => rows,
                Err(e) => {
                    self.graph.tx_rollback(tx);
                    return Err(e);
                }
            };
            let fired = match self.fire_before(tx) {
                Ok(fired) => fired,
                Err(failure) => {
                    self.graph.tx_rollback(tx);
                    return Err(ExecError::TriggerFailed {
                        name: failure.name,
                        message: failure.message,
                    });
                }
            };
            match self.graph.tx_commit(tx) {
                Ok(()) => {
                    for event in events {
                        crate::event_bus::post(event);
                    }
                    return Ok((rows, fired));
                }
                Err(crate::native::CommitError::Conflict) if attempt < ATTEMPTS => {}
                Err(crate::native::CommitError::Constraint(v)) => {
                    return Err(ExecError::Storage(DrevoError::ConstraintViolation {
                        kind: v.kind,
                        message: v.message,
                    }));
                }
                Err(crate::native::CommitError::ReadOnly) => {
                    return Err(ExecError::Storage(DrevoError::ReadOnly));
                }
                Err(crate::native::CommitError::PreparedPending(gids)) => {
                    return Err(ExecError::Storage(DrevoError::PreparedTransactionPending(
                        gids,
                    )));
                }
                Err(e) => {
                    return Err(ExecError::Storage(DrevoError::Io(std::io::Error::other(
                        e.to_string(),
                    ))));
                }
            }
        }
    }

    /// Limit every statement [`execute`](Self::execute) runs to `limit` of
    /// wall-clock time (#547); `None` (the default) means no limit. A
    /// statement over the limit fails with
    /// [`ExecError::Timeout`] and
    /// releases the index lock it held. The clock starts when the statement
    /// starts, so time spent waiting for the lock behind a writer counts.
    /// Sub-millisecond limits round up to 1 ms.
    pub fn set_statement_timeout(&self, limit: Option<std::time::Duration>) {
        let ms = limit.map_or(0, |d| {
            u64::try_from(d.as_millis()).unwrap_or(u64::MAX).max(1)
        });
        self.statement_timeout_ms.store(ms, Ordering::Relaxed);
    }

    /// Keep the conflict sets of the newest `commits` commits
    /// (`DREVO_TX_CONFLICT_HISTORY`, issue #749); see
    /// [`NativeGraph::set_conflict_history`].
    pub fn set_conflict_history(&self, commits: usize) {
        self.graph.set_conflict_history(commits);
    }

    /// The [`crate::native::Isolation`] of explicit transactions that do not
    /// ask for one (`DREVO_TX_ISOLATION`).
    pub fn set_isolation(&self, isolation: crate::native::Isolation) {
        self.graph.set_isolation(isolation);
    }

    /// Rejected commits by reason since this database was opened (issue
    /// #749); see [`NativeGraph::conflict_stats`].
    pub fn conflict_stats(&self) -> drevo_core::conflict::ConflictStats {
        self.graph.conflict_stats()
    }

    /// The statement timeout set by
    /// [`set_statement_timeout`](Self::set_statement_timeout), if any.
    pub fn statement_timeout(&self) -> Option<std::time::Duration> {
        match self.statement_timeout_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(std::time::Duration::from_millis(ms)),
        }
    }

    /// Compact the WAL and trim the consumed change-feed once enough ops
    /// have accumulated since the last compaction — the runtime bound on
    /// disk (log rewrite as the state snapshot; a no-op for an in-memory
    /// engine) and memory (feed history behind every index's cursor is
    /// dropped). Runs inline on the statement that crosses the threshold:
    /// the pause is one state rewrite, and only one caller compacts at a
    /// time. A failed compaction leaves the log valid and is retried at
    /// the next threshold crossing.
    /// Rewrite the WAL as the state snapshot; `true` on success. On wasm
    /// there is no WAL (the durable constructor is not compiled), so only
    /// the feed trim applies.
    #[cfg(not(target_arch = "wasm32"))]
    fn compact_wal_ok(&self) -> bool {
        self.graph.compact_wal().is_ok()
    }

    /// See the non-wasm variant — nothing on disk to rewrite here.
    #[cfg(target_arch = "wasm32")]
    fn compact_wal_ok(&self) -> bool {
        true
    }

    fn maybe_compact(&self) {
        let head = self.graph.change_head();
        if head.saturating_sub(self.last_compact_head.load(Ordering::SeqCst))
            < self.compact_every_ops
        {
            return;
        }
        if self.compacting.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.compact_wal_ok() {
            self.last_compact_head
                .store(self.graph.change_head(), Ordering::SeqCst);
            // Every index re-syncs to the head before serving, so history at
            // or before the synced cursor is consumed; trim it to bound the
            // feed. (A cursor below the new floor would only mean a lagged
            // rebuild — every index handles that — but by construction the
            // service's indexes never lag past their own synced head.)
            self.trim_feed();
        }
        self.compacting.store(false, Ordering::SeqCst);
    }

    /// Run `serve` with the index stack caught up to the change-feed head:
    /// the shared lock when already fresh (reads run concurrently), the
    /// exclusive lock to re-sync after a write. Running under the exclusive
    /// guard in the stale case is deliberate: it only happens for the first
    /// request after a write, and it keeps the sync + serve pair simple
    /// (std's `RwLock` cannot downgrade).
    fn with_fresh_indexes<R>(&self, serve: impl FnOnce(&ServiceIndexes) -> R) -> R {
        let fresh = |idx: &ServiceIndexes| {
            idx.synced_head == self.graph.change_head()
                && idx.paths_version == self.path_specs_version.load(Ordering::SeqCst)
                && idx.secondary == self.secondary_indexes()
        };
        // Bring the indexes up to date under the write lock, then serve under
        // the read lock, so a long statement never holds up the others. Under
        // a steady stream of writes the indexes may move on again between the
        // two; after a few rounds the statement is served under the write
        // lock it caught up with.
        for _ in 0..3 {
            {
                let idx = self.indexes.read().unwrap_or_else(|e| e.into_inner());
                if fresh(&idx) {
                    return serve(&idx);
                }
            }
            let mut idx = self.indexes.write().unwrap_or_else(|e| e.into_inner());
            self.catch_up_indexes(&mut idx);
        }
        let mut idx = self.indexes.write().unwrap_or_else(|e| e.into_inner());
        self.catch_up_indexes(&mut idx);
        serve(&idx)
    }

    /// Bring `idx` up to the graph's head and the current path and text
    /// index definitions.
    fn catch_up_indexes(&self, idx: &mut ServiceIndexes) {
        let version = self.path_specs_version.load(Ordering::SeqCst);
        if idx.paths_version != version {
            let (specs, text_specs) = {
                let reg = self.path_indexes.read().unwrap_or_else(|e| e.into_inner());
                (reg.specs(), reg.text_specs())
            };
            idx.paths.set_specs(specs, &self.graph);
            idx.texts.set_specs(text_specs, &self.graph);
            idx.paths_version = version;
        }
        idx.set_secondary(self.secondary_indexes(), &self.graph);
        if idx.synced_head != self.graph.change_head() {
            idx.catch_up(&self.graph);
        }
    }

    /// Whether this database keeps its secondary indexes: the property index,
    /// the value cache and the full-text indexes. On by default.
    #[must_use]
    pub fn secondary_indexes(&self) -> bool {
        self.secondary_wanted.load(Ordering::SeqCst)
    }

    /// Keep (`true`) or drop (`false`) this database's secondary indexes —
    /// the property-equality index, the value cache and the node and
    /// relationship full-text indexes — and remember the choice across
    /// restarts. Off, their memory is freed before the next statement runs:
    /// lookups by property scan instead, and `fts.*` /
    /// `db.index.fulltext.*` are unavailable. Labels, adjacency, ids and the
    /// declared path, text and vector indexes are unaffected. On again, they
    /// are rebuilt from the graph before the next statement.
    ///
    /// # Errors
    ///
    /// [`DrevoError::ReadOnly`] on a read-only database.
    pub fn set_secondary_indexes(&self, on: bool) -> Result<(), DrevoError> {
        if self.is_read_only() {
            return Err(DrevoError::ReadOnly);
        }
        self.secondary_wanted.store(on, Ordering::SeqCst);
        self.persist_semantic();
        Ok(())
    }

    /// Full-text search over the service's BM25 index — the engine of the
    /// `POST /search/fts` route, matching the KV store's response shape.
    pub fn search_fts(&self, query: &str, limit: usize) -> Vec<crate::model::ScoredNode> {
        self.with_fresh_indexes(|idx| {
            idx.fts
                .search(query, limit)
                .into_iter()
                .filter_map(|(id, score)| {
                    self.graph
                        .get_node_arc(id)
                        .map(|node| crate::model::ScoredNode {
                            node: (*node).clone(),
                            score,
                        })
                })
                .collect()
        })
    }

    /// The graph as the `drevo-json-v1` dump document — the engine of
    /// `GET /export/json`, matching the KV route's body.
    ///
    /// # Errors
    ///
    /// Propagates scan failures and non-serialisable property values.
    pub fn export_json(&self) -> Result<String, DrevoError> {
        use crate::engine::GraphEngine;
        let dump = GraphEngine::export_dump(&self.graph)?;
        serde_json::to_string_pretty(&dump)
            .map_err(|e| DrevoError::Io(std::io::Error::other(e.to_string())))
    }

    /// Keep the latest `changes` committed changes in the change feed, and as
    /// many named events, for event subscribers to resume from (default
    /// [`Self::DEFAULT_EVENT_HISTORY`]). Older history is dropped now and
    /// after each compaction.
    pub fn set_event_history(&self, changes: u64) {
        self.event_history.store(changes, Ordering::Relaxed);
        self.trim_feed();
    }

    /// Drop change-feed history older than the event history, never past
    /// what the indexes have consumed. An older cursor is told to reload
    /// (`GET /events`).
    fn trim_feed(&self) {
        let synced = self
            .indexes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .synced_head;
        let history = self.event_history.load(Ordering::Relaxed);
        let keep_from = self.graph.change_head().saturating_sub(history);
        self.graph.trim_before(synced.min(keep_from));
    }

    /// The position of the latest committed change in the change feed.
    #[must_use]
    pub fn change_head(&self) -> u64 {
        self.graph.change_head()
    }

    /// Every committed change after `cursor`, in commit order, with the
    /// cursor to resume from; `lagged` when history before it was trimmed.
    #[must_use]
    pub fn changes_since(&self, cursor: u64) -> crate::native::ChangeBatch {
        self.graph.changes_since(cursor)
    }

    /// The server's running statements and open transactions (see
    /// [`crate::transactions`]).
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn transactions(&self) -> std::sync::Arc<crate::transactions::Transactions> {
        std::sync::Arc::clone(&self.transactions.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Serve this database as `name`, listing its transactions in `shared`
    /// with the other databases of the server.
    #[cfg(all(not(target_arch = "wasm32"), feature = "http"))]
    pub(crate) fn join_server(
        &self,
        name: &str,
        shared: std::sync::Arc<crate::transactions::Transactions>,
    ) {
        *self
            .database_name
            .write()
            .unwrap_or_else(|e| e.into_inner()) = name.to_string();
        *self.transactions.write().unwrap_or_else(|e| e.into_inner()) = shared;
    }

    /// List a statement of this database while it runs.
    #[cfg(not(target_arch = "wasm32"))]
    fn list_statement(&self) -> crate::transactions::Listed {
        let name = self
            .database_name
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.transactions().list_new(&name, false)
    }

    /// Run `work` as a listed statement that `TERMINATE TRANSACTIONS` can
    /// stop.
    fn listed<R>(&self, work: impl FnOnce() -> R) -> R {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let listed = self.list_statement();
            crate::cypher::executor::deadline::with_cancel(
                Some(listed.entry().terminated_flag()),
                work,
            )
        }
        #[cfg(target_arch = "wasm32")]
        {
            work()
        }
    }

    /// Whether explicit transaction `tx` was terminated (and so must be
    /// rolled back rather than committed).
    #[must_use]
    pub fn tx_terminated(&self, tx: crate::native::NativeTxId) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.explicit_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&tx)
                .is_some_and(|l| l.entry().is_terminated())
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = tx;
            false
        }
    }

    /// Stop listing explicit transaction `tx`.
    fn unlist_tx(&self, tx: crate::native::NativeTxId) {
        #[cfg(not(target_arch = "wasm32"))]
        self.explicit_txs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&tx);
        #[cfg(target_arch = "wasm32")]
        let _ = tx;
    }

    /// Begin a registered transaction on the durable store — the session
    /// keeps the returned id between statements. See
    /// [`crate::native::NativeGraph::tx_begin`].
    pub fn begin_tx(&self) -> crate::native::NativeTxId {
        self.begin_tx_with(None)
    }

    /// [`Self::begin_tx`] at `isolation`, or at the database's default when
    /// `None` (see [`crate::native::Isolation`]).
    pub fn begin_tx_with(
        &self,
        isolation: Option<crate::native::Isolation>,
    ) -> crate::native::NativeTxId {
        let tx = match isolation {
            Some(isolation) => self.graph.tx_begin_with(isolation),
            None => self.graph.tx_begin(),
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let name = self
                .database_name
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let listed = self.transactions().list_new(&name, true);
            self.explicit_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(tx, listed);
        }
        tx
    }

    /// Execute one statement inside a registered transaction: the executor
    /// runs over the transaction's working copy (read-your-writes, invisible
    /// to concurrent statements). While nothing was committed since the
    /// transaction began, the label, property, path and text indexes narrow
    /// `MATCH` as in auto-commit mode, completed with the nodes the
    /// transaction wrote; after another writer's commit the statement scans.
    /// Search, vector, `SHOW INDEXES` and auto-embed work as in auto-commit
    /// mode; full-text search fails once the transaction has written, and
    /// schema commands fail with `ForbiddenInTransaction`.
    ///
    /// # Errors
    ///
    /// The executor's [`crate::cypher::executor::ExecError`]; a closed
    /// transaction surfaces as a storage error rather than a panic.
    pub fn execute_in_tx(
        &self,
        tx: crate::native::NativeTxId,
        query: &Query,
        params: HashMap<String, Value>,
    ) -> Result<ExecResult, ExecError> {
        self.execute_in_tx_as(tx, query, params, None)
    }

    /// [`execute_in_tx`](Self::execute_in_tx) for a signed-in user (#636).
    /// A refused write fails the statement; the transaction keeps what its
    /// earlier statements wrote, and the client rolls it back as usual.
    ///
    /// # Errors
    ///
    /// As [`execute_in_tx`](Self::execute_in_tx), and
    /// [`ExecError::Forbidden`].
    pub fn execute_in_tx_as(
        &self,
        tx: crate::native::NativeTxId,
        query: &Query,
        params: HashMap<String, Value>,
        restriction: Option<Restriction<'_>>,
    ) -> Result<ExecResult, ExecError> {
        #[cfg(feature = "http")]
        let watch = crate::problems::SlowWatch::start(&params);
        let result = self.execute_in_open_tx(tx, query, params, restriction);
        #[cfg(feature = "http")]
        if let Some(watch) = watch {
            watch.finish(&self.database_name(), true);
        }
        result
    }

    /// The body of [`execute_in_tx_as`](Self::execute_in_tx_as), without the
    /// slow-statement log.
    fn execute_in_open_tx(
        &self,
        tx: crate::native::NativeTxId,
        query: &Query,
        params: HashMap<String, Value>,
        restriction: Option<Restriction<'_>>,
    ) -> Result<ExecResult, ExecError> {
        let Some(engine) = self.graph.tx_engine(tx) else {
            return Err(ExecError::Storage(DrevoError::Io(std::io::Error::other(
                "the transaction has already been closed",
            ))));
        };
        // The label, property, path and text indexes describe the committed
        // graph, not this transaction's snapshot, so reads scan instead. The
        // full-text indexes have no scan fallback: they serve the statement
        // and their hits resolve against the snapshot, until the transaction
        // has written (`tx_has_written`).
        let tx_has_written = self.graph.tx_has_writes(tx);
        // A terminated transaction runs nothing more; its commit is refused.
        #[cfg(not(target_arch = "wasm32"))]
        let entry = self
            .explicit_txs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&tx)
            .map(|l| std::sync::Arc::clone(l.entry()));
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(entry) = &entry {
            if entry.is_terminated() {
                return Err(ExecError::Terminated);
            }
            entry.set_query(crate::transactions::current_query());
        }
        #[cfg(not(target_arch = "wasm32"))]
        struct Idle(Option<std::sync::Arc<crate::transactions::Entry>>);
        #[cfg(not(target_arch = "wasm32"))]
        impl Drop for Idle {
            fn drop(&mut self) {
                if let Some(entry) = &self.0 {
                    entry.set_query(None);
                }
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        let flag = entry.as_ref().map(|e| e.terminated_flag());
        #[cfg(not(target_arch = "wasm32"))]
        let idle = Idle(entry);
        #[cfg(target_arch = "wasm32")]
        let flag = None;
        let (result, events) = crate::event_bus::collect(|| {
            crate::cypher::executor::deadline::with_cancel(flag, || {
                crate::cypher::executor::deadline::with_statement_timeout(
                    self.statement_timeout(),
                    || {
                        self.with_fresh_indexes(|idx| {
                            // With nothing committed since the transaction
                            // began, the indexes describe its snapshot except
                            // for the nodes it wrote, which complete every
                            // index lookup.
                            let overlay = self.graph.tx_index_overlay(tx);
                            let indexed = overlay.is_some();
                            let ctx = NativeQueryContext {
                                fts: idx.fts(),
                                fts_rel: idx.fts_rel(),
                                labels: indexed.then_some(&idx.labels),
                                properties: idx.props().filter(|_| indexed),
                                paths: indexed.then_some(&idx.paths),
                                texts: indexed.then_some(&idx.texts),
                                values: idx.values(),
                                tx_written: overlay.as_ref(),
                                #[cfg(feature = "http")]
                                embedder: self.embedder.get(),
                                #[cfg(not(feature = "http"))]
                                embedder: None,
                                semantic: Some(self),
                                in_transaction: true,
                                tx_has_written,
                                restriction,
                            };
                            execute_on_engine_with_context(query, &engine, &ctx, params)
                        })
                    },
                )
            })
        });
        // A statement that took the transaction over its write limit rolls
        // it back, freeing what it buffered (#751).
        if matches!(
            result,
            Err(ExecError::Storage(DrevoError::TransactionTooLarge { .. }))
        ) {
            self.rollback_tx(tx);
            return result;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(entry) = &idle.0 {
            let writes = self.graph.tx_write_count(tx).unwrap_or(0);
            entry.set_buffered_writes(u64::try_from(writes).unwrap_or(u64::MAX));
        }
        // A failed statement's events go with it; the others wait for the
        // commit.
        if result.is_ok() && !events.is_empty() {
            self.tx_events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(tx)
                .or_default()
                .extend(events);
        }
        result
    }

    /// The named events explicit transaction `tx` posted, forgotten.
    fn take_tx_events(&self, tx: crate::native::NativeTxId) -> Vec<crate::event_bus::Pending> {
        self.tx_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&tx)
            .unwrap_or_default()
    }

    /// The named events of the transaction prepared under `gid`, forgotten.
    fn take_prepared_triggers(&self, gid: &str) -> Option<HashMap<String, Value>> {
        self.prepared_triggers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(gid)
    }

    /// Whether a writing statement must run as a transaction so triggers see
    /// its changes: some trigger is installed and running, and this is not a
    /// trigger's own statement.
    fn triggers_active(&self) -> bool {
        !crate::trigger::in_trigger()
            && self
                .triggers
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .has_active()
    }

    /// The running triggers of `phase`, in installation order.
    fn triggers_of(&self, phase: crate::trigger::TriggerPhase) -> Vec<crate::trigger::Trigger> {
        self.triggers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .active(phase)
            .cloned()
            .collect()
    }

    /// Run the `before` triggers in transaction `tx`, ahead of its commit,
    /// with the parameters describing what it changed. Returns those
    /// parameters for the `after` phases, or `None` when no trigger runs
    /// (none is installed, the transaction changed nothing, or this is a
    /// trigger's own transaction).
    ///
    /// # Errors
    ///
    /// The first trigger that fails; the caller rolls `tx` back.
    fn fire_before(
        &self,
        tx: crate::native::NativeTxId,
    ) -> Result<Option<HashMap<String, Value>>, crate::trigger::TriggerFailure> {
        if !self.triggers_active() {
            return Ok(None);
        }
        let Some(changes) = self.graph.tx_changes(tx) else {
            return Ok(None);
        };
        if changes.is_empty() {
            return Ok(None);
        }
        let params = crate::trigger::change_params(&changes);
        for trigger in self.triggers_of(crate::trigger::TriggerPhase::Before) {
            let Some(engine) = self.graph.tx_engine(tx) else {
                break;
            };
            crate::trigger::run(&trigger, &params, |query, params| {
                self.with_fresh_indexes(|idx| {
                    let ctx = NativeQueryContext {
                        fts: idx.fts(),
                        fts_rel: idx.fts_rel(),
                        labels: None,
                        properties: None,
                        paths: None,
                        texts: None,
                        values: None,
                        #[cfg(feature = "http")]
                        embedder: self.embedder.get(),
                        #[cfg(not(feature = "http"))]
                        embedder: None,
                        semantic: Some(self),
                        in_transaction: true,
                        tx_has_written: true,
                        tx_written: None,
                        restriction: None,
                    };
                    execute_on_engine_with_context(query, &engine, &ctx, params)
                })
            })?;
        }
        Ok(Some(params))
    }

    /// Run the `after` triggers for a committed transaction's change
    /// parameters, each as a transaction of its own, and hand the
    /// `afterAsync` ones to the trigger worker (or run them next when there
    /// is none). A failing trigger is logged; the committed transaction
    /// stands.
    fn fire_after(&self, params: HashMap<String, Value>) {
        for trigger in self.triggers_of(crate::trigger::TriggerPhase::After) {
            self.run_after_trigger(&trigger, &params);
        }
        let deferred = self.triggers_of(crate::trigger::TriggerPhase::AfterAsync);
        if deferred.is_empty() {
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(worker) = self.trigger_worker.get() {
            let job = AsyncTriggers {
                triggers: deferred,
                params,
            };
            let sent = worker.lock().unwrap_or_else(|e| e.into_inner()).send(job);
            match sent {
                Ok(()) => return,
                // The worker is gone: run them here instead.
                Err(std::sync::mpsc::SendError(job)) => {
                    for trigger in &job.triggers {
                        self.run_after_trigger(trigger, &job.params);
                    }
                    return;
                }
            }
        }
        for trigger in &deferred {
            self.run_after_trigger(trigger, &params);
        }
    }

    /// Run one `after` / `afterAsync` trigger as its own statement; its writes
    /// do not fire triggers.
    fn run_after_trigger(
        &self,
        trigger: &crate::trigger::Trigger,
        params: &HashMap<String, Value>,
    ) {
        let outcome =
            crate::trigger::run(trigger, params, |query, params| self.execute(query, params));
        if let Err(failure) = outcome {
            #[cfg(feature = "http")]
            tracing::warn!(trigger = %failure.name, error = %failure.message, "trigger failed after commit");
            #[cfg(not(feature = "http"))]
            let _ = failure;
        }
    }

    /// Run `afterAsync` triggers on a thread of their own from now on, so a
    /// commit does not wait for them. Without it they run right after the
    /// `after` triggers, on the committing thread. Calling it again does
    /// nothing; the thread ends when the service is dropped.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn start_trigger_worker(self: &std::sync::Arc<Self>) {
        let (tx, rx) = std::sync::mpsc::channel::<AsyncTriggers>();
        if self.trigger_worker.set(std::sync::Mutex::new(tx)).is_err() {
            return;
        }
        let service = std::sync::Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("drevo-triggers".to_string())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    let Some(service) = service.upgrade() else {
                        return;
                    };
                    for trigger in &job.triggers {
                        service.run_after_trigger(trigger, &job.params);
                    }
                }
            });
        if let Err(e) = spawned {
            #[cfg(feature = "http")]
            tracing::error!(error = %e, "cannot start the trigger worker; afterAsync triggers run inline");
            #[cfg(not(feature = "http"))]
            let _ = e;
        }
    }

    fn take_prepared_events(&self, gid: &str) -> Vec<crate::event_bus::Pending> {
        self.prepared_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(gid)
            .unwrap_or_default()
    }

    /// Commit a registered transaction (one fsynced WAL batch, atomic swap),
    /// then apply the runtime compaction policy — a committed batch counts
    /// toward the threshold like any other write.
    ///
    /// # Errors
    ///
    /// [`crate::native::CommitError`] — `Conflict` when another writer
    /// committed since the transaction began (retryable), `Constraint` on a
    /// schema violation, `Io` on a WAL failure or a closed transaction.
    pub fn commit_tx(
        &self,
        tx: crate::native::NativeTxId,
    ) -> std::result::Result<(), crate::native::CommitError> {
        if self.tx_terminated(tx) {
            self.rollback_tx(tx);
            return Err(crate::native::CommitError::Io(
                "the transaction has been terminated".to_string(),
            ));
        }
        let fired = match self.fire_before(tx) {
            Ok(fired) => fired,
            Err(failure) => {
                self.rollback_tx(tx);
                return Err(crate::native::CommitError::Rejected(failure.to_string()));
            }
        };
        let result = self.graph.tx_commit(tx);
        self.unlist_tx(tx);
        let events = self.take_tx_events(tx);
        if result.is_ok() {
            self.publish_events(events);
            if let Some(params) = fired {
                self.fire_after(params);
            }
            self.maybe_compact();
        }
        result
    }

    /// Discard a registered transaction. `false` when it was already closed.
    pub fn rollback_tx(&self, tx: crate::native::NativeTxId) -> bool {
        self.unlist_tx(tx);
        self.take_tx_events(tx);
        self.graph.tx_rollback(tx)
    }

    /// Two-phase commit, phase one (#556): validate registered transaction
    /// `tx`, durably log its write set as prepared under `gid`, and raise the
    /// write fence — see [`NativeGraph::tx_prepare`] and
    /// `docs/rfc-two-phase-commit.md`. The transaction is closed either way.
    ///
    /// # Errors
    ///
    /// [`crate::native::PrepareError`]; nothing is prepared.
    pub fn prepare_tx(
        &self,
        tx: crate::native::NativeTxId,
        gid: &str,
    ) -> std::result::Result<(), crate::native::PrepareError> {
        let fired = match self.fire_before(tx) {
            Ok(fired) => fired,
            Err(failure) => {
                self.rollback_tx(tx);
                return Err(crate::native::PrepareError::Rejected(failure.to_string()));
            }
        };
        self.unlist_tx(tx);
        let events = self.take_tx_events(tx);
        let result = self.graph.tx_prepare(tx, gid);
        if result.is_ok() && !events.is_empty() {
            self.prepared_events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(gid.to_string(), events);
        }
        if let (Ok(()), Some(params)) = (&result, fired) {
            self.prepared_triggers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(gid.to_string(), params);
        }
        result
    }

    /// Two-phase commit, phase two (#556): apply the write set prepared under
    /// `gid` (cannot conflict), then apply the runtime compaction policy like
    /// any other commit.
    ///
    /// # Errors
    ///
    /// [`crate::native::ResolveError`]: unknown `gid`, or an I/O failure that
    /// leaves the transaction prepared (retry).
    pub fn commit_prepared(
        &self,
        gid: &str,
    ) -> std::result::Result<(), crate::native::ResolveError> {
        let result = self.graph.commit_prepared(gid);
        if result.is_ok() {
            let events = self.take_prepared_events(gid);
            self.publish_events(events);
            if let Some(params) = self.take_prepared_triggers(gid) {
                self.fire_after(params);
            }
            self.maybe_compact();
        }
        result
    }

    /// Two-phase commit (#556): discard the write set prepared under `gid`.
    ///
    /// # Errors
    ///
    /// [`crate::native::ResolveError`], as for
    /// [`commit_prepared`](Self::commit_prepared).
    pub fn rollback_prepared(
        &self,
        gid: &str,
    ) -> std::result::Result<(), crate::native::ResolveError> {
        let result = self.graph.rollback_prepared(gid);
        if result.is_ok() {
            self.take_prepared_events(gid);
            self.take_prepared_triggers(gid);
        }
        result
    }

    /// Two-phase commit (#556): roll back the transaction prepared under
    /// `gid` **heuristically** — an operator or timeout decision rather than
    /// the coordinator's. A later `commit_prepared(gid)` then reports
    /// [`crate::native::ResolveError::HeuristicRollback`], so the coordinator
    /// learns its decision was overridden.
    ///
    /// # Errors
    ///
    /// [`crate::native::ResolveError`], as for
    /// [`rollback_prepared`](Self::rollback_prepared).
    pub fn heuristic_rollback_prepared(
        &self,
        gid: &str,
    ) -> std::result::Result<(), crate::native::ResolveError> {
        let result = self.graph.heuristic_rollback_prepared(gid);
        if result.is_ok() {
            self.take_prepared_events(gid);
            self.take_prepared_triggers(gid);
        }
        result
    }

    /// Every prepared, unresolved transaction (#556), by ascending `gid`.
    pub fn list_prepared(&self) -> Vec<crate::native::PreparedInfo> {
        self.graph.list_prepared()
    }

    /// One node by storage id — the engine of `GET /nodes/{id}`.
    ///
    /// # Errors
    ///
    /// [`crate::error::DrevoError::NodeNotFound`] when the id does not
    /// exist, exactly as the KV route reports it.
    pub fn get_node(&self, id: u64) -> Result<crate::model::Node, DrevoError> {
        self.graph
            .get_node_arc(id)
            .map(|n| (*n).clone())
            .ok_or(crate::error::DrevoError::NodeNotFound(id))
    }

    /// Look up a node by its globally-unique UUID — embedded-handle parity with
    /// `Drevo::get_node_by_uuid` (issue #445). `None` if no node carries it.
    pub fn get_node_by_uuid(&self, uuid: [u8; 16]) -> Option<crate::model::Node> {
        self.graph.get_node_by_uuid(uuid)
    }

    /// The oldest node (lowest id) with `title`, or `None`. Titles are not
    /// unique (#595); [`nodes_by_title`](Self::nodes_by_title) returns all.
    pub fn get_node_by_title(&self, title: &str) -> Option<crate::model::Node> {
        self.graph.get_node_by_title(title)
    }

    /// Every node with `title`, id-ascending.
    pub fn nodes_by_title(&self, title: &str) -> Vec<crate::model::Node> {
        self.graph.nodes_by_title(title)
    }

    /// Most-recently-updated nodes first, capped at `limit` — parity with
    /// `Drevo::list_recent` (`updated_at` desc, id desc).
    pub fn list_recent(&self, limit: usize) -> Vec<crate::model::Node> {
        self.graph.list_recent(limit)
    }

    /// Edges of `kind`, id-ascending, paginated — parity with
    /// `Drevo::list_edges_by_kind`.
    pub fn list_edges_by_kind(
        &self,
        kind: &str,
        limit: usize,
        offset: usize,
    ) -> Vec<crate::model::Edge> {
        self.graph.list_edges_by_kind(kind, limit, offset)
    }

    /// Create a node — the engine of `POST /nodes`, embedded-handle parity with
    /// `Drevo::create_node`. Returns the stored node with generated id, uuid and
    /// timestamps.
    ///
    /// # Errors
    /// Propagates a WAL/encode failure as [`DrevoError`].
    pub fn create_node(
        &self,
        mut new_node: crate::model::NewNode,
    ) -> Result<crate::model::Node, DrevoError> {
        use crate::engine::GraphEngine;
        // Server-side auto-embed on ingest (#447): embed the configured text
        // property before the write, so `drevo.semantic.query` retrieves the
        // node with no client round-trip. Fail-open, no-op without an embedder.
        let fields = NodeTextFields {
            title: &new_node.title,
            body: &new_node.body,
        };
        self.apply_auto_embeddings(&new_node.kind, &mut new_node.properties, fields, None);
        Ok(self.graph.create_node(new_node)?)
    }

    /// Create many nodes in one durable batch — parity with
    /// `Drevo::create_nodes`. Each node is passed through the server-side
    /// auto-embed step (#447) before the batch is written, so a registered
    /// label's text property is embedded on bulk ingest exactly as it is on the
    /// single-node [`create_node`](Self::create_node) path. Fail-open: without a
    /// configured embedder (or for an unregistered label) the node is stored
    /// unchanged.
    ///
    /// # Errors
    /// Propagates a duplicate-title validation error (the whole batch fails and
    /// nothing is written) or a WAL/encode failure as [`DrevoError`].
    pub fn create_nodes(
        &self,
        mut new_nodes: Vec<crate::model::NewNode>,
    ) -> Result<Vec<crate::model::Node>, DrevoError> {
        for nn in &mut new_nodes {
            let fields = NodeTextFields {
                title: &nn.title,
                body: &nn.body,
            };
            self.apply_auto_embeddings(&nn.kind, &mut nn.properties, fields, None);
        }
        Ok(self.graph.create_nodes(new_nodes)?)
    }

    /// Create an edge — the engine of `POST /edges`, parity with
    /// `Drevo::create_edge`. Returns the stored edge.
    ///
    /// # Errors
    /// [`DrevoError::NodeNotFound`] when either endpoint is absent; propagates a
    /// WAL/encode failure.
    pub fn create_edge(
        &self,
        mut new_edge: crate::model::NewEdge,
    ) -> Result<crate::model::Edge, DrevoError> {
        use crate::engine::GraphEngine;
        // Relationship auto-embed on ingest (#447), mirroring `create_node`.
        self.apply_auto_embeddings_edge(&new_edge.kind, &mut new_edge.properties, None);
        Ok(self.graph.create_edge(new_edge)?)
    }

    /// Apply a partial update to a node — embedded-handle parity with
    /// `Drevo::update_node`. Returns the updated node.
    ///
    /// # Errors
    /// [`DrevoError::NodeNotFound`] when the id does not exist; propagates a
    /// WAL/encode failure.
    pub fn update_node(
        &self,
        id: u64,
        patch: crate::model::NodePatch,
    ) -> Result<crate::model::Node, DrevoError> {
        use crate::engine::GraphEngine;
        Ok(self.graph.update_node(id, patch)?)
    }

    /// Delete a node (and its incident edges) — parity with `Drevo::delete_node`.
    ///
    /// # Errors
    /// [`DrevoError::NodeNotFound`] when the id does not exist; propagates a
    /// WAL/encode failure.
    pub fn delete_node(&self, id: u64) -> Result<(), DrevoError> {
        use crate::engine::GraphEngine;
        Ok(self.graph.delete_node(id)?)
    }

    /// Fetch an edge by id — parity with `Drevo::get_edge`. `None` if absent.
    ///
    /// # Errors
    /// Propagates a WAL/decode failure as [`DrevoError`].
    pub fn get_edge(&self, id: u64) -> Result<Option<crate::model::Edge>, DrevoError> {
        use crate::engine::GraphEngine;
        Ok(self.graph.get_edge(id)?)
    }

    /// Apply a partial update to an edge — parity with `Drevo::update_edge`.
    /// Returns the updated edge.
    ///
    /// # Errors
    /// [`DrevoError::EdgeNotFound`] when the id does not exist; propagates a
    /// WAL/encode failure.
    pub fn update_edge(
        &self,
        id: u64,
        patch: crate::model::EdgePatch,
    ) -> Result<crate::model::Edge, DrevoError> {
        use crate::engine::GraphEngine;
        Ok(self.graph.update_edge(id, patch)?)
    }

    /// Delete an edge — parity with `Drevo::delete_edge`.
    ///
    /// # Errors
    /// [`DrevoError::EdgeNotFound`] when the id does not exist; propagates a
    /// WAL/encode failure.
    pub fn delete_edge(&self, id: u64) -> Result<(), DrevoError> {
        use crate::engine::GraphEngine;
        Ok(self.graph.delete_edge(id)?)
    }

    /// Bounded subgraph around `root` within `depth` hops — parity with
    /// `Drevo::subgraph`. Convenience wrapper over
    /// [`subgraph_filtered`](Self::subgraph_filtered) with no edge-kind filter.
    ///
    /// # Errors
    /// [`DrevoError::NodeNotFound`] when the root does not exist.
    pub fn subgraph(&self, root: u64, depth: u8) -> Result<crate::model::SubGraph, DrevoError> {
        self.subgraph_filtered(root, depth, None)
    }

    /// Nodes of `kind`, id-ascending, paginated — parity with
    /// `Drevo::list_nodes_by_kind` (the engine of `GET /nodes?kind=`).
    pub fn list_nodes_by_kind(
        &self,
        kind: &str,
        limit: usize,
        offset: usize,
    ) -> Vec<crate::model::Node> {
        use crate::engine::GraphEngine;
        self.graph
            .nodes_by_kind(kind, limit, offset)
            .unwrap_or_default()
            .into_iter()
            .map(|n| (*n).clone())
            .collect()
    }

    /// Keyword facets over nodes of `kind` — the engine of `GET /facets` and the
    /// engine-agnostic counterpart of `Drevo::facets`. Keywords are
    /// scored with the native FTS corpus statistics (`doc_count` / `trigram_df`)
    /// rather than the KV backend, so the facet counts match what `fts.search`
    /// would rank; `build_facets`/`node_property_text` are shared with the KV
    /// path.
    ///
    /// # Errors
    /// Propagates a keyword-extraction failure as [`DrevoError`].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn facets(
        &self,
        kind: &str,
        property: &str,
        k: usize,
        collapse: &crate::fts::facet::FacetCollapse<'_>,
    ) -> Result<Vec<crate::fts::facet::Facet>, DrevoError> {
        let nodes = self.list_nodes_by_kind(kind, usize::MAX, 0);
        self.with_fresh_indexes(|idx| {
            let mut per_doc: Vec<(u64, Vec<String>)> = Vec::with_capacity(nodes.len());
            for node in &nodes {
                let Some(text) = crate::fts::facet::node_property_text(node, property) else {
                    continue;
                };
                let keywords = crate::fts::keywords::extract_keywords_scored(
                    &text,
                    k,
                    false,
                    idx.fts.doc_count(),
                    &|term_trigrams| Ok(idx.fts.trigram_df(term_trigrams)),
                )?;
                if !keywords.is_empty() {
                    per_doc.push((node.id, keywords));
                }
            }
            Ok(crate::fts::facet::build_facets(&per_doc, collapse))
        })
    }

    // ----- durable embedding store + HNSW (embedded parity, #446) ------------
    //
    // The native counterpart of the KV `vec:` store + `build_vector_index`.
    // Reads/writes delegate to the durable `NativeGraph` store; the HNSW index
    // is rebuilt on demand from that store via the shared, engine-agnostic
    // `vector_store::build_hnsw_from` (issue #446 S0).

    /// Store (or replace) a node's embedding, durably — parity with
    /// `Drevo::set_embedding`.
    ///
    /// # Errors
    /// [`DrevoError::NodeNotFound`] if the node is absent; propagates a WAL
    /// failure.
    pub fn set_embedding(&self, node_id: u64, embedding: Vec<f32>) -> Result<(), DrevoError> {
        Ok(self.graph.set_embedding(node_id, embedding)?)
    }

    /// Store many embeddings in one durable batch (one fsync), all-or-nothing —
    /// parity with `Drevo::set_embeddings_batch`.
    ///
    /// # Errors
    /// [`DrevoError::NodeNotFound`] for the first absent node (nothing written);
    /// propagates a WAL failure.
    pub fn set_embeddings_batch(&self, embeddings: &[(u64, Vec<f32>)]) -> Result<(), DrevoError> {
        Ok(self.graph.set_embeddings_batch(embeddings)?)
    }

    /// Fetch a node's embedding, or `None` — parity with `Drevo::get_embedding`.
    pub fn get_embedding(&self, node_id: u64) -> Option<Vec<f32>> {
        self.graph.get_embedding(node_id)
    }

    /// Delete a node's embedding (idempotent), durably — parity with
    /// `Drevo::delete_embedding`.
    ///
    /// # Errors
    /// Propagates a WAL failure.
    pub fn delete_embedding(&self, node_id: u64) -> Result<(), DrevoError> {
        Ok(self.graph.delete_embedding(node_id)?)
    }

    /// The number of stored embeddings — parity with `Drevo::embedding_count`.
    pub fn embedding_count(&self) -> usize {
        self.graph.embedding_count()
    }

    /// Liveness probe — parity with `Drevo::health_check`. Touches the store
    /// (node/edge counts) to prove the engine is readable; the durable engine
    /// recovers on open, so there is no separate integrity/recover step.
    ///
    /// # Errors
    /// Infallible today (reads cannot fail); returns `Result` to match the KV
    /// handle's signature so the embedded/Python surface is identical.
    pub fn health_check(&self) -> Result<(), DrevoError> {
        let _ = self.graph.node_count();
        let _ = self.graph.edge_count();
        Ok(())
    }

    // ── Semantic auto-embedding registry (issue #447) ────────────────────────
    //
    // The native home of the KV handle's `Drevo::semantic` / `rel_semantic`
    // control plane. The registry type (`SemanticIndexRegistry`) is pure and
    // reused verbatim; mutations persist to the `semantic.json` sidecar so a
    // registration survives a restart. The query/embed path already runs on
    // native; these are the register/status control-plane methods the executor
    // will resolve native-first (a later slice), replacing the KV `secondary`.

    /// Register (or re-enable) a node-side auto-embedding target — parity with
    /// `Drevo::semantic_register`.
    ///
    /// # Errors
    /// Propagates the registry transition error (e.g. already enabled).
    pub fn semantic_register(
        &self,
        label: &str,
        text_property: &str,
        embedding_property: &str,
        mode: crate::semantic_index::IndexMode,
        model: Option<String>,
    ) -> Result<crate::semantic_index::SemanticIndex, crate::semantic_index::IndexError> {
        let target = {
            let mut reg = self.semantic.write().unwrap_or_else(|e| e.into_inner());
            reg.enable(label, text_property, embedding_property, mode, model)
                .cloned()?
        };
        self.persist_semantic();
        Ok(target)
    }

    /// Register (or re-enable) a relationship-side target — parity with
    /// `Drevo::semantic_register_rel`.
    ///
    /// # Errors
    /// Propagates the registry transition error.
    pub fn semantic_register_rel(
        &self,
        rel_type: &str,
        text_property: &str,
        embedding_property: &str,
        mode: crate::semantic_index::IndexMode,
        model: Option<String>,
    ) -> Result<crate::semantic_index::SemanticIndex, crate::semantic_index::IndexError> {
        let target = {
            let mut reg = self.rel_semantic.write().unwrap_or_else(|e| e.into_inner());
            reg.enable(rel_type, text_property, embedding_property, mode, model)
                .cloned()?
        };
        self.persist_semantic();
        Ok(target)
    }

    // ----- named vector indexes (issue #532) ---------------------------------

    /// Register a named vector index (`CREATE VECTOR INDEX <name> FOR (n:label)
    /// ON (n.property)`, or over relationships of type `label` for
    /// [`crate::vector_index_registry::VectorIndexEntity::Relationship`]),
    /// persisting it to the sidecar. `if_not_exists` makes a name clash a no-op
    /// (the `IF NOT EXISTS` form).
    ///
    /// # Errors
    /// [`crate::vector_index_registry::VectorIndexError::AlreadyExists`] if the
    /// name is taken and `if_not_exists` is false.
    pub fn vector_index_create(
        &self,
        name: &str,
        label: &str,
        property: &str,
        entity: crate::vector_index_registry::VectorIndexEntity,
        if_not_exists: bool,
    ) -> Result<
        crate::vector_index_registry::VectorIndex,
        crate::vector_index_registry::VectorIndexError,
    > {
        let index = {
            let mut reg = self
                .vector_indexes
                .write()
                .unwrap_or_else(|e| e.into_inner());
            // Index names are unique across index kinds, like Neo4j.
            let path_taken = self
                .path_indexes
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(name)
                .is_some()
                || self
                    .constraints
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(name)
                    .is_some();
            if path_taken {
                return Err(
                    crate::vector_index_registry::VectorIndexError::AlreadyExists(name.to_string()),
                );
            }
            reg.create(
                name.to_string(),
                label.to_string(),
                property.to_string(),
                entity,
                if_not_exists,
            )?
        };
        self.persist_semantic();
        Ok(index)
    }

    /// Drop a named vector index (`DROP INDEX <name>`), persisting the change.
    /// `if_exists` makes a missing name a no-op.
    ///
    /// # Errors
    /// [`crate::vector_index_registry::VectorIndexError::NotFound`] if the name
    /// is unknown and `if_exists` is false.
    pub fn vector_index_drop(
        &self,
        name: &str,
        if_exists: bool,
    ) -> Result<(), crate::vector_index_registry::VectorIndexError> {
        {
            let mut reg = self
                .vector_indexes
                .write()
                .unwrap_or_else(|e| e.into_inner());
            reg.remove(name, if_exists)?;
        }
        self.persist_semantic();
        Ok(())
    }

    /// The `(label, property)` a named vector index targets, if registered —
    /// how `db.index.vector.queryNodes(<name>, …)` resolves its scan.
    #[must_use]
    pub fn vector_index_get(
        &self,
        name: &str,
    ) -> Option<crate::vector_index_registry::VectorIndex> {
        self.vector_indexes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// Every registered vector index, in creation order (for `SHOW INDEXES`).
    #[must_use]
    pub fn vector_index_list(&self) -> Vec<crate::vector_index_registry::VectorIndex> {
        self.vector_indexes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .list()
            .to_vec()
    }

    // ----- nested-path and text indexes (issues #578, #589) -------------------

    /// Register a path index (`CREATE INDEX [<name>] FOR (n[:label]) ON
    /// (n.a.b, …)`) or, with `kind` = `Text`, a text index (`CREATE TEXT
    /// INDEX … ON (n.prop)`), persisting it. A missing `name` is generated from
    /// the kind, label and paths. `if_not_exists` makes a taken name a no-op.
    /// The index is built before the next statement runs.
    ///
    /// # Errors
    /// [`crate::path_index_registry::IndexError::AlreadyExists`] if the name is
    /// taken by any index and `if_not_exists` is false.
    pub fn path_index_create(
        &self,
        name: Option<&str>,
        kind: crate::path_index_registry::IndexKind,
        label: Option<&str>,
        paths: Vec<crate::native_path_index::PropertyPath>,
        if_not_exists: bool,
    ) -> Result<(), crate::path_index_registry::IndexError> {
        use crate::path_index_registry::{generated_name, IndexError, PathIndex};
        let name = name.map_or_else(|| generated_name(kind, label, &paths), str::to_string);
        {
            let vectors = self
                .vector_indexes
                .read()
                .unwrap_or_else(|e| e.into_inner());
            let mut reg = self.path_indexes.write().unwrap_or_else(|e| e.into_inner());
            let constraint_taken = self
                .constraints
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&name)
                .is_some();
            if reg.get(&name).is_some() || vectors.get(&name).is_some() || constraint_taken {
                return if if_not_exists {
                    Ok(())
                } else {
                    Err(IndexError::AlreadyExists(name))
                };
            }
            reg.insert(PathIndex {
                name,
                kind,
                label: label.map(str::to_string),
                paths,
            });
        }
        self.path_specs_version.fetch_add(1, Ordering::SeqCst);
        self.persist_semantic();
        Ok(())
    }

    /// Every registered path and text index, in creation order (for `SHOW
    /// INDEXES`).
    #[must_use]
    pub fn path_index_list(&self) -> Vec<crate::path_index_registry::PathIndex> {
        self.path_indexes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .list()
            .to_vec()
    }

    /// Drop the path, text or vector index named `name` (`DROP INDEX <name>`),
    /// persisting the change. `if_exists` makes a missing name a no-op.
    ///
    /// # Errors
    /// [`crate::path_index_registry::IndexError::NotFound`] if no index has
    /// that name and `if_exists` is false.
    pub fn index_drop(
        &self,
        name: &str,
        if_exists: bool,
    ) -> Result<(), crate::path_index_registry::IndexError> {
        let removed_path = self
            .path_indexes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        if removed_path {
            self.path_specs_version.fetch_add(1, Ordering::SeqCst);
            self.persist_semantic();
            return Ok(());
        }
        if self.vector_index_get(name).is_some() {
            return self
                .vector_index_drop(name, true)
                .map_err(|_| crate::path_index_registry::IndexError::NotFound(name.to_string()));
        }
        if if_exists {
            Ok(())
        } else {
            Err(crate::path_index_registry::IndexError::NotFound(
                name.to_string(),
            ))
        }
    }

    // ----- Node constraints (issues #604, #606) ---------------------------------

    /// Declare a node constraint (`CREATE CONSTRAINT [<name>] FOR (n:label)
    /// REQUIRE …`), persisting it. A missing `name` is generated from the
    /// label, properties and kind. `if_not_exists` makes a taken name or an
    /// equivalent constraint a no-op.
    ///
    /// # Errors
    /// [`ConstraintError::AlreadyExists`](crate::constraint_registry::ConstraintError::AlreadyExists)
    /// if the name is taken by a constraint or an index,
    /// [`Equivalent`](crate::constraint_registry::ConstraintError::Equivalent)
    /// if the same rule exists under another name,
    /// [`Violated`](crate::constraint_registry::ConstraintError::Violated) if
    /// existing data breaks it.
    pub fn constraint_create(
        &self,
        name: Option<&str>,
        label: &str,
        kind: crate::constraint_registry::ConstraintKind,
        properties: &[String],
        if_not_exists: bool,
    ) -> Result<(), crate::constraint_registry::ConstraintError> {
        use crate::constraint_registry::{generated_name, ConstraintError, SchemaConstraint};
        let name = name.map_or_else(|| generated_name(label, kind, properties), str::to_string);
        {
            let mut reg = self.constraints.write().unwrap_or_else(|e| e.into_inner());
            let index_taken = self
                .path_indexes
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&name)
                .is_some()
                || self.vector_index_get(&name).is_some();
            if reg.get(&name).is_some() || index_taken {
                return if if_not_exists {
                    Ok(())
                } else {
                    Err(ConstraintError::AlreadyExists(name))
                };
            }
            let constraint = SchemaConstraint {
                name,
                label: label.to_string(),
                kind,
                properties: properties.to_vec(),
            };
            if let Some(existing) = reg.equivalent(&constraint) {
                return if if_not_exists {
                    Ok(())
                } else {
                    Err(ConstraintError::Equivalent(existing.name.clone()))
                };
            }
            self.graph
                .add_constraint(constraint.constraint())
                .map_err(|e| ConstraintError::Violated(e.to_string()))?;
            reg.insert(constraint);
        }
        self.persist_semantic();
        Ok(())
    }

    /// Drop the constraint named `name` (`DROP CONSTRAINT <name>`), persisting
    /// the change. `if_exists` makes a missing name a no-op.
    ///
    /// # Errors
    /// [`ConstraintError::NotFound`](crate::constraint_registry::ConstraintError::NotFound)
    /// if no constraint has that name and `if_exists` is false.
    pub fn constraint_drop(
        &self,
        name: &str,
        if_exists: bool,
    ) -> Result<(), crate::constraint_registry::ConstraintError> {
        let removed = self
            .constraints
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        match removed {
            Some(c) => {
                self.graph.remove_constraint(&c.constraint());
                self.persist_semantic();
                Ok(())
            }
            None if if_exists => Ok(()),
            None => Err(crate::constraint_registry::ConstraintError::NotFound(
                name.to_string(),
            )),
        }
    }

    /// Install `trigger`, replacing an installed trigger of the same name, and
    /// persist it. The statement is parsed first, so a trigger that could
    /// never run is refused.
    ///
    /// # Errors
    ///
    /// [`DrevoError::InvalidArgument`] for an empty name or a statement that
    /// does not parse.
    pub fn trigger_install(
        &self,
        trigger: crate::trigger::Trigger,
    ) -> Result<crate::trigger::Trigger, DrevoError> {
        if trigger.name.trim().is_empty() {
            return Err(DrevoError::InvalidArgument(
                "a trigger needs a name".to_string(),
            ));
        }
        crate::cypher::parser::parse(&trigger.statement).map_err(|e| {
            DrevoError::InvalidArgument(format!(
                "the statement of trigger `{}` does not parse: {e}",
                trigger.name
            ))
        })?;
        self.triggers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .install(trigger.clone());
        self.persist_semantic();
        Ok(trigger)
    }

    /// Remove the trigger named `name`, returning it; `None` if there is none.
    pub fn trigger_drop(&self, name: &str) -> Option<crate::trigger::Trigger> {
        let removed = self
            .triggers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        if removed.is_some() {
            self.persist_semantic();
        }
        removed
    }

    /// Remove every trigger, returning them in installation order.
    pub fn trigger_drop_all(&self) -> Vec<crate::trigger::Trigger> {
        let removed = self
            .triggers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        if !removed.is_empty() {
            self.persist_semantic();
        }
        removed
    }

    /// Stop (`paused = true`) or start the trigger named `name`, returning it
    /// as updated; `None` if there is none.
    pub fn trigger_set_paused(&self, name: &str, paused: bool) -> Option<crate::trigger::Trigger> {
        let updated = self
            .triggers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .set_paused(name, paused)
            .cloned();
        if updated.is_some() {
            self.persist_semantic();
        }
        updated
    }

    /// The name this database is served under: the name the server gave it,
    /// or `drevo` for a database opened on its own.
    #[must_use]
    pub fn database_name(&self) -> String {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.database_name
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
        #[cfg(target_arch = "wasm32")]
        {
            DEFAULT_DATABASE_NAME.to_string()
        }
    }

    /// Every installed trigger, in installation order.
    #[must_use]
    pub fn trigger_list(&self) -> Vec<crate::trigger::Trigger> {
        self.triggers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .list()
            .to_vec()
    }

    /// Every declared constraint, in creation order (for `SHOW CONSTRAINTS`).
    #[must_use]
    pub fn constraint_list(&self) -> Vec<crate::constraint_registry::SchemaConstraint> {
        self.constraints
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .list()
            .to_vec()
    }

    /// The registered node-side targets — parity with `Drevo::semantic_status`.
    pub fn semantic_status(&self) -> Vec<crate::semantic_index::SemanticIndex> {
        self.semantic
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .list()
            .to_vec()
    }

    /// The registered relationship-side targets.
    pub fn semantic_status_rel(&self) -> Vec<crate::semantic_index::SemanticIndex> {
        self.rel_semantic
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .list()
            .to_vec()
    }

    /// Detailed status for `drevo.semantic.status` — parity with
    /// `Drevo::semantic_status_detailed` (#263/#266, ported to native in #447).
    ///
    /// For each Auto target it scans the live backlog (`pending` = matching
    /// nodes/edges that carry the text property but no embedding) and reads the
    /// cumulative swallowed-failure tally (`failed` / `last_error`) recorded by
    /// the fail-open auto-embed write path. `degraded` is `pending > 0`, so a
    /// client can tell "fully embedded" from "writes landed, embeddings
    /// missing". Manual targets have no drevo-managed backlog, so they always
    /// read clean.
    pub fn semantic_status_detailed(&self) -> Vec<crate::report::SemanticTargetStatus> {
        let mut out = Vec::new();
        for index in self.semantic_status() {
            out.push(self.status_for("node", index));
        }
        for index in self.semantic_status_rel() {
            out.push(self.status_for("relationship", index));
        }
        out
    }

    /// Build the health row for one target (#447; native mirror of
    /// `Drevo::status_for`). Only Auto targets are drevo-managed, so only they
    /// have a backlog.
    fn status_for(
        &self,
        kind: &'static str,
        index: crate::semantic_index::SemanticIndex,
    ) -> crate::report::SemanticTargetStatus {
        let pending = if matches!(index.mode, crate::semantic_index::IndexMode::Auto) {
            match kind {
                "relationship" => self.semantic_pending_count_rel(
                    &index.label,
                    &index.text_property,
                    &index.embedding_property,
                ),
                _ => self.semantic_pending_count(
                    &index.label,
                    &index.text_property,
                    &index.embedding_property,
                ),
            }
        } else {
            0
        };
        let (failed, last_error) =
            self.embed_failure_stat(kind, &index.label, &index.embedding_property);
        crate::report::SemanticTargetStatus {
            target_kind: kind,
            index,
            pending,
            failed,
            last_error,
            degraded: pending > 0,
        }
    }

    /// Count nodes of `label` that carry a non-empty `text_property` but still
    /// lack `embedding_property` — the live auto-embed backlog (#447; mirror of
    /// `Drevo::semantic_pending_count`). Scans a consistent snapshot.
    fn semantic_pending_count(
        &self,
        label: &str,
        text_property: &str,
        embedding_property: &str,
    ) -> usize {
        let mut pending = 0;
        for node in self.graph.snapshot().all_nodes() {
            let matches_label = node.kind == label
                || matches!(
                    node.properties.get("_labels"),
                    Some(serde_json::Value::Array(arr))
                        if arr.iter().any(|v| v.as_str() == Some(label))
                );
            if !matches_label {
                continue;
            }
            let has_text =
                node_text(&node.properties, NodeTextFields::of(&node), text_property).is_some();
            let has_embedding = node.properties.contains_key(embedding_property);
            if has_text && !has_embedding {
                pending += 1;
            }
        }
        pending
    }

    /// Relationship mirror of [`Self::semantic_pending_count`] (#447): edges of
    /// `rel_type` with a non-empty `text_property` but no `embedding_property`.
    fn semantic_pending_count_rel(
        &self,
        rel_type: &str,
        text_property: &str,
        embedding_property: &str,
    ) -> usize {
        let mut pending = 0;
        for edge in self.graph.snapshot().all_edges() {
            if edge.kind != rel_type {
                continue;
            }
            let has_text = edge
                .properties
                .get(text_property)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| !s.is_empty());
            let has_embedding = edge.properties.contains_key(embedding_property);
            if has_text && !has_embedding {
                pending += 1;
            }
        }
        pending
    }

    /// Record a swallowed auto-embed failure for a target (#447; mirror of
    /// `Drevo::record_embed_failure`), keyed by `kind` so a node label and a
    /// relationship type of the same name never collide.
    #[cfg(feature = "http")]
    fn record_embed_failure(&self, kind: &str, name: &str, embedding_property: &str, error: &str) {
        let mut map = self
            .embed_failures
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let stat = map
            .entry((
                kind.to_string(),
                name.to_string(),
                embedding_property.to_string(),
            ))
            .or_default();
        stat.count += 1;
        stat.last_error = error.to_string();
    }

    /// Read the cumulative failure count and most-recent error for a target
    /// (#447). `(0, None)` when nothing has failed — and always so on a build
    /// without `http` (no embedder to fail).
    fn embed_failure_stat(
        &self,
        kind: &str,
        name: &str,
        embedding_property: &str,
    ) -> (u64, Option<String>) {
        #[cfg(feature = "http")]
        {
            let map = self
                .embed_failures
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match map.get(&(
                kind.to_string(),
                name.to_string(),
                embedding_property.to_string(),
            )) {
                Some(stat) if stat.count > 0 => (stat.count, Some(stat.last_error.clone())),
                _ => (0, None),
            }
        }
        #[cfg(not(feature = "http"))]
        {
            let _ = (kind, name, embedding_property);
            (0, None)
        }
    }

    /// Apply server-side auto-embedding to a **node's** properties just before
    /// it is persisted (#447; native mirror of `Drevo::apply_auto_embeddings`).
    ///
    /// For every registered [`crate::semantic_index::IndexMode::Auto`] target
    /// whose label matches this node (primary `kind` or a `_labels` secondary
    /// label), embed the text in the target's `text_property` (see
    /// `node_text`, which also reads the node's `title` / `body` fields) and
    /// write the vector into its `embedding_property`. A double no-op keeps the
    /// common path free: it returns immediately when no embedder is installed
    /// and when no Auto target matches. `old` is the pre-patch property map and
    /// text fields on update; a target whose source text is unchanged and whose
    /// embedding is already present is skipped. An upstream failure is logged,
    /// tallied, and swallowed — a transient embedder outage must never fail a
    /// write.
    #[cfg(feature = "http")]
    pub fn apply_auto_embeddings(
        &self,
        kind: &str,
        properties: &mut crate::model::Properties,
        fields: NodeTextFields<'_>,
        old: Option<(&crate::model::Properties, NodeTextFields<'_>)>,
    ) {
        let Some(embedder) = self.embedder.get() else {
            return;
        };
        let targets: Vec<(String, String, String)> = {
            let reg = self.semantic.read().unwrap_or_else(|e| e.into_inner());
            reg.list()
                .iter()
                .filter(|t| matches!(t.mode, crate::semantic_index::IndexMode::Auto))
                .map(|t| {
                    (
                        t.label.clone(),
                        t.text_property.clone(),
                        t.embedding_property.clone(),
                    )
                })
                .collect()
        };
        if targets.is_empty() {
            return;
        }
        let mut labels = vec![kind.to_string()];
        if let Some(serde_json::Value::Array(arr)) = properties.get("_labels") {
            for item in arr {
                if let serde_json::Value::String(s) = item {
                    if !labels.iter().any(|l| l == s) {
                        labels.push(s.clone());
                    }
                }
            }
        }
        for (label, text_prop, emb_prop) in targets {
            if !labels.iter().any(|l| l == &label) {
                continue;
            }
            let Some(text) = node_text(properties, fields, &text_prop).map(str::to_string) else {
                continue;
            };
            if let Some((old_props, old_fields)) = old {
                let unchanged = node_text(old_props, old_fields, &text_prop) == Some(text.as_str());
                if unchanged && properties.contains_key(&emb_prop) {
                    continue;
                }
            }
            match embedder.embed_query(&text) {
                Ok(vector) => {
                    let arr = serde_json::Value::Array(
                        vector.into_iter().map(|f| serde_json::json!(f)).collect(),
                    );
                    properties.insert(emb_prop, arr);
                }
                Err(error) => {
                    tracing::warn!(label = %label, %error, "native auto-embed failed (ignored)");
                    self.record_embed_failure("node", &label, &emb_prop, &error.to_string());
                }
            }
        }
    }

    /// No-op stand-in on a build without `http` (no embedder is compiled in).
    #[cfg(not(feature = "http"))]
    pub fn apply_auto_embeddings(
        &self,
        kind: &str,
        properties: &mut crate::model::Properties,
        fields: NodeTextFields<'_>,
        old: Option<(&crate::model::Properties, NodeTextFields<'_>)>,
    ) {
        let _ = (kind, properties, fields, old);
    }

    /// Relationship mirror of [`Self::apply_auto_embeddings`] (#447): apply
    /// auto-embedding to an **edge's** properties before it is persisted.
    /// `rel_type` is the edge's `kind`; matching Auto-mode targets in the
    /// relationship registry embed `text_property` into `embedding_property`.
    #[cfg(feature = "http")]
    pub fn apply_auto_embeddings_edge(
        &self,
        rel_type: &str,
        properties: &mut crate::model::Properties,
        old: Option<&crate::model::Properties>,
    ) {
        let Some(embedder) = self.embedder.get() else {
            return;
        };
        let targets: Vec<(String, String, String)> = {
            let reg = self.rel_semantic.read().unwrap_or_else(|e| e.into_inner());
            reg.list()
                .iter()
                .filter(|t| {
                    matches!(t.mode, crate::semantic_index::IndexMode::Auto) && t.label == rel_type
                })
                .map(|t| {
                    (
                        t.label.clone(),
                        t.text_property.clone(),
                        t.embedding_property.clone(),
                    )
                })
                .collect()
        };
        for (rel, text_prop, emb_prop) in targets {
            let Some(text) = properties
                .get(&text_prop)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            if let Some(old) = old {
                let unchanged =
                    old.get(&text_prop).and_then(serde_json::Value::as_str) == Some(text.as_str());
                if unchanged && properties.contains_key(&emb_prop) {
                    continue;
                }
            }
            match embedder.embed_query(&text) {
                Ok(vector) => {
                    let arr = serde_json::Value::Array(
                        vector.into_iter().map(|f| serde_json::json!(f)).collect(),
                    );
                    properties.insert(emb_prop, arr);
                }
                Err(error) => {
                    tracing::warn!(rel_type = %rel, %error, "native auto-embed (rel) failed (ignored)");
                    self.record_embed_failure("relationship", &rel, &emb_prop, &error.to_string());
                }
            }
        }
    }

    /// No-op stand-in on a build without `http`.
    #[cfg(not(feature = "http"))]
    pub fn apply_auto_embeddings_edge(
        &self,
        rel_type: &str,
        properties: &mut crate::model::Properties,
        old: Option<&crate::model::Properties>,
    ) {
        let _ = (rel_type, properties, old);
    }

    /// Backfill embeddings for existing nodes of `label` — parity with
    /// `Drevo::semantic_reindex` (#262). Scans the label's nodes, embeds
    /// `text_property` into `embedding_property` (a node property) for up to
    /// `batch_size` that still lack it, and returns resumable counts. Idempotent
    /// (already-embedded nodes are skipped); fail-open (an embed failure leaves
    /// the node for a later pass). Needs the server-side embedder, so it is
    /// gated on `http`.
    ///
    /// # Errors
    /// Propagates a node-update failure from the graph.
    #[cfg(feature = "http")]
    pub fn semantic_reindex(
        &self,
        label: &str,
        text_property: &str,
        embedding_property: &str,
        batch_size: usize,
    ) -> Result<crate::report::SemanticReindexReport, DrevoError> {
        use crate::engine::GraphEngine;
        let mut report = crate::report::SemanticReindexReport::default();
        let mut budget = batch_size;
        // One consistent snapshot to scan; writes below mutate the live graph.
        for node in self.graph.snapshot().all_nodes() {
            // Match the primary kind plus any secondary `_labels` (same as KV).
            let matches_label = node.kind == label
                || matches!(
                    node.properties.get("_labels"),
                    Some(serde_json::Value::Array(arr))
                        if arr.iter().any(|v| v.as_str() == Some(label))
                );
            if !matches_label {
                continue;
            }
            report.scanned += 1;
            let text = node_text(&node.properties, NodeTextFields::of(&node), text_property)
                .map(str::to_string);
            let already_embedded = node.properties.contains_key(embedding_property);
            let Some(text) = text.filter(|_| !already_embedded) else {
                report.skipped += 1;
                continue;
            };
            if budget == 0 {
                report.remaining += 1;
                continue;
            }
            let Some(embedder) = self.embedder.get() else {
                report.remaining += 1;
                continue;
            };
            match embedder.embed_query(&text) {
                Ok(vector) => {
                    let mut props = node.properties.clone();
                    props.insert(
                        embedding_property,
                        serde_json::Value::Array(
                            vector.into_iter().map(|f| serde_json::json!(f)).collect(),
                        ),
                    );
                    let patch = crate::model::NodePatch {
                        properties: Some(props),
                        ..Default::default()
                    };
                    self.graph.update_node(node.id, patch)?;
                    report.embedded += 1;
                    budget -= 1;
                }
                Err(error) => {
                    tracing::warn!(label, error = %error, "native reindex embed failed (ignored)");
                    report.remaining += 1;
                }
            }
        }
        Ok(report)
    }

    /// Backfill embeddings for existing **edges** of `rel_type` — the edge
    /// mirror of [`Self::semantic_reindex`], parity with
    /// `Drevo::semantic_reindex_rel` (#266). Same idempotent / resumable /
    /// fail-open semantics.
    ///
    /// # Errors
    /// Propagates an edge-update failure from the graph.
    #[cfg(feature = "http")]
    pub fn semantic_reindex_rel(
        &self,
        rel_type: &str,
        text_property: &str,
        embedding_property: &str,
        batch_size: usize,
    ) -> Result<crate::report::SemanticReindexReport, DrevoError> {
        use crate::engine::GraphEngine;
        let mut report = crate::report::SemanticReindexReport::default();
        let mut budget = batch_size;
        for edge in self.graph.snapshot().all_edges() {
            if edge.kind != rel_type {
                continue;
            }
            report.scanned += 1;
            let text = edge
                .properties
                .get(text_property)
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let already_embedded = edge.properties.contains_key(embedding_property);
            let Some(text) = text.filter(|_| !already_embedded) else {
                report.skipped += 1;
                continue;
            };
            if budget == 0 {
                report.remaining += 1;
                continue;
            }
            let Some(embedder) = self.embedder.get() else {
                report.remaining += 1;
                continue;
            };
            match embedder.embed_query(&text) {
                Ok(vector) => {
                    let mut props = edge.properties.clone();
                    props.insert(
                        embedding_property,
                        serde_json::Value::Array(
                            vector.into_iter().map(|f| serde_json::json!(f)).collect(),
                        ),
                    );
                    let patch = crate::model::EdgePatch {
                        properties: Some(props),
                        ..Default::default()
                    };
                    self.graph.update_edge(edge.id, patch)?;
                    report.embedded += 1;
                    budget -= 1;
                }
                Err(error) => {
                    tracing::warn!(rel_type, error = %error, "native reindex (rel) embed failed (ignored)");
                    report.remaining += 1;
                }
            }
        }
        Ok(report)
    }

    /// Atomically rewrite the `semantic.json` sidecar with both registries.
    /// Best-effort (a storage hiccup must not fail a control-plane call); a
    /// no-op for an in-memory service. Mirrors the KV `persist_semantic_registry`.
    fn persist_semantic(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(path) = self.semantic_sidecar.as_ref() else {
                return;
            };
            let Ok(bytes) = self.semantic_sidecar_bytes() else {
                return;
            };
            let _ = write_sidecar(path, SEMANTIC_CONTEXT, &bytes);
        }
    }

    /// The `semantic.json` sidecar's contents for the registries as they are
    /// now.
    #[cfg(not(target_arch = "wasm32"))]
    fn semantic_sidecar_bytes(&self) -> serde_json::Result<Vec<u8>> {
        let sidecar = SemanticSidecar {
            node: self
                .semantic
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            rel: self
                .rel_semantic
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            vector: self
                .vector_indexes
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            path: self
                .path_indexes
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            constraints: self
                .constraints
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            triggers: self
                .triggers
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            secondary_indexes: self.secondary_indexes(),
        };
        serde_json::to_vec(&sidecar)
    }

    /// Back up this database into `dir` while it keeps serving: the engine's
    /// log and sidecars, `semantic.json` (the semantic, vector and path index
    /// registrations and the constraints) and `extra_files`. With `since`,
    /// only what was logged after that backup. See [`drevo_core::backup`].
    ///
    /// # Errors
    ///
    /// [`drevo_core::backup::BackupError`].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn backup_to(
        &self,
        dir: &std::path::Path,
        since: Option<drevo_core::backup::BackupManifest>,
        mut extra_files: Vec<(String, Vec<u8>)>,
    ) -> Result<drevo_core::backup::BackupManifest, drevo_core::backup::BackupError> {
        if self.semantic_sidecar.is_some() {
            let bytes = self
                .semantic_sidecar_bytes()
                .map_err(|e| drevo_core::backup::BackupError::Io(std::io::Error::other(e)))?;
            // As the file is stored: sealed for an encrypted store.
            let bytes = sidecar_bytes(SEMANTIC_CONTEXT, bytes)?;
            extra_files.push(("semantic.json".to_string(), bytes));
        }
        self.graph.backup_to(
            dir,
            &drevo_core::backup::BackupOptions { since, extra_files },
        )
    }

    /// Rebuild an in-memory HNSW index over every stored embedding — parity
    /// with `Drevo::build_vector_index`. The index is not persisted (the
    /// embeddings are); rebuild after reopen.
    ///
    /// # Errors
    /// [`DrevoError::Vector`] if a stored embedding cannot be inserted (e.g. a
    /// dimension mismatch against the first).
    pub fn build_vector_index(
        &self,
        config: crate::vector::HnswConfig,
    ) -> Result<crate::vector::HnswIndex, DrevoError> {
        crate::vector::store::build_hnsw_from(
            self.graph
                .all_embeddings()
                .into_iter()
                .map(|(id, v)| (id, crate::vector::Vector::from(v))),
            config,
        )
    }

    /// k-nearest embeddings to `query` by the default (cosine) metric —
    /// `(node_id, distance)`, nearest first. Parity with the Python handle's
    /// `vector_search`: rebuilds the HNSW index, then searches.
    ///
    /// # Errors
    /// [`DrevoError::Vector`] on a dimension mismatch (query vs stored vectors).
    pub fn vector_search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>, DrevoError> {
        let index = self.build_vector_index(crate::vector::HnswConfig::default())?;
        Ok(index
            .search(query, k)?
            .into_iter()
            .map(|n| (n.key, n.distance))
            .collect())
    }

    // ----- traversal (embedded parity with the KV `Drevo` handle, #445) ------
    //
    // These reuse the same engine-agnostic `crate::traversal` algorithms the KV
    // handle does, driven by closures over the native graph — so BFS/DFS visit
    // order, depth semantics (start node excluded), and Dijkstra weighting are
    // identical on both engines. Native reads are infallible, hence the `Ok(..)`
    // wrap into the traversal closures' `Result` contract.

    /// Breadth-first search from `start_id` (start excluded), depth-capped —
    /// parity with `Drevo::bfs`.
    pub fn bfs(
        &self,
        start_id: u64,
        max_depth: u8,
        direction: crate::model::Direction,
        edge_kind: Option<&str>,
    ) -> Result<Vec<crate::model::Node>, DrevoError> {
        let snap = self.graph.snapshot();
        crate::traversal::bfs(
            start_id,
            max_depth,
            direction,
            edge_kind,
            &|id| Ok(snap.get_node(id)),
            &|id, dir| Ok(snap.edges_of(id, dir)),
        )
    }

    /// Depth-first search from `start_id` (start excluded), depth-capped —
    /// parity with `Drevo::dfs`.
    pub fn dfs(
        &self,
        start_id: u64,
        max_depth: u8,
        direction: crate::model::Direction,
        edge_kind: Option<&str>,
    ) -> Result<Vec<crate::model::Node>, DrevoError> {
        let snap = self.graph.snapshot();
        crate::traversal::dfs(
            start_id,
            max_depth,
            direction,
            edge_kind,
            &|id| Ok(snap.get_node(id)),
            &|id, dir| Ok(snap.edges_of(id, dir)),
        )
    }

    /// Lowest-total-weight path `from → to` over outgoing edges (Dijkstra) —
    /// parity with `Drevo::shortest_path`. `None` if unreachable.
    pub fn shortest_path(&self, from: u64, to: u64) -> Result<Option<Vec<u64>>, DrevoError> {
        self.shortest_path_filtered(from, to, None)
    }

    /// [`Self::shortest_path`] restricted to edges of `edge_kind` when `Some` —
    /// parity with `Drevo::shortest_path_filtered`.
    pub fn shortest_path_filtered(
        &self,
        from: u64,
        to: u64,
        edge_kind: Option<&str>,
    ) -> Result<Option<Vec<u64>>, DrevoError> {
        let snap = self.graph.snapshot();
        crate::traversal::shortest_path(
            from,
            to,
            edge_kind,
            &|id| Ok(snap.get_node(id)),
            &|id, dir| Ok(snap.edges_of(id, dir)),
        )
    }

    /// The lowest-cost path `from → to` under `options`, read from one
    /// snapshot: A* when `options.heuristic` is set, a bidirectional Dijkstra
    /// otherwise. `None` when either node is missing, `to` is unreachable, or
    /// every path costs more than `options.max_cost`.
    ///
    /// # Errors
    ///
    /// [`DrevoError::InvalidArgument`] for out-of-range options or a
    /// relationship whose cost is negative or not a number.
    pub fn weighted_path(
        &self,
        from: u64,
        to: u64,
        options: &crate::weighted_path::WeightedPathOptions,
    ) -> Result<Option<crate::weighted_path::WeightedPath>, DrevoError> {
        let snap = self.graph.snapshot();
        crate::weighted_path::weighted_path(
            from,
            to,
            options,
            &|id| Ok(snap.get_node_arc(id)),
            &|id, dir, f| {
                snap.for_each_edge_of(id, dir, f);
                Ok(())
            },
        )
    }

    /// Public-transport journeys from any of `starts` to any of `ends` over
    /// the timetable in the graph — see [`crate::timetable`].
    pub fn timetable(
        &self,
        starts: &[crate::timetable::Endpoint],
        ends: &[crate::timetable::Endpoint],
        options: &crate::timetable::TimetableOptions,
    ) -> Result<Vec<crate::timetable::Journey>, DrevoError> {
        let snap = self.graph.snapshot();
        crate::timetable::earliest_arrivals(starts, ends, options, &|id, dir, f| {
            snap.for_each_edge_of(id, dir, f);
            Ok(())
        })
    }

    /// The connected subgraph within `depth` hops of `root` — parity with
    /// `Drevo::subgraph_filtered`.
    pub fn subgraph_filtered(
        &self,
        root: u64,
        depth: u8,
        edge_kind: Option<&str>,
    ) -> Result<crate::model::SubGraph, DrevoError> {
        let snap = self.graph.snapshot();
        crate::traversal::subgraph(
            root,
            depth,
            edge_kind,
            &|id| Ok(snap.get_node(id)),
            &|id, dir| Ok(snap.edges_of(id, dir)),
        )
    }

    /// Immediate neighbours of `node_id` (BFS depth 1) — parity with
    /// `Drevo::neighbors`; served straight from the native adjacency index.
    pub fn neighbors(
        &self,
        node_id: u64,
        direction: crate::model::Direction,
        kind: Option<&str>,
    ) -> Vec<crate::model::Node> {
        self.graph.snapshot().neighbors(node_id, direction, kind)
    }

    /// Edges incident to `node_id` in `direction` — the engine of
    /// `GET /nodes/{id}/edges`, parity with `Drevo::edges_of`. A missing node
    /// yields an empty list (the KV route does the same — it does not 404).
    pub fn edges_of(
        &self,
        node_id: u64,
        direction: crate::model::Direction,
    ) -> Vec<crate::model::Edge> {
        self.graph.snapshot().edges_of(node_id, direction)
    }

    /// Import a `drevo-json-v1` dump into the durable store — the engine of
    /// `POST /import/json`, parity with `Drevo::import_json`. Parses and
    /// validates the format header, then replays the records through the WAL
    /// (durable, idempotent for drevo's own exports).
    ///
    /// # Errors
    /// [`DrevoError::Io`] on malformed JSON or an unknown format; propagates a
    /// title collision / WAL failure.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn import_json(&self, raw: &str) -> Result<crate::dump::ImportReport, DrevoError> {
        use crate::engine::GraphEngine;
        let dump: crate::dump::Dump = serde_json::from_str(raw)
            .map_err(|e| DrevoError::Io(std::io::Error::other(e.to_string())))?;
        if dump.format != crate::dump::FORMAT_V1 {
            return Err(DrevoError::Io(std::io::Error::other(format!(
                "unsupported dump format: {:?} — expected {}",
                dump.format,
                crate::dump::FORMAT_V1
            ))));
        }
        Ok(self.graph.apply_dump(dump)?)
    }

    fn execute_with(
        &self,
        idx: &ServiceIndexes,
        query: &Query,
        params: HashMap<String, Value>,
        restriction: Option<Restriction<'_>>,
    ) -> Result<ExecResult, ExecError> {
        let ctx = NativeQueryContext {
            fts: idx.fts(),
            fts_rel: idx.fts_rel(),
            labels: Some(&idx.labels),
            properties: idx.props(),
            paths: Some(&idx.paths),
            texts: Some(&idx.texts),
            values: idx.values(),
            #[cfg(feature = "http")]
            embedder: self.embedder.get(),
            #[cfg(not(feature = "http"))]
            embedder: None,
            // The native semantic control plane — `semantic.register/status`
            // manage this service's registry instead of a KV secondary (#447).
            semantic: Some(self),
            in_transaction: false,
            tx_has_written: false,
            tx_written: None,
            restriction,
        };
        execute_on_engine_with_context(query, &self.graph, &ctx, params)
    }
    /// Export the whole graph as a GraphML 1.0 document — the same wire
    /// format (and renderer) as the KV server's `GET /export/graphml`, so a
    /// durable-native backup is interchangeable with a KV one.
    ///
    /// # Errors
    ///
    /// Propagates [`crate::error::DrevoError`] from the scan or a
    /// non-serialisable property value.
    pub fn export_graphml(&self) -> Result<String, DrevoError> {
        use crate::engine::GraphEngine;
        let nodes: Vec<crate::model::Node> = GraphEngine::all_nodes(&self.graph)?
            .iter()
            .map(|n| (**n).clone())
            .collect();
        let edges = GraphEngine::all_edges(&self.graph)?;
        crate::dump::render_graphml(&nodes, &edges)
    }

    /// Import a GraphML document (drevo's own export, or any GraphML
    /// following the same conventions) into the durable graph — the
    /// engine-generic inverse of [`Self::export_graphml`]. Applied through
    /// the engine's dump cycle, so everything lands in the WAL as one
    /// fsynced atomic batch and the indexes catch up on the next statement.
    /// Re-importing an own export is idempotent (identical rows are
    /// skipped).
    ///
    /// # Errors
    ///
    /// Propagates parse failures ([`crate::dump::DumpError`] lifted into
    /// [`crate::error::DrevoError`]) and id-collision conflicts, exactly as
    /// the KV import reports them.
    pub fn import_graphml(&self, xml: &str) -> Result<crate::dump::ImportReport, DrevoError> {
        use crate::engine::GraphEngine;
        let all = GraphEngine::all_nodes(&self.graph)?;
        let db_max_node = all.iter().map(|n| n.id).max().unwrap_or(0);
        let db_max_edge = GraphEngine::all_edges(&self.graph)?
            .iter()
            .map(|e| e.id)
            .max()
            .unwrap_or(0);
        let (nodes, edges) = crate::dump::graphml_records(xml, db_max_node, db_max_edge)?;
        let next_node_id = nodes.iter().map(|n| n.id).max().map_or(1, |m| m + 1);
        let next_edge_id = edges.iter().map(|e| e.id).max().map_or(1, |m| m + 1);
        let report = self.graph.apply_dump(crate::dump::Dump {
            format: crate::dump::FORMAT_V1.to_string(),
            exported_at: crate::model::now_ms(),
            next_node_id,
            next_edge_id,
            nodes,
            edges,
        })?;
        Ok(report)
    }
}

/// Whether `query` has a clause that writes data (`CREATE`, `MERGE`, `SET`,
/// `REMOVE`, `DELETE`, `FOREACH`) in any `UNION` arm.
/// Whether `query` calls a procedure that writes the graph (`drevo.memory.*`).
fn query_calls_writing_procedure(query: &Query) -> bool {
    use crate::cypher::ast::Clause;
    query.parts.iter().any(|part| {
        part.query.clauses.iter().any(|c| {
            matches!(c, Clause::Call(call) if matches!(
                call.name.join(".").as_str(),
                "drevo.memory.addMessage"
                    | "drevo.memory.recordReasoning"
                    | "drevo.memory.rememberEntity"
                    | "drevo.memory.assertFact"
                    | "drevo.memory.retractFact"
            ))
        })
    })
}

/// Whether `query` creates or drops an index or constraint, registers a
/// semantic index or changes a trigger: schema kept in `semantic.json`,
/// beside the log.
fn query_changes_schema(query: &Query) -> bool {
    use crate::cypher::ast::Clause;
    query.parts.iter().any(|part| {
        part.query.clauses.iter().any(|c| match c {
            Clause::CreateVectorIndex(_)
            | Clause::CreatePathIndex(_)
            | Clause::DropIndex(_)
            | Clause::CreateConstraint(_)
            | Clause::DropConstraint(_) => true,
            Clause::Call(call) => {
                let name = call.name.join(".");
                matches!(
                    name.as_str(),
                    "drevo.semantic.register" | "drevo.semantic.registerRel"
                ) || (name.starts_with("apoc.trigger.") && name != "apoc.trigger.list")
                    || matches!(
                        name.as_str(),
                        "drevo.sequence.create" | "drevo.sequence.drop"
                    )
                    || name == "drevo.indexes.setSecondary"
            }
            _ => false,
        })
    })
}

fn query_writes(query: &Query) -> bool {
    use crate::cypher::ast::Clause;
    query.parts.iter().any(|part| {
        part.query.clauses.iter().any(|c| {
            matches!(
                c,
                Clause::Create(_)
                    | Clause::Merge(_)
                    | Clause::Set(_)
                    | Clause::Remove(_)
                    | Clause::Delete(_)
                    | Clause::Foreach(_)
            )
        })
    })
}

#[cfg(test)]
mod traversal_parity_tests {
    //! Embedded-parity traversal on native (#445): the BFS/DFS/shortest-path/
    //! subgraph/neighbours wrappers drive the same `crate::traversal` algorithms
    //! the KV `Drevo` handle uses, over a native snapshot.
    use super::NativeService;
    use crate::engine::GraphEngine;
    use crate::model::{Direction, NewEdge, NewNode, Properties};

    fn nn(title: &str) -> NewNode {
        NewNode {
            kind: "n".into(),
            title: title.into(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        }
    }
    fn ne(from_id: u64, to_id: u64, kind: &str) -> NewEdge {
        NewEdge {
            from_id,
            to_id,
            kind: kind.into(),
            weight: 1.0,
            properties: Properties::default(),
        }
    }
    fn titles(nodes: &[crate::model::Node]) -> Vec<String> {
        let mut t: Vec<String> = nodes.iter().map(|n| n.title.clone()).collect();
        t.sort();
        t
    }

    /// a -link-> b -link-> c ; a -other-> d
    fn fixture() -> (NativeService, u64, u64, u64, u64) {
        let svc = NativeService::in_memory();
        let g = svc.graph();
        let a = g.create_node(nn("a")).unwrap().id;
        let b = g.create_node(nn("b")).unwrap().id;
        let c = g.create_node(nn("c")).unwrap().id;
        let d = g.create_node(nn("d")).unwrap().id;
        g.create_edge(ne(a, b, "link")).unwrap();
        g.create_edge(ne(b, c, "link")).unwrap();
        g.create_edge(ne(a, d, "other")).unwrap();
        (svc, a, b, c, d)
    }

    #[test]
    fn bfs_depth_and_kind_filter() {
        let (svc, a, _b, _c, _d) = fixture();
        // depth 2, all kinds → b, c, d (start excluded)
        let all = svc.bfs(a, 2, Direction::Outgoing, None).unwrap();
        assert_eq!(titles(&all), vec!["b", "c", "d"]);
        // kind-filtered to "link" → only the b, c chain
        let links = svc.bfs(a, 2, Direction::Outgoing, Some("link")).unwrap();
        assert_eq!(titles(&links), vec!["b", "c"]);
        // depth 1 stops at direct neighbours
        let d1 = svc.bfs(a, 1, Direction::Outgoing, Some("link")).unwrap();
        assert_eq!(titles(&d1), vec!["b"]);
    }

    #[test]
    fn dfs_reaches_the_same_set() {
        let (svc, a, _b, _c, _d) = fixture();
        let out = svc.dfs(a, 2, Direction::Outgoing, Some("link")).unwrap();
        assert_eq!(titles(&out), vec!["b", "c"]);
    }

    #[test]
    fn shortest_path_follows_edges() {
        let (svc, a, b, c, d) = fixture();
        // a -link-> b -link-> c  → path [a, b, c]
        assert_eq!(svc.shortest_path(a, c).unwrap(), Some(vec![a, b, c]));
        // within the "link" subgraph, d (only reachable via "other") is unreachable
        assert_eq!(
            svc.shortest_path_filtered(a, d, Some("link")).unwrap(),
            None
        );
    }

    #[test]
    fn neighbors_and_subgraph() {
        let (svc, a, _b, _c, _d) = fixture();
        let neigh = svc.neighbors(a, Direction::Outgoing, None);
        assert_eq!(titles(&neigh), vec!["b", "d"]);
        let only_link = svc.neighbors(a, Direction::Outgoing, Some("link"));
        assert_eq!(titles(&only_link), vec!["b"]);
        // subgraph within 2 hops over "link": a, b, c
        let sg = svc.subgraph_filtered(a, 2, Some("link")).unwrap();
        assert_eq!(titles(&sg.nodes), vec!["a", "b", "c"]);
    }
}

#[cfg(test)]
mod crud_parity_tests {
    //! The inherent CRUD wrappers (`update_node`/`delete_node`/`get_edge`/
    //! `update_edge`/`delete_edge`/`subgraph`) added for embedded-handle parity
    //! with `Drevo` (epic #444) — the surface the C-FFI and WASM bindings drive.
    use super::NativeService;
    use crate::error::DrevoError;
    use crate::model::{EdgePatch, NewEdge, NewNode, NodePatch, Properties};

    fn nn(title: &str) -> NewNode {
        NewNode {
            kind: "n".into(),
            title: title.into(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        }
    }

    #[test]
    fn update_node_changes_fields_and_reports_missing() {
        let svc = NativeService::in_memory();
        let id = svc.create_node(nn("orig")).unwrap().id;
        let patch = NodePatch {
            title: Some("renamed".into()),
            ..Default::default()
        };
        let updated = svc.update_node(id, patch).unwrap();
        assert_eq!(updated.title, "renamed");
        assert_eq!(svc.get_node(id).unwrap().title, "renamed");
        // A patch against a missing id surfaces NodeNotFound.
        let miss = svc.update_node(9999, NodePatch::default());
        assert!(matches!(miss, Err(DrevoError::NodeNotFound(9999))));
    }

    #[test]
    fn delete_node_removes_it() {
        let svc = NativeService::in_memory();
        let id = svc.create_node(nn("gone")).unwrap().id;
        svc.delete_node(id).unwrap();
        assert!(matches!(svc.get_node(id), Err(DrevoError::NodeNotFound(_))));
        // Deleting a missing node reports NodeNotFound.
        assert!(matches!(
            svc.delete_node(id),
            Err(DrevoError::NodeNotFound(_))
        ));
    }

    #[test]
    fn edge_get_update_delete_roundtrip() {
        let svc = NativeService::in_memory();
        let a = svc.create_node(nn("a")).unwrap().id;
        let b = svc.create_node(nn("b")).unwrap().id;
        let e = svc
            .create_edge(NewEdge {
                from_id: a,
                to_id: b,
                kind: "link".into(),
                weight: 1.0,
                properties: Properties::default(),
            })
            .unwrap();
        // get_edge returns the stored edge; a missing id is Ok(None).
        assert_eq!(svc.get_edge(e.id).unwrap().unwrap().kind, "link");
        assert!(svc.get_edge(9999).unwrap().is_none());
        // update_edge applies a patch.
        let updated = svc
            .update_edge(
                e.id,
                EdgePatch {
                    weight: Some(2.5),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.weight, 2.5);
        // delete_edge removes it; a second delete reports EdgeNotFound.
        svc.delete_edge(e.id).unwrap();
        assert!(svc.get_edge(e.id).unwrap().is_none());
        assert!(matches!(
            svc.delete_edge(e.id),
            Err(DrevoError::EdgeNotFound(_))
        ));
    }

    #[test]
    fn subgraph_extracts_the_local_neighbourhood() {
        let svc = NativeService::in_memory();
        let a = svc.create_node(nn("a")).unwrap().id;
        let b = svc.create_node(nn("b")).unwrap().id;
        let c = svc.create_node(nn("c")).unwrap().id;
        svc.create_edge(NewEdge {
            from_id: a,
            to_id: b,
            kind: "link".into(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .unwrap();
        svc.create_edge(NewEdge {
            from_id: b,
            to_id: c,
            kind: "link".into(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .unwrap();
        // depth 1 from a reaches b (and the a→b edge), not c.
        let sub = svc.subgraph(a, 1).unwrap();
        let ids: std::collections::BTreeSet<u64> = sub.nodes.iter().map(|n| n.id).collect();
        assert!(ids.contains(&a) && ids.contains(&b));
        assert!(!ids.contains(&c), "depth-1 subgraph must not reach c");
    }
}

#[cfg(test)]
mod embedding_store_tests {
    //! Native embedding store + HNSW on the service layer (#446): set/get/
    //! delete/count/batch and `build_vector_index`/`vector_search`.
    use super::NativeService;
    use crate::engine::GraphEngine;
    use crate::model::{NewNode, Properties};

    fn nn(title: &str) -> NewNode {
        NewNode {
            kind: "doc".into(),
            title: title.into(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        }
    }

    #[test]
    fn store_roundtrip_and_node_validation() {
        let svc = NativeService::in_memory();
        let a = svc.graph().create_node(nn("a")).unwrap().id;

        svc.set_embedding(a, vec![1.0, 0.0, 0.0]).unwrap();
        assert_eq!(svc.embedding_count(), 1);
        assert_eq!(svc.get_embedding(a), Some(vec![1.0, 0.0, 0.0]));
        svc.delete_embedding(a).unwrap();
        assert_eq!(svc.get_embedding(a), None);

        // set on a missing node is an error surfaced as DrevoError.
        assert!(svc.set_embedding(999, vec![1.0]).is_err());
    }

    #[test]
    fn vector_search_finds_nearest() {
        let svc = NativeService::in_memory();
        let a = svc.graph().create_node(nn("a")).unwrap().id;
        let b = svc.graph().create_node(nn("b")).unwrap().id;
        let c = svc.graph().create_node(nn("c")).unwrap().id;
        svc.set_embeddings_batch(&[
            (a, vec![1.0, 0.0, 0.0]),
            (b, vec![0.0, 1.0, 0.0]),
            (c, vec![0.0, 0.0, 1.0]),
        ])
        .unwrap();
        assert_eq!(svc.embedding_count(), 3);

        let hits = svc.vector_search(&[0.9, 0.1, 0.0], 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, a, "nearest to [0.9,0.1,0] is a=[1,0,0]");
    }
}

#[cfg(test)]
mod health_and_batch_tests {
    //! Service-level `health_check` + batch create (#446 S3 prerequisite).
    use super::NativeService;
    use crate::model::{NewNode, Properties};

    fn nn(title: &str) -> NewNode {
        NewNode {
            kind: "doc".into(),
            title: title.into(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        }
    }

    #[test]
    fn health_check_ok_on_a_live_service() {
        let svc = NativeService::in_memory();
        svc.health_check().unwrap();
        svc.graph().create_nodes(vec![nn("a"), nn("b")]).unwrap();
        svc.health_check().unwrap();
    }

    #[test]
    fn create_nodes_stores_the_whole_batch() {
        // The service-level batch create (auto-embed-aware parity with
        // `Drevo::create_nodes`) writes every node and hands back the stored
        // rows with generated ids. Without a configured embedder the auto-embed
        // step is a fail-open no-op, so this exercises the storage path itself.
        let svc = NativeService::in_memory();
        let stored = svc.create_nodes(vec![nn("a"), nn("b"), nn("c")]).unwrap();
        assert_eq!(stored.len(), 3);
        assert!(stored.iter().all(|n| n.id != 0), "ids are assigned");
        assert!(svc.get_node_by_title("a").is_some());
        assert!(svc.get_node_by_title("c").is_some());
    }

    #[test]
    fn create_nodes_accepts_repeated_titles() {
        // Titles are not unique (#595): a batch repeating an existing title
        // writes every node.
        let svc = NativeService::in_memory();
        svc.create_node(nn("dup")).unwrap();
        svc.create_nodes(vec![nn("fresh"), nn("dup")]).unwrap();
        assert!(svc.get_node_by_title("fresh").is_some());
        assert_eq!(svc.nodes_by_title("dup").len(), 2);
    }
}

#[cfg(test)]
mod semantic_registry_tests {
    //! Native semantic registry (#447): register/status + sidecar durability.
    use super::NativeService;
    use crate::semantic_index::IndexMode;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn tmp_wal() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "drevo_sem_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("native.wal")
    }

    #[test]
    fn register_and_status_in_memory() {
        let svc = NativeService::in_memory();
        assert!(svc.semantic_status().is_empty());
        svc.semantic_register("Doc", "text", "embedding", IndexMode::Manual, None)
            .unwrap();
        svc.semantic_register_rel("MENTIONS", "note", "vec", IndexMode::Auto, None)
            .unwrap();
        let node = svc.semantic_status();
        assert_eq!(node.len(), 1);
        assert_eq!(node[0].label, "Doc");
        assert_eq!(svc.semantic_status_rel().len(), 1);
        // In-memory has no sidecar, so this is a no-op (must not panic).
    }

    #[test]
    fn registration_survives_reopen_via_sidecar() {
        let wal = tmp_wal();
        {
            let svc = NativeService::open(&wal).unwrap();
            svc.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
                .unwrap();
        }
        // A fresh service over the same WAL dir reloads the registry from
        // semantic.json next to the WAL.
        let svc = NativeService::open(&wal).unwrap();
        let targets = svc.semantic_status();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "Doc");
        assert_eq!(targets[0].mode, IndexMode::Auto);
        let _ = std::fs::remove_dir_all(wal.parent().unwrap());
    }
}

#[cfg(test)]
mod node_text_tests {
    //! [`super::node_text`] (#536): the text a semantic target reads from a node.
    use super::{node_text, NodeTextFields, SYNTHETIC_TITLE_PREFIX};
    use crate::model::Properties;
    use std::collections::HashMap;

    fn props(pairs: &[(&str, serde_json::Value)]) -> Properties {
        Properties::from(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<HashMap<_, _>>(),
        )
    }

    const FIELDS: NodeTextFields<'static> = NodeTextFields {
        title: "A title",
        body: "Some body",
    };

    #[test]
    fn reads_a_string_property() {
        let p = props(&[("summary", serde_json::json!("hello"))]);
        assert_eq!(node_text(&p, FIELDS, "summary"), Some("hello"));
        assert_eq!(node_text(&p, FIELDS, "missing"), None);
    }

    #[test]
    fn falls_back_to_the_title_and_body_fields() {
        let p = props(&[]);
        assert_eq!(node_text(&p, FIELDS, "title"), Some("A title"));
        assert_eq!(node_text(&p, FIELDS, "body"), Some("Some body"));
    }

    #[test]
    fn a_property_of_the_same_name_wins_over_the_field() {
        let p = props(&[("body", serde_json::json!("from props"))]);
        assert_eq!(node_text(&p, FIELDS, "body"), Some("from props"));
    }

    #[test]
    fn empty_non_string_and_synthetic_text_reads_as_none() {
        let p = props(&[("n", serde_json::json!(3)), ("e", serde_json::json!(""))]);
        assert_eq!(node_text(&p, FIELDS, "n"), None);
        assert_eq!(node_text(&p, FIELDS, "e"), None);
        let synthetic = format!("{SYNTHETIC_TITLE_PREFIX}Doc:0190");
        let fields = NodeTextFields {
            title: &synthetic,
            body: "",
        };
        assert_eq!(node_text(&props(&[]), fields, "title"), None);
        assert_eq!(node_text(&props(&[]), fields, "body"), None);
    }
}
