//! An embeddable property-graph engine: nodes and edges with JSON properties,
//! held in memory, with optional write-ahead-log durability, ACID
//! transactions, and secondary indexes that follow the graph through a change
//! feed.
//!
//! This is the engine underneath [drevo](https://github.com/ice1x/drevo), which
//! adds Cypher, the HTTP and Bolt servers, and the Python bindings on top. Use
//! `drevo-core` on its own when you want the graph inside your process and
//! nothing else.
//!
//! # Example
//!
//! ```
//! use drevo_core::engine::GraphEngine;
//! use drevo_core::model::{Direction, NewEdge, NewNode, Properties};
//! use drevo_core::native::NativeGraph;
//! use drevo_core::native_fts::NativeFtsIndex;
//!
//! fn task(title: &str) -> NewNode {
//!     NewNode {
//!         kind: "task".into(),
//!         title: title.into(),
//!         body: String::new(),
//!         body_html: String::new(),
//!         properties: Properties::default(),
//!     }
//! }
//!
//! let graph = NativeGraph::new(); // or NativeGraph::open_durable("graph.wal")?
//! let deploy = graph.create_node(task("Deploy release"))?;
//! let review = graph.create_node(task("Review release notes"))?;
//! graph.create_edge(NewEdge {
//!     from_id: deploy.id,
//!     to_id: review.id,
//!     kind: "blocked_by".into(),
//!     weight: 1.0,
//!     properties: Properties::default(),
//! })?;
//!
//! let blockers = graph.neighbors(deploy.id, Direction::Outgoing, Some("blocked_by"))?;
//! assert_eq!(blockers[0].title, "Review release notes");
//!
//! // Indexes catch up with the graph by reading its change feed.
//! let mut fts = NativeFtsIndex::new();
//! fts.sync(&graph);
//! assert_eq!(fts.search("deploy", 1)[0].0, deploy.id);
//! # Ok::<(), drevo_core::error::CoreError>(())
//! ```
//!
//! # Where to start
//!
//! - [`native::NativeGraph`] — the engine: create, read, update and delete
//!   nodes and edges, traverse, take snapshots, run transactions
//!   ([`begin`](native::NativeGraph::begin)) and two-phase commits.
//! - [`engine::GraphEngine`] — the trait that operations are written against.
//! - [`model`] — [`Node`](model::Node), [`Edge`](model::Edge) and their
//!   create/patch inputs.
//! - Indexes: [`native_fts`] (BM25 full-text search),
//!   [`native_label_index`], [`native_property_index`], and opt-in indexes on
//!   nested property paths, [`native_path_index`], and on substrings,
//!   [`native_text_index`].
//! - Text: [`tokenizer`] and [`levenshtein`] edit distance.
//! - Replication: [`replica`], [`delta`], [`hlc`], [`lww`].
//! - Backups: [`backup`], an online physical copy of a durable store and its
//!   restore.
//! - Counters: [`sequence`], named sequences that hand out unique, increasing
//!   numbers outside transactions.

#[cfg(not(target_arch = "wasm32"))]
pub mod backup;
pub mod bm25;
pub mod conflict;
pub mod csr;
pub mod delta;
mod dense_map;
pub mod dump;
#[cfg(feature = "encryption")]
pub mod encryption;
pub mod engine;
pub mod error;
pub mod hlc;
mod id_set;
pub mod labels;
pub mod levenshtein;
pub mod lww;
pub mod model;
pub mod native;
pub mod native_fts;
pub mod native_label_index;
pub mod native_path_index;
pub mod native_property_index;
pub mod native_text_index;
mod properties;
pub mod replica;
#[cfg(not(target_arch = "wasm32"))]
mod sealed_file;
pub mod sequence;
pub mod tokenizer;
pub mod value_encoding;
pub mod wal_format;
