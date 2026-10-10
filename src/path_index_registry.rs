//! Named path-index definitions (issue #578).
//!
//! `CREATE INDEX <name> FOR (n:Label) ON (n.meta.author, n.meta.*)` declares
//! which nested property paths the
//! [`NativePathIndex`](crate::native_path_index::NativePathIndex) maintains.
//! This registry holds those definitions under their names; it is persisted in
//! the `semantic.json` sidecar next to the vector-index registry (see
//! [`crate::native_service`]), so definitions survive a restart while the
//! index data itself is rebuilt from the WAL.

use serde::{Deserialize, Serialize};

use crate::native_path_index::{PathIndexSpec, PropertyPath};

/// One named path index: the paths it covers, for nodes with `label` (or all
/// nodes when `label` is `None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathIndex {
    /// The index name. Unique across path and vector indexes, like Neo4j.
    pub name: String,
    /// The label the index is restricted to, if any.
    pub label: Option<String>,
    /// The nested paths, in declaration order.
    pub paths: Vec<PropertyPath>,
}

impl PathIndex {
    /// The [`PathIndexSpec`]s this index contributes.
    pub fn specs(&self) -> impl Iterator<Item = PathIndexSpec> + '_ {
        self.paths.iter().map(|path| PathIndexSpec {
            label: self.label.clone(),
            path: path.clone(),
        })
    }
}

/// Errors from index DDL.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IndexError {
    /// `CREATE … INDEX` (without `IF NOT EXISTS`) used a taken name.
    #[error("an index named `{0}` already exists")]
    AlreadyExists(String),
    /// `DROP INDEX` (without `IF EXISTS`) named an unknown index.
    #[error("no index named `{0}`")]
    NotFound(String),
}

/// Registered path indexes, in creation order. A `Vec` so the sidecar form is
/// a plain JSON array.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathIndexRegistry {
    indexes: Vec<PathIndex>,
}

impl PathIndexRegistry {
    /// The index named `name`, if registered.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&PathIndex> {
        self.indexes.iter().find(|i| i.name == name)
    }

    /// Every registered index, in creation order.
    #[must_use]
    pub fn list(&self) -> &[PathIndex] {
        &self.indexes
    }

    /// Every spec of every registered index — what the path index maintains.
    #[must_use]
    pub fn specs(&self) -> Vec<PathIndexSpec> {
        let mut specs: Vec<PathIndexSpec> = Vec::new();
        for spec in self.indexes.iter().flat_map(PathIndex::specs) {
            if !specs.contains(&spec) {
                specs.push(spec);
            }
        }
        specs
    }

    /// Register `index`. The caller has checked the name is free.
    pub fn insert(&mut self, index: PathIndex) {
        self.indexes.push(index);
    }

    /// Remove the index named `name`; `true` if it existed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.indexes.len();
        self.indexes.retain(|i| i.name != name);
        self.indexes.len() != before
    }
}

/// The name Neo4j-style DDL gets when none is given:
/// `index_<label|all>_<paths>`, with every non-alphanumeric character as `_`
/// and a wildcard as `star` — e.g. `index_Bug_meta_severity`,
/// `index_all_star`.
#[must_use]
pub fn generated_name(label: Option<&str>, paths: &[PropertyPath]) -> String {
    let mut name = format!("index_{}", label.unwrap_or("all"));
    for path in paths {
        for segment in path.segments() {
            name.push('_');
            name.extend(
                segment
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '_' }),
            );
        }
        if path.is_wildcard() {
            name.push_str("_star");
        }
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(segments: &[&str], wildcard: bool) -> PropertyPath {
        PropertyPath::new(
            segments.iter().map(|s| (*s).to_string()).collect(),
            wildcard,
        )
        .expect("valid")
    }

    #[test]
    fn generated_names_are_readable_and_stable() {
        assert_eq!(
            generated_name(Some("Bug"), &[p(&["meta", "severity"], false)]),
            "index_Bug_meta_severity"
        );
        assert_eq!(generated_name(None, &[p(&[], true)]), "index_all_star");
        assert_eq!(
            generated_name(Some("Doc"), &[p(&["a b", "c"], false), p(&["meta"], true)]),
            "index_Doc_a_b_c_meta_star"
        );
    }

    #[test]
    fn specs_are_deduplicated_across_indexes() {
        let mut reg = PathIndexRegistry::default();
        reg.insert(PathIndex {
            name: "a".into(),
            label: Some("Bug".into()),
            paths: vec![p(&["meta"], true)],
        });
        reg.insert(PathIndex {
            name: "b".into(),
            label: Some("Bug".into()),
            paths: vec![p(&["meta"], true), p(&["x", "y"], false)],
        });
        assert_eq!(reg.specs().len(), 2);
        assert!(reg.remove("a"));
        assert!(!reg.remove("a"));
        assert_eq!(reg.list().len(), 1);
    }

    #[test]
    fn serde_round_trip() {
        let mut reg = PathIndexRegistry::default();
        reg.insert(PathIndex {
            name: "doc".into(),
            label: None,
            paths: vec![p(&["meta", "author"], false), p(&[], true)],
        });
        let json = serde_json::to_string(&reg).unwrap();
        let back: PathIndexRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reg);
    }
}
