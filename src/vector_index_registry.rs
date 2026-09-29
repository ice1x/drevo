//! Named vector-index registry (issue #532).
//!
//! Neo4j addresses a vector index by an arbitrary **name** —
//! `CREATE VECTOR INDEX moviePlots FOR (m:Movie) ON (m.plotEmbedding)` and then
//! `CALL db.index.vector.queryNodes('moviePlots', 5, $q)`. drevo has always been
//! able to *search* an embedding property (`drevo.vector.query`, the `SEARCH …
//! VECTOR INDEX Label.property` clause), but it had no way to bind that arbitrary
//! name to the `(label, property)` pair it stands for. This registry is that
//! binding, and nothing more: drevo auto-indexes embeddings, so there is no
//! separate structure to build — the name is metadata that lets a Neo4j-shaped
//! client's `CREATE VECTOR INDEX` / `db.index.vector.queryNodes` round-trip work
//! unmodified.
//!
//! It is the vector-index sibling of [`crate::semantic_index::SemanticIndexRegistry`]
//! and is persisted the same way — in the `semantic.json` sidecar next to the WAL
//! (see [`crate::native_service`]) — so registrations survive a restart.

use serde::{Deserialize, Serialize};

/// What a vector index covers: node embeddings (`FOR (n:Label)`) or
/// relationship embeddings (`FOR ()-[r:TYPE]-()`). Neo4j's `entityType`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VectorIndexEntity {
    /// A node index, queried with `db.index.vector.queryNodes`.
    #[default]
    Node,
    /// A relationship index, queried with `db.index.vector.queryRelationships`.
    Relationship,
}

/// One named vector index: a Neo4j-compatible `name` bound to the
/// `(label, property)` whose embeddings it targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorIndex {
    /// The index name, as given to `CREATE VECTOR INDEX <name>` and looked up by
    /// `db.index.vector.queryNodes(<name>, …)`. Case-sensitive, like Neo4j.
    pub name: String,
    /// Node label (the `FOR (n:Label)` label) or, for a relationship index,
    /// relationship type (the `FOR ()-[r:TYPE]-()` type) the index covers.
    pub label: String,
    /// Embedding property the index targets (the `ON (n.property)` property).
    pub property: String,
    /// Node or relationship index. Absent in sidecars written before
    /// relationship indexes existed, which were all node indexes.
    #[serde(default)]
    pub entity: VectorIndexEntity,
}

/// Errors from vector-index registry operations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VectorIndexError {
    /// A `CREATE VECTOR INDEX` (without `IF NOT EXISTS`) named an index that
    /// already exists.
    #[error("a vector index named `{0}` already exists")]
    AlreadyExists(String),
    /// A lookup / drop named an index that does not exist.
    #[error("no vector index named `{0}`")]
    NotFound(String),
}

/// A registry of named vector indexes, keyed by `name`.
///
/// A `Vec` (not a map) so the serialised form is a plain JSON array — a stable,
/// diff-friendly shape in the `semantic.json` sidecar. Empty by default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorIndexRegistry {
    indexes: Vec<VectorIndex>,
}

impl VectorIndexRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The index named `name`, if registered (case-sensitive, Neo4j-style).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&VectorIndex> {
        self.indexes.iter().find(|i| i.name == name)
    }

    /// Every registered index, in creation order.
    #[must_use]
    pub fn list(&self) -> &[VectorIndex] {
        &self.indexes
    }

    /// Register a new vector index.
    ///
    /// `CREATE VECTOR INDEX <name> FOR (n:label) ON (n.property)` (or, with
    /// [`VectorIndexEntity::Relationship`], `FOR ()-[r:label]-() ON
    /// (r.property)`). When `if_not_exists` is set (the `IF NOT EXISTS` form) a
    /// name clash is a no-op; otherwise it is [`VectorIndexError::AlreadyExists`].
    /// Matches Neo4j, where a name is unique across all index types.
    ///
    /// # Errors
    /// [`VectorIndexError::AlreadyExists`] if `name` is taken and `if_not_exists`
    /// is false.
    pub fn create(
        &mut self,
        name: String,
        label: String,
        property: String,
        entity: VectorIndexEntity,
        if_not_exists: bool,
    ) -> Result<VectorIndex, VectorIndexError> {
        if let Some(existing) = self.get(&name) {
            if if_not_exists {
                return Ok(existing.clone());
            }
            return Err(VectorIndexError::AlreadyExists(name));
        }
        let index = VectorIndex {
            name,
            label,
            property,
            entity,
        };
        self.indexes.push(index.clone());
        Ok(index)
    }

    /// Remove the index named `name`.
    ///
    /// When `if_exists` is set (the `IF EXISTS` form) a missing name is a no-op;
    /// otherwise it is [`VectorIndexError::NotFound`].
    ///
    /// # Errors
    /// [`VectorIndexError::NotFound`] if `name` is unknown and `if_exists` is
    /// false.
    // Named `remove`, not `drop`, so a `registry.drop(..)` call site can never be
    // mistaken for the `Drop::drop` destructor.
    pub fn remove(&mut self, name: &str, if_exists: bool) -> Result<(), VectorIndexError> {
        let before = self.indexes.len();
        self.indexes.retain(|i| i.name != name);
        if self.indexes.len() == before && !if_exists {
            return Err(VectorIndexError::NotFound(name.to_string()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_get_and_list() {
        let mut reg = VectorIndexRegistry::new();
        let idx = reg
            .create(
                "moviePlots".into(),
                "Movie".into(),
                "plotEmbedding".into(),
                VectorIndexEntity::Node,
                false,
            )
            .unwrap();
        assert_eq!(idx.name, "moviePlots");
        assert_eq!(idx.label, "Movie");
        assert_eq!(idx.property, "plotEmbedding");

        let got = reg.get("moviePlots").expect("registered");
        assert_eq!(got, &idx);
        assert_eq!(reg.list().len(), 1);
    }

    #[test]
    fn get_is_case_sensitive_like_neo4j() {
        let mut reg = VectorIndexRegistry::new();
        reg.create(
            "MoviePlots".into(),
            "Movie".into(),
            "e".into(),
            VectorIndexEntity::Node,
            false,
        )
        .unwrap();
        assert!(reg.get("movieplots").is_none());
        assert!(reg.get("MoviePlots").is_some());
    }

    #[test]
    fn duplicate_name_without_if_not_exists_errors() {
        let mut reg = VectorIndexRegistry::new();
        reg.create(
            "i".into(),
            "A".into(),
            "e".into(),
            VectorIndexEntity::Node,
            false,
        )
        .unwrap();
        let err = reg
            .create(
                "i".into(),
                "B".into(),
                "other".into(),
                VectorIndexEntity::Node,
                false,
            )
            .unwrap_err();
        assert_eq!(err, VectorIndexError::AlreadyExists("i".into()));
        // The original binding is untouched.
        assert_eq!(reg.get("i").unwrap().label, "A");
        assert_eq!(reg.list().len(), 1);
    }

    #[test]
    fn if_not_exists_is_idempotent_and_keeps_the_original() {
        let mut reg = VectorIndexRegistry::new();
        reg.create(
            "i".into(),
            "A".into(),
            "e".into(),
            VectorIndexEntity::Node,
            false,
        )
        .unwrap();
        // Same name, different target, IF NOT EXISTS → no-op, returns the original.
        let got = reg
            .create(
                "i".into(),
                "B".into(),
                "other".into(),
                VectorIndexEntity::Node,
                true,
            )
            .unwrap();
        assert_eq!(got.label, "A");
        assert_eq!(got.property, "e");
        assert_eq!(reg.list().len(), 1);
    }

    #[test]
    fn drop_removes_and_reports_missing() {
        let mut reg = VectorIndexRegistry::new();
        reg.create(
            "i".into(),
            "A".into(),
            "e".into(),
            VectorIndexEntity::Node,
            false,
        )
        .unwrap();
        reg.remove("i", false).unwrap();
        assert!(reg.get("i").is_none());
        // Dropping again without IF EXISTS errors.
        assert_eq!(
            reg.remove("i", false).unwrap_err(),
            VectorIndexError::NotFound("i".into())
        );
        // …but IF EXISTS makes it a no-op.
        reg.remove("i", true).unwrap();
    }

    #[test]
    fn serde_round_trips_as_a_json_array() {
        let mut reg = VectorIndexRegistry::new();
        reg.create(
            "a".into(),
            "A".into(),
            "e".into(),
            VectorIndexEntity::Node,
            false,
        )
        .unwrap();
        reg.create(
            "b".into(),
            "B".into(),
            "f".into(),
            VectorIndexEntity::Node,
            false,
        )
        .unwrap();
        let json = serde_json::to_string(&reg).unwrap();
        let back: VectorIndexRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reg);
        // list() preserves creation order.
        assert_eq!(
            back.list()
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn sidecar_without_entity_loads_as_node_indexes() {
        // A semantic.json written before relationship indexes existed.
        let json = r#"{"indexes":[{"name":"i","label":"A","property":"e"}]}"#;
        let reg: VectorIndexRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(reg.get("i").unwrap().entity, VectorIndexEntity::Node);
    }

    #[test]
    fn relationship_entity_round_trips() {
        let mut reg = VectorIndexRegistry::new();
        reg.create(
            "r".into(),
            "SIMILAR".into(),
            "e".into(),
            VectorIndexEntity::Relationship,
            false,
        )
        .unwrap();
        let json = serde_json::to_string(&reg).unwrap();
        assert!(json.contains(r#""entity":"relationship""#), "{json}");
        let back: VectorIndexRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.get("r").unwrap().entity,
            VectorIndexEntity::Relationship
        );
    }
}
