//! Change-data-capture (CDC) decoding + schema mapping — Phase 15 task `00097`.
//!
//! The [`streaming`](crate::streaming) engine (`00096`) ingests a flat stream of
//! [`IngestEvent`]s addressed by producer-owned keys. The most common producer
//! of such a stream in practice is **change data capture**: an upstream
//! relational database (Postgres, MySQL, …) tails its write-ahead log and emits
//! a row-level change feed. This module is the bridge from that *relational*
//! change feed to drevo's *graph* event model.
//!
//! It does two things, both dependency-free and WASM-safe:
//!
//! 1. **Decode** ([`CdcChange`]) — parse the standard JSON a Postgres logical
//!    replication slot emits via the ubiquitous [`wal2json`] output plugin
//!    (format-version 1): an envelope `{"change":[ … ]}` of per-row
//!    insert / update / delete records. drevo bundles **no** Postgres driver —
//!    decoding the plugin's plain-JSON output keeps this an always-compiled,
//!    WASM-safe substrate, exactly as `00096` decodes broker messages without
//!    bundling a Kafka client. A deployment supplies the actual replication-slot
//!    connection (`tokio-postgres` + `START_REPLICATION`, Debezium, an HTTP CDC
//!    tail) and feeds the bytes here. The decoder is format-tolerant: it accepts
//!    the `{"change":[…]}` envelope, a bare `[…]` array, or a single change
//!    object.
//!
//! 2. **Map** ([`SchemaMap`]) — turn each decoded row change into zero or more
//!    [`IngestEvent`]s under a declarative, per-table mapping
//!    ([`TableMapping`]): a row becomes an [`UpsertNode`](IngestEvent::UpsertNode)
//!    (its primary key → a stable, namespaced [`EntityKey`], chosen columns →
//!    `title` / `body` / `properties`), and each declared foreign key
//!    ([`ForeignKey`]) becomes an outgoing
//!    [`UpsertEdge`](IngestEvent::UpsertEdge) to the referenced row's node. A
//!    `DELETE` becomes a [`DeleteNode`](IngestEvent::DeleteNode) (plus
//!    [`DeleteEdge`](IngestEvent::DeleteEdge)s when the slot runs with
//!    `REPLICA IDENTITY FULL`, which carries the old foreign-key values).
//!
//! The produced events feed the existing `00096`
//! [`IngestConsumer`](crate::streaming::IngestConsumer) /
//! [`IngestSink`](crate::streaming::IngestSink) machinery unchanged, inheriting
//! its at-least-once, idempotent, dead-letter guarantees. Because every produced
//! event carries full desired state (last-writer-wins) and a stable key, a
//! re-delivered CDC window converges on the identical graph.
//!
//! # Example
//!
//! ```
//! use drevo::streaming::{SchemaMap, TableMapping, ForeignKey, PropertyColumns, IngestEvent};
//!
//! // Two Postgres tables: notes(id, title, body, mood, author_id) → people(id, name).
//! let schema = SchemaMap::new()
//!     .map_table(
//!         "public.people",
//!         TableMapping::new("person", "id").title_column("name"),
//!     )
//!     .map_table(
//!         "public.notes",
//!         TableMapping::new("note", "id")
//!             .title_column("title")
//!             .body_column("body")
//!             .properties(PropertyColumns::AllExceptStructural)
//!             .foreign_key(ForeignKey::new("author_id", "authored_by", "public.people")),
//!     );
//!
//! let wal = br#"{"change":[
//!     {"kind":"insert","schema":"public","table":"notes",
//!      "columnnames":["id","title","body","mood","author_id"],
//!      "columnvalues":[42,"Hello","first post","calm",7]}
//! ]}"#;
//!
//! let events = schema.map_wal2json(wal).unwrap();
//! // One UpsertNode (note:42) + one UpsertEdge (note:42 -authored_by-> person:7).
//! assert_eq!(events.len(), 2);
//! assert!(matches!(&events[0], IngestEvent::UpsertNode { .. }));
//! assert!(matches!(&events[1], IngestEvent::UpsertEdge { .. }));
//! ```
//!
//! # Scope
//!
//! Like the rest of `00096`, this is ingestion *substrate*: a decoder and a
//! mapping engine, not a running pipeline. It bundles no database driver, opens
//! no socket, and is not wired into the executor / HTTP API / Bolt session. It
//! keeps its own [`CdcError`] channel rather than widening the crate-wide
//! `DrevoError`, mirroring how the streaming engine keeps
//! [`StreamError`](crate::streaming::StreamError).
//!
//! [`wal2json`]: https://github.com/eulerto/wal2json

use crate::streaming::event::{EventProperties, IngestEvent};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// A failure raised while decoding or mapping a CDC change feed.
///
/// Kept separate from [`StreamError`](crate::streaming::StreamError): that type
/// stamps consume-loop failures with a broker [`Offset`](crate::streaming::Offset),
/// whereas these are payload-shape and configuration errors that arise *before*
/// an event reaches the consumer.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CdcError {
    /// The bytes were not a decodable wal2json payload (not JSON, or not the
    /// expected envelope / change shape). Carries a human-readable reason.
    #[error("could not decode CDC payload: {0}")]
    Decode(String),

    /// The change record named an operation this mapper does not translate
    /// (e.g. `truncate`, or a wal2json `message`). Carries the raw kind.
    #[error("unsupported CDC operation: {0}")]
    UnsupportedOp(String),

    /// A change targeted a table with no [`TableMapping`] registered in the
    /// [`SchemaMap`], and the map is in strict mode (see
    /// [`SchemaMap::ignore_unmapped`]). Carries the fully-qualified table name.
    #[error("no mapping configured for table '{0}'")]
    UnmappedTable(String),

    /// A row did not carry the configured key column (or it was `null`), so no
    /// stable [`EntityKey`](crate::streaming::EntityKey) could be derived.
    #[error("row in table '{table}' is missing a value for key column '{column}'")]
    MissingKey {
        /// The fully-qualified table the row belonged to.
        table: String,
        /// The key column that was absent or null.
        column: String,
    },
}

/// Convenience alias for results from the CDC layer.
pub type Result<T> = core::result::Result<T, CdcError>;

/// The DML operation a [`CdcChange`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOp {
    /// A new row was inserted.
    Insert,
    /// An existing row was updated (wal2json carries the full new image).
    Update,
    /// A row was deleted (wal2json carries the old identity in `oldkeys`).
    Delete,
}

impl ChangeOp {
    /// Parse the wal2json `kind` string (`"insert"` / `"update"` / `"delete"`).
    ///
    /// # Errors
    ///
    /// Returns [`CdcError::UnsupportedOp`] for any other kind (`"truncate"`,
    /// `"message"`, …) — those have no row-level graph translation.
    pub fn from_kind(kind: &str) -> Result<Self> {
        match kind {
            "insert" => Ok(ChangeOp::Insert),
            "update" => Ok(ChangeOp::Update),
            "delete" => Ok(ChangeOp::Delete),
            other => Err(CdcError::UnsupportedOp(other.to_string())),
        }
    }

    /// Whether this operation removes the row (rather than upserting it).
    #[must_use]
    pub const fn is_delete(self) -> bool {
        matches!(self, ChangeOp::Delete)
    }
}

/// One decoded row-level change from a Postgres logical-replication stream.
///
/// This is the relational counterpart of an [`IngestEvent`]: it still speaks in
/// tables, columns, and primary keys. [`SchemaMap::map_change`] is what lifts it
/// into the graph model.
#[derive(Debug, Clone, PartialEq)]
pub struct CdcChange {
    /// The DML operation.
    pub op: ChangeOp,
    /// The source schema (e.g. `"public"`).
    pub schema: String,
    /// The source table (e.g. `"notes"`).
    pub table: String,
    /// The new row image (column → value). Populated for insert / update;
    /// empty for delete.
    pub columns: BTreeMap<String, serde_json::Value>,
    /// The row's identity columns (`oldkeys`). Populated for update / delete;
    /// under `REPLICA IDENTITY FULL` this is the *entire* old row, which lets a
    /// delete reconstruct its foreign-key edges.
    pub identity: BTreeMap<String, serde_json::Value>,
}

/// Internal serde shape of one wal2json `change[]` element (format-version 1).
#[derive(Debug, Deserialize, Serialize)]
struct RawChange {
    kind: String,
    schema: String,
    table: String,
    #[serde(default)]
    columnnames: Vec<String>,
    #[serde(default)]
    columnvalues: Vec<serde_json::Value>,
    #[serde(default)]
    oldkeys: Option<RawOldKeys>,
}

/// Internal serde shape of a wal2json `oldkeys` object.
#[derive(Debug, Deserialize, Serialize)]
struct RawOldKeys {
    #[serde(default)]
    keynames: Vec<String>,
    #[serde(default)]
    keyvalues: Vec<serde_json::Value>,
}

/// Internal serde shape of the wal2json envelope `{"change":[ … ]}`.
#[derive(Debug, Deserialize)]
struct RawEnvelope {
    change: Vec<RawChange>,
}

fn zip_columns(
    names: Vec<String>,
    values: Vec<serde_json::Value>,
) -> BTreeMap<String, serde_json::Value> {
    names.into_iter().zip(values).collect()
}

impl CdcChange {
    /// The fully-qualified table name, `"schema.table"`.
    #[must_use]
    pub fn fqtn(&self) -> String {
        format!("{}.{}", self.schema, self.table)
    }

    /// Look a column value up in the new image, then (for deletes) the identity.
    #[must_use]
    fn lookup(&self, column: &str) -> Option<&serde_json::Value> {
        self.columns
            .get(column)
            .or_else(|| self.identity.get(column))
    }

    fn from_raw(raw: RawChange) -> Result<Self> {
        let op = ChangeOp::from_kind(&raw.kind)?;
        if raw.columnnames.len() != raw.columnvalues.len() {
            return Err(CdcError::Decode(format!(
                "table '{}.{}': {} column names but {} values",
                raw.schema,
                raw.table,
                raw.columnnames.len(),
                raw.columnvalues.len()
            )));
        }
        let identity = match raw.oldkeys {
            Some(ref ok) if ok.keynames.len() != ok.keyvalues.len() => {
                return Err(CdcError::Decode(format!(
                    "table '{}.{}': {} oldkey names but {} values",
                    raw.schema,
                    raw.table,
                    ok.keynames.len(),
                    ok.keyvalues.len()
                )));
            }
            Some(ok) => zip_columns(ok.keynames, ok.keyvalues),
            None => BTreeMap::new(),
        };
        Ok(CdcChange {
            op,
            schema: raw.schema,
            table: raw.table,
            columns: zip_columns(raw.columnnames, raw.columnvalues),
            identity,
        })
    }

    /// Decode a wal2json payload into its sequence of row changes.
    ///
    /// Accepts the standard `{"change":[ … ]}` envelope, a bare `[ … ]` array of
    /// change objects, or a single change object — whichever the upstream tail
    /// hands over.
    ///
    /// # Errors
    ///
    /// [`CdcError::Decode`] if the bytes are not JSON or not a recognizable
    /// wal2json shape; [`CdcError::UnsupportedOp`] if any change names an
    /// untranslatable operation.
    pub fn parse_wal2json(bytes: &[u8]) -> Result<Vec<Self>> {
        let value: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|e| CdcError::Decode(format!("invalid JSON: {e}")))?;

        let raws: Vec<RawChange> = match value {
            serde_json::Value::Object(ref map) if map.contains_key("change") => {
                let env: RawEnvelope = serde_json::from_value(value)
                    .map_err(|e| CdcError::Decode(format!("invalid wal2json envelope: {e}")))?;
                env.change
            }
            serde_json::Value::Object(_) => {
                let one: RawChange = serde_json::from_value(value)
                    .map_err(|e| CdcError::Decode(format!("invalid change object: {e}")))?;
                vec![one]
            }
            serde_json::Value::Array(_) => serde_json::from_value(value)
                .map_err(|e| CdcError::Decode(format!("invalid change array: {e}")))?,
            other => {
                return Err(CdcError::Decode(format!(
                    "expected a wal2json object or array, got {}",
                    json_type_name(&other)
                )));
            }
        };

        raws.into_iter().map(CdcChange::from_raw).collect()
    }
}

fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Render a scalar JSON value to the string form used for keys and titles.
///
/// Returns `None` for `null` (no usable value). Strings pass through verbatim;
/// numbers and booleans render to their literal form; arrays / objects fall back
/// to compact JSON (composite keys are unusual but handled rather than dropped).
fn scalar_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}

/// Which of a table's columns become node [`properties`](IngestEvent::UpsertNode).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PropertyColumns {
    /// Carry every column that is not already consumed structurally (the key,
    /// title, body, and any foreign-key columns). The default.
    #[default]
    AllExceptStructural,
    /// Carry only the named columns (those present on the row).
    Only(Vec<String>),
    /// Carry no properties.
    None,
}

/// A foreign-key column that becomes an outgoing edge to the referenced row's
/// node.
///
/// The edge runs *from* this row's node *to* the node of the row identified by
/// the foreign-key value in the referenced table. The edge's key is derived
/// deterministically from its endpoints and kind, so replaying the change
/// re-targets the same edge (idempotent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    /// The foreign-key column on *this* table.
    pub column: String,
    /// The [`kind`](IngestEvent::UpsertEdge) assigned to the produced edge.
    pub edge_kind: String,
    /// The mapped table name (`"schema.table"` or bare) the FK references — used
    /// to namespace the target node's [`EntityKey`](crate::streaming::EntityKey).
    pub references: String,
}

impl ForeignKey {
    /// Declare a foreign-key edge: `column` on this table references `table`,
    /// producing an edge of kind `edge_kind`.
    #[must_use]
    pub fn new(
        column: impl Into<String>,
        edge_kind: impl Into<String>,
        references: impl Into<String>,
    ) -> Self {
        ForeignKey {
            column: column.into(),
            edge_kind: edge_kind.into(),
            references: references.into(),
        }
    }
}

/// How rows of one table become graph nodes (and their foreign keys, edges).
///
/// Built fluently:
///
/// ```
/// use drevo::streaming::{TableMapping, ForeignKey, PropertyColumns};
///
/// let m = TableMapping::new("task", "id")
///     .title_column("summary")
///     .body_column("description")
///     .properties(PropertyColumns::AllExceptStructural)
///     .foreign_key(ForeignKey::new("assignee_id", "assigned_to", "public.users"));
/// assert_eq!(m.node_kind, "task");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct TableMapping {
    /// The node [`kind`](IngestEvent::UpsertNode) assigned to rows of this table.
    pub node_kind: String,
    /// The column whose value is the row's stable identity (the primary key).
    pub key_column: String,
    /// Namespace prefix for the produced [`EntityKey`](crate::streaming::EntityKey),
    /// keeping keys from distinct tables disjoint. Defaults to the table name
    /// passed to [`SchemaMap::map_table`].
    pub key_prefix: Option<String>,
    /// Column mapped to the node title; falls back to the key string if unset or
    /// `null`.
    pub title_column: Option<String>,
    /// Column mapped to the node body; defaults to empty if unset or `null`.
    pub body_column: Option<String>,
    /// Which columns become node properties.
    pub properties: PropertyColumns,
    /// Foreign-key columns that become outgoing edges.
    pub foreign_keys: Vec<ForeignKey>,
}

impl TableMapping {
    /// Start a mapping: rows of this table become `node_kind` nodes keyed by
    /// `key_column`. Title defaults to the key, body to empty, properties to
    /// [`PropertyColumns::AllExceptStructural`], with no foreign keys until
    /// [`foreign_key`](Self::foreign_key) is called.
    #[must_use]
    pub fn new(node_kind: impl Into<String>, key_column: impl Into<String>) -> Self {
        TableMapping {
            node_kind: node_kind.into(),
            key_column: key_column.into(),
            key_prefix: None,
            title_column: None,
            body_column: None,
            properties: PropertyColumns::default(),
            foreign_keys: Vec::new(),
        }
    }

    /// Override the [`EntityKey`](crate::streaming::EntityKey) namespace prefix
    /// (defaults to the registered table name).
    #[must_use]
    pub fn key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = Some(prefix.into());
        self
    }

    /// Map a column to the node title.
    #[must_use]
    pub fn title_column(mut self, column: impl Into<String>) -> Self {
        self.title_column = Some(column.into());
        self
    }

    /// Map a column to the node body.
    #[must_use]
    pub fn body_column(mut self, column: impl Into<String>) -> Self {
        self.body_column = Some(column.into());
        self
    }

    /// Choose which columns become node properties.
    #[must_use]
    pub fn properties(mut self, columns: PropertyColumns) -> Self {
        self.properties = columns;
        self
    }

    /// Declare a foreign-key edge on this table.
    #[must_use]
    pub fn foreign_key(mut self, fk: ForeignKey) -> Self {
        self.foreign_keys.push(fk);
        self
    }

    /// The set of columns consumed structurally (key, title, body, FK columns),
    /// excluded from `AllExceptStructural` properties.
    fn structural_columns(&self) -> HashSet<&str> {
        let mut set = HashSet::new();
        set.insert(self.key_column.as_str());
        if let Some(t) = &self.title_column {
            set.insert(t.as_str());
        }
        if let Some(b) = &self.body_column {
            set.insert(b.as_str());
        }
        for fk in &self.foreign_keys {
            set.insert(fk.column.as_str());
        }
        set
    }

    fn build_properties(&self, change: &CdcChange) -> EventProperties {
        match &self.properties {
            PropertyColumns::None => EventProperties::new(),
            PropertyColumns::Only(cols) => cols
                .iter()
                .filter_map(|c| change.columns.get(c).map(|v| (c.clone(), v.clone())))
                .collect(),
            PropertyColumns::AllExceptStructural => {
                let structural = self.structural_columns();
                change
                    .columns
                    .iter()
                    .filter(|(name, _)| !structural.contains(name.as_str()))
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect()
            }
        }
    }
}

/// A declarative mapping from a relational schema to drevo's graph event model.
///
/// Register one [`TableMapping`] per table you replicate, then feed decoded
/// changes (or raw wal2json bytes) through [`map_change`](Self::map_change) /
/// [`map_wal2json`](Self::map_wal2json) to obtain [`IngestEvent`]s for the
/// `00096` consumer.
#[derive(Debug, Clone, Default)]
pub struct SchemaMap {
    tables: HashMap<String, TableMapping>,
    ignore_unmapped: bool,
}

impl SchemaMap {
    /// An empty, strict map (changes to unmapped tables are
    /// [`CdcError::UnmappedTable`] errors).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a table mapping under `table` (use the fully-qualified
    /// `"schema.table"` to disambiguate same-named tables across schemas; a bare
    /// table name also matches). The table name doubles as the default
    /// [`EntityKey`](crate::streaming::EntityKey) prefix. Builder style.
    #[must_use]
    pub fn map_table(mut self, table: impl Into<String>, mapping: TableMapping) -> Self {
        self.insert(table, mapping);
        self
    }

    /// Register a table mapping, mutating in place.
    pub fn insert(&mut self, table: impl Into<String>, mapping: TableMapping) {
        self.tables.insert(table.into(), mapping);
    }

    /// Silently drop changes to tables that have no mapping rather than
    /// erroring. Useful when a publication includes tables you do not model.
    /// Builder style.
    #[must_use]
    pub fn ignore_unmapped(mut self, ignore: bool) -> Self {
        self.ignore_unmapped = ignore;
        self
    }

    /// Resolve the mapping for a change, trying the fully-qualified name first,
    /// then the bare table name. Returns the registered name (the default key
    /// prefix) alongside the mapping.
    fn mapping_for(&self, change: &CdcChange) -> Option<(String, &TableMapping)> {
        let fqtn = change.fqtn();
        if let Some(m) = self.tables.get(&fqtn) {
            return Some((fqtn, m));
        }
        self.tables
            .get(&change.table)
            .map(|m| (change.table.clone(), m))
    }

    /// The key prefix a referenced table uses, for building edge targets. Tries
    /// the FK's `references` name as registered; falls back to it verbatim.
    fn prefix_of_reference<'a>(&'a self, references: &'a str) -> &'a str {
        match self.tables.get(references) {
            Some(m) => m.key_prefix.as_deref().unwrap_or(references),
            None => references,
        }
    }

    /// Map one decoded change into its [`IngestEvent`]s.
    ///
    /// * insert / update → one [`UpsertNode`](IngestEvent::UpsertNode) plus one
    ///   [`UpsertEdge`](IngestEvent::UpsertEdge) per non-null foreign key.
    /// * delete → one [`DeleteNode`](IngestEvent::DeleteNode) plus one
    ///   [`DeleteEdge`](IngestEvent::DeleteEdge) per foreign key whose old value
    ///   is available (i.e. under `REPLICA IDENTITY FULL`); otherwise the node
    ///   delete alone (the graph's cascade reclaims its edges).
    ///
    /// # Errors
    ///
    /// [`CdcError::UnmappedTable`] for an unmapped table in strict mode;
    /// [`CdcError::MissingKey`] when the key column is absent or `null`.
    pub fn map_change(&self, change: &CdcChange) -> Result<Vec<IngestEvent>> {
        let Some((prefix_reg, mapping)) = self.mapping_for(change) else {
            if self.ignore_unmapped {
                return Ok(Vec::new());
            }
            return Err(CdcError::UnmappedTable(change.fqtn()));
        };
        let prefix = mapping.key_prefix.as_deref().unwrap_or(&prefix_reg);

        let node_key = self.entity_key(prefix, mapping, change)?;
        let mut events = Vec::new();

        if change.op.is_delete() {
            // Edges first so a downstream that does not cascade can still tear
            // the node's edges down before the node vanishes.
            for fk in &mapping.foreign_keys {
                if let Some(target_key) = self.edge_target(fk, change) {
                    events.push(IngestEvent::DeleteEdge {
                        key: edge_key(&node_key, &fk.edge_kind, &target_key),
                    });
                }
            }
            events.push(IngestEvent::DeleteNode { key: node_key });
            return Ok(events);
        }

        let title = mapping
            .title_column
            .as_ref()
            .and_then(|c| change.columns.get(c))
            .and_then(scalar_to_string)
            .unwrap_or_else(|| node_key.clone());
        let body = mapping
            .body_column
            .as_ref()
            .and_then(|c| change.columns.get(c))
            .and_then(scalar_to_string)
            .unwrap_or_default();

        events.push(IngestEvent::UpsertNode {
            key: node_key.clone(),
            kind: mapping.node_kind.clone(),
            title,
            body,
            properties: mapping.build_properties(change),
        });

        for fk in &mapping.foreign_keys {
            if let Some(target_key) = self.edge_target(fk, change) {
                events.push(IngestEvent::UpsertEdge {
                    key: edge_key(&node_key, &fk.edge_kind, &target_key),
                    from: node_key.clone(),
                    to: target_key,
                    kind: fk.edge_kind.clone(),
                    weight: 1.0,
                    properties: EventProperties::new(),
                });
            }
        }

        Ok(events)
    }

    /// Decode a wal2json payload and map every change in it, in order.
    ///
    /// # Errors
    ///
    /// Any [`CdcError`] from decoding ([`CdcChange::parse_wal2json`]) or mapping
    /// ([`map_change`](Self::map_change)).
    pub fn map_wal2json(&self, bytes: &[u8]) -> Result<Vec<IngestEvent>> {
        let changes = CdcChange::parse_wal2json(bytes)?;
        let mut events = Vec::new();
        for change in &changes {
            events.extend(self.map_change(change)?);
        }
        Ok(events)
    }

    /// Build a row's namespaced entity key from its key column.
    fn entity_key(
        &self,
        prefix: &str,
        mapping: &TableMapping,
        change: &CdcChange,
    ) -> Result<String> {
        let raw = change
            .lookup(&mapping.key_column)
            .and_then(scalar_to_string)
            .ok_or_else(|| CdcError::MissingKey {
                table: change.fqtn(),
                column: mapping.key_column.clone(),
            })?;
        Ok(format!("{prefix}:{raw}"))
    }

    /// Build the target node key for a foreign-key edge, or `None` when the FK
    /// value is absent / null (a nullable FK with no referent).
    fn edge_target(&self, fk: &ForeignKey, change: &CdcChange) -> Option<String> {
        let raw = change.lookup(&fk.column).and_then(scalar_to_string)?;
        let target_prefix = self.prefix_of_reference(&fk.references);
        Some(format!("{target_prefix}:{raw}"))
    }
}

/// Deterministic, replay-stable key for a foreign-key edge.
fn edge_key(from: &str, kind: &str, to: &str) -> String {
    format!("{from}-{kind}->{to}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn insert_wal(table: &str, names: &[&str], values: serde_json::Value) -> Vec<u8> {
        json!({
            "change": [{
                "kind": "insert",
                "schema": "public",
                "table": table,
                "columnnames": names,
                "columnvalues": values,
            }]
        })
        .to_string()
        .into_bytes()
    }

    // ---- decoding -------------------------------------------------------

    #[test]
    fn change_op_parses_known_kinds_and_rejects_others() {
        assert_eq!(ChangeOp::from_kind("insert").unwrap(), ChangeOp::Insert);
        assert_eq!(ChangeOp::from_kind("update").unwrap(), ChangeOp::Update);
        assert_eq!(ChangeOp::from_kind("delete").unwrap(), ChangeOp::Delete);
        assert_eq!(
            ChangeOp::from_kind("truncate").unwrap_err(),
            CdcError::UnsupportedOp("truncate".into())
        );
    }

    #[test]
    fn parses_wal2json_envelope_insert() {
        let bytes = insert_wal("notes", &["id", "title"], json!([42, "Hello"]));
        let changes = CdcChange::parse_wal2json(&bytes).unwrap();
        assert_eq!(changes.len(), 1);
        let c = &changes[0];
        assert_eq!(c.op, ChangeOp::Insert);
        assert_eq!(c.fqtn(), "public.notes");
        assert_eq!(c.columns.get("id"), Some(&json!(42)));
        assert_eq!(c.columns.get("title"), Some(&json!("Hello")));
        assert!(c.identity.is_empty());
    }

    #[test]
    fn parses_delete_with_oldkeys_as_identity() {
        let bytes = json!({
            "change": [{
                "kind": "delete",
                "schema": "public",
                "table": "notes",
                "oldkeys": {"keynames": ["id"], "keyvalues": [42]}
            }]
        })
        .to_string()
        .into_bytes();
        let c = &CdcChange::parse_wal2json(&bytes).unwrap()[0];
        assert_eq!(c.op, ChangeOp::Delete);
        assert!(c.columns.is_empty());
        assert_eq!(c.identity.get("id"), Some(&json!(42)));
    }

    #[test]
    fn parses_bare_array_and_single_object() {
        let arr = json!([{
            "kind":"insert","schema":"public","table":"t",
            "columnnames":["id"],"columnvalues":[1]
        }])
        .to_string()
        .into_bytes();
        assert_eq!(CdcChange::parse_wal2json(&arr).unwrap().len(), 1);

        let obj = json!({
            "kind":"insert","schema":"public","table":"t",
            "columnnames":["id"],"columnvalues":[1]
        })
        .to_string()
        .into_bytes();
        assert_eq!(CdcChange::parse_wal2json(&obj).unwrap().len(), 1);
    }

    #[test]
    fn malformed_payloads_are_decode_errors_not_panics() {
        assert!(matches!(
            CdcChange::parse_wal2json(b"not json"),
            Err(CdcError::Decode(_))
        ));
        // A JSON scalar is not a change feed.
        assert!(matches!(
            CdcChange::parse_wal2json(b"42"),
            Err(CdcError::Decode(_))
        ));
    }

    #[test]
    fn mismatched_column_arity_is_a_decode_error() {
        let bytes = insert_wal("notes", &["id", "title"], json!([42])); // one value, two names
        assert!(matches!(
            CdcChange::parse_wal2json(&bytes),
            Err(CdcError::Decode(_))
        ));
    }

    #[test]
    fn unsupported_op_surfaces_through_parse() {
        let bytes = json!({"change":[{"kind":"truncate","schema":"public","table":"t"}]})
            .to_string()
            .into_bytes();
        assert_eq!(
            CdcChange::parse_wal2json(&bytes).unwrap_err(),
            CdcError::UnsupportedOp("truncate".into())
        );
    }

    // ---- mapping --------------------------------------------------------

    fn notes_schema() -> SchemaMap {
        SchemaMap::new()
            .map_table(
                "public.people",
                TableMapping::new("person", "id").title_column("name"),
            )
            .map_table(
                "public.notes",
                TableMapping::new("note", "id")
                    .title_column("title")
                    .body_column("body")
                    .properties(PropertyColumns::AllExceptStructural)
                    .foreign_key(ForeignKey::new("author_id", "authored_by", "public.people")),
            )
    }

    #[test]
    fn insert_maps_to_upsert_node_with_namespaced_key() {
        let schema = notes_schema();
        let change = &CdcChange::parse_wal2json(&insert_wal(
            "notes",
            &["id", "title", "body"],
            json!([42, "Hello", "first post"]),
        ))
        .unwrap()[0];
        let events = schema.map_change(change).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            IngestEvent::UpsertNode {
                key,
                kind,
                title,
                body,
                ..
            } => {
                assert_eq!(key, "public.notes:42");
                assert_eq!(kind, "note");
                assert_eq!(title, "Hello");
                assert_eq!(body, "first post");
            }
            other => panic!("expected UpsertNode, got {other:?}"),
        }
    }

    #[test]
    fn structural_columns_are_excluded_from_properties() {
        let schema = notes_schema();
        let change = &CdcChange::parse_wal2json(&insert_wal(
            "notes",
            &["id", "title", "body", "mood", "author_id"],
            json!([42, "Hello", "b", "calm", 7]),
        ))
        .unwrap()[0];
        let events = schema.map_change(change).unwrap();
        let IngestEvent::UpsertNode { properties, .. } = &events[0] else {
            panic!("expected UpsertNode");
        };
        // id/title/body/author_id are structural; only `mood` survives.
        assert_eq!(properties.len(), 1);
        assert_eq!(properties.get("mood"), Some(&json!("calm")));
    }

    #[test]
    fn only_property_columns_are_honored() {
        let schema = SchemaMap::new().map_table(
            "t",
            TableMapping::new("thing", "id").properties(PropertyColumns::Only(vec!["a".into()])),
        );
        let change = &CdcChange::parse_wal2json(&insert_wal(
            "t",
            &["id", "a", "b"],
            json!([1, "keep", "drop"]),
        ))
        .unwrap()[0];
        let IngestEvent::UpsertNode { properties, .. } = &schema.map_change(change).unwrap()[0]
        else {
            panic!("expected UpsertNode");
        };
        assert_eq!(properties.len(), 1);
        assert_eq!(properties.get("a"), Some(&json!("keep")));
    }

    #[test]
    fn foreign_key_becomes_a_namespaced_edge() {
        let schema = notes_schema();
        let change = &CdcChange::parse_wal2json(&insert_wal(
            "notes",
            &["id", "title", "author_id"],
            json!([42, "Hello", 7]),
        ))
        .unwrap()[0];
        let events = schema.map_change(change).unwrap();
        assert_eq!(events.len(), 2);
        match &events[1] {
            IngestEvent::UpsertEdge {
                key,
                from,
                to,
                kind,
                ..
            } => {
                assert_eq!(from, "public.notes:42");
                assert_eq!(to, "public.people:7");
                assert_eq!(kind, "authored_by");
                assert_eq!(key, "public.notes:42-authored_by->public.people:7");
            }
            other => panic!("expected UpsertEdge, got {other:?}"),
        }
    }

    #[test]
    fn null_foreign_key_produces_no_edge() {
        let schema = notes_schema();
        let change = &CdcChange::parse_wal2json(&insert_wal(
            "notes",
            &["id", "title", "author_id"],
            json!([42, "Hello", null]),
        ))
        .unwrap()[0];
        let events = schema.map_change(change).unwrap();
        assert_eq!(events.len(), 1); // node only, no edge
        assert!(matches!(events[0], IngestEvent::UpsertNode { .. }));
    }

    #[test]
    fn title_falls_back_to_key_when_absent() {
        let schema =
            SchemaMap::new().map_table("t", TableMapping::new("thing", "id").title_column("name"));
        let change = &CdcChange::parse_wal2json(&insert_wal("t", &["id"], json!([99]))).unwrap()[0];
        let IngestEvent::UpsertNode { title, .. } = &schema.map_change(change).unwrap()[0] else {
            panic!("expected UpsertNode");
        };
        assert_eq!(title, "t:99");
    }

    #[test]
    fn delete_maps_to_delete_node() {
        let schema = notes_schema();
        let bytes = json!({
            "change": [{
                "kind": "delete", "schema": "public", "table": "notes",
                "oldkeys": {"keynames": ["id"], "keyvalues": [42]}
            }]
        })
        .to_string()
        .into_bytes();
        let change = &CdcChange::parse_wal2json(&bytes).unwrap()[0];
        let events = schema.map_change(change).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            IngestEvent::DeleteNode {
                key: "public.notes:42".into()
            }
        );
    }

    #[test]
    fn delete_with_replica_identity_full_also_deletes_edges() {
        let schema = notes_schema();
        // REPLICA IDENTITY FULL → oldkeys carries the whole old row incl. author_id.
        let bytes = json!({
            "change": [{
                "kind": "delete", "schema": "public", "table": "notes",
                "oldkeys": {
                    "keynames": ["id", "title", "author_id"],
                    "keyvalues": [42, "Hello", 7]
                }
            }]
        })
        .to_string()
        .into_bytes();
        let change = &CdcChange::parse_wal2json(&bytes).unwrap()[0];
        let events = schema.map_change(change).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            IngestEvent::DeleteEdge {
                key: "public.notes:42-authored_by->public.people:7".into()
            }
        );
        assert_eq!(
            events[1],
            IngestEvent::DeleteNode {
                key: "public.notes:42".into()
            }
        );
    }

    #[test]
    fn update_is_last_writer_wins_upsert() {
        let schema = notes_schema();
        let bytes = json!({
            "change": [{
                "kind": "update", "schema": "public", "table": "notes",
                "columnnames": ["id", "title", "body"],
                "columnvalues": [42, "Edited", "revised"],
                "oldkeys": {"keynames": ["id"], "keyvalues": [42]}
            }]
        })
        .to_string()
        .into_bytes();
        let change = &CdcChange::parse_wal2json(&bytes).unwrap()[0];
        let IngestEvent::UpsertNode { key, title, .. } = &schema.map_change(change).unwrap()[0]
        else {
            panic!("expected UpsertNode");
        };
        assert_eq!(key, "public.notes:42");
        assert_eq!(title, "Edited");
    }

    #[test]
    fn unmapped_table_errors_in_strict_mode() {
        let schema = notes_schema();
        let change =
            &CdcChange::parse_wal2json(&insert_wal("widgets", &["id"], json!([1]))).unwrap()[0];
        assert_eq!(
            schema.map_change(change).unwrap_err(),
            CdcError::UnmappedTable("public.widgets".into())
        );
    }

    #[test]
    fn ignore_unmapped_drops_unknown_tables() {
        let schema = notes_schema().ignore_unmapped(true);
        let change =
            &CdcChange::parse_wal2json(&insert_wal("widgets", &["id"], json!([1]))).unwrap()[0];
        assert!(schema.map_change(change).unwrap().is_empty());
    }

    #[test]
    fn missing_key_column_is_an_error() {
        let schema = SchemaMap::new().map_table("t", TableMapping::new("thing", "id"));
        // Row has no `id` column at all.
        let change =
            &CdcChange::parse_wal2json(&insert_wal("t", &["name"], json!(["x"]))).unwrap()[0];
        assert_eq!(
            schema.map_change(change).unwrap_err(),
            CdcError::MissingKey {
                table: "public.t".into(),
                column: "id".into(),
            }
        );
    }

    #[test]
    fn bare_table_name_matches_when_fqtn_unregistered() {
        // Registered under the bare name, change arrives fully-qualified.
        let schema = SchemaMap::new().map_table("notes", TableMapping::new("note", "id"));
        let change =
            &CdcChange::parse_wal2json(&insert_wal("notes", &["id"], json!([5]))).unwrap()[0];
        let IngestEvent::UpsertNode { key, .. } = &schema.map_change(change).unwrap()[0] else {
            panic!("expected UpsertNode");
        };
        assert_eq!(key, "notes:5");
    }

    #[test]
    fn explicit_key_prefix_overrides_table_name() {
        let schema = SchemaMap::new().map_table(
            "public.notes",
            TableMapping::new("note", "id").key_prefix("n"),
        );
        let change =
            &CdcChange::parse_wal2json(&insert_wal("notes", &["id"], json!([5]))).unwrap()[0];
        let IngestEvent::UpsertNode { key, .. } = &schema.map_change(change).unwrap()[0] else {
            panic!("expected UpsertNode");
        };
        assert_eq!(key, "n:5");
    }

    #[test]
    fn map_wal2json_flattens_a_multi_change_batch() {
        let schema = notes_schema();
        let bytes = json!({
            "change": [
                {"kind":"insert","schema":"public","table":"people",
                 "columnnames":["id","name"],"columnvalues":[7,"Ada"]},
                {"kind":"insert","schema":"public","table":"notes",
                 "columnnames":["id","title","author_id"],"columnvalues":[42,"Hi",7]}
            ]
        })
        .to_string()
        .into_bytes();
        let events = schema.map_wal2json(&bytes).unwrap();
        // person node + note node + authored_by edge.
        assert_eq!(events.len(), 3);
    }
}
