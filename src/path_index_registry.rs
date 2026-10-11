//! Named path-index definitions (issues #578, #589).
//!
//! `CREATE INDEX <name> FOR (n:Label) ON (n.meta.author, n.meta.*)` declares
//! which nested property paths the
//! [`NativePathIndex`](crate::native_path_index::NativePathIndex) maintains;
//! `CREATE TEXT INDEX <name> FOR (n:Label) ON (n.title)` declares a property
//! the trigram [`NativeTextIndex`](crate::native_text_index::NativeTextIndex)
//! maintains. This registry holds those definitions under their names; it is persisted in
//! the `semantic.json` sidecar next to the vector-index registry (see
//! [`crate::native_service`]), so definitions survive a restart while the
//! index data itself is rebuilt from the WAL.

use serde::{Deserialize, Serialize};

use crate::native_path_index::{PathIndexSpec, PropertyPath};
use crate::native_text_index::TextIndexSpec;

/// What a [`PathIndex`] serves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexKind {
    /// Equality and numeric ranges on nested paths (`CREATE INDEX`).
    #[default]
    Range,
    /// `CONTAINS` / `STARTS WITH` / `ENDS WITH` on one property, through
    /// trigrams (`CREATE TEXT INDEX`).
    Text,
}

/// One named path index: the paths it covers, for nodes with `label` (or all
/// nodes when `label` is `None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathIndex {
    /// The index name. Unique across path and vector indexes, like Neo4j.
    pub name: String,
    /// What the index serves; definitions saved before text indexes existed
    /// are range indexes.
    #[serde(default)]
    pub kind: IndexKind,
    /// The label the index is restricted to, if any.
    pub label: Option<String>,
    /// The paths, in declaration order: nested ones for a range index, exactly
    /// one (top-level or nested, without a wildcard) for a text index.
    pub paths: Vec<PropertyPath>,
}

impl PathIndex {
    /// The [`PathIndexSpec`]s a range index contributes; none for a text index.
    pub fn specs(&self) -> impl Iterator<Item = PathIndexSpec> + '_ {
        self.paths
            .iter()
            .filter(|_| self.kind == IndexKind::Range)
            .map(|path| PathIndexSpec {
                label: self.label.clone(),
                path: path.clone(),
            })
    }

    /// The [`TextIndexSpec`]s a text index contributes; none for a range index.
    pub fn text_specs(&self) -> impl Iterator<Item = TextIndexSpec> + '_ {
        self.paths
            .iter()
            .filter(|path| self.kind == IndexKind::Text && !path.is_wildcard())
            .filter_map(|path| {
                TextIndexSpec::new(self.label.clone(), path.segments().to_vec()).ok()
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

    /// Every spec of every registered text index — what the text index
    /// maintains.
    #[must_use]
    pub fn text_specs(&self) -> Vec<TextIndexSpec> {
        let mut specs: Vec<TextIndexSpec> = Vec::new();
        for spec in self.indexes.iter().flat_map(PathIndex::text_specs) {
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
/// `index_all_star`. A text index gets a `text_` prefix:
/// `text_index_Ticket_title`.
#[must_use]
pub fn generated_name(kind: IndexKind, label: Option<&str>, paths: &[PropertyPath]) -> String {
    let prefix = match kind {
        IndexKind::Range => "",
        IndexKind::Text => "text_",
    };
    let mut name = format!("{prefix}index_{}", label.unwrap_or("all"));
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
            generated_name(
                IndexKind::Range,
                Some("Bug"),
                &[p(&["meta", "severity"], false)]
            ),
            "index_Bug_meta_severity"
        );
        assert_eq!(
            generated_name(IndexKind::Range, None, &[p(&[], true)]),
            "index_all_star"
        );
        assert_eq!(
            generated_name(
                IndexKind::Range,
                Some("Doc"),
                &[p(&["a b", "c"], false), p(&["meta"], true)]
            ),
            "index_Doc_a_b_c_meta_star"
        );
        assert_eq!(
            generated_name(IndexKind::Text, Some("Ticket"), &[p(&["title"], false)]),
            "text_index_Ticket_title"
        );
    }

    #[test]
    fn specs_are_deduplicated_across_indexes() {
        let mut reg = PathIndexRegistry::default();
        reg.insert(PathIndex {
            name: "a".into(),
            kind: IndexKind::Range,
            label: Some("Bug".into()),
            paths: vec![p(&["meta"], true)],
        });
        reg.insert(PathIndex {
            name: "b".into(),
            kind: IndexKind::Range,
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
            kind: IndexKind::Range,
            label: None,
            paths: vec![p(&["meta", "author"], false), p(&[], true)],
        });
        reg.insert(PathIndex {
            name: "titles".into(),
            kind: IndexKind::Text,
            label: Some("Doc".into()),
            paths: vec![p(&["title"], false)],
        });
        let json = serde_json::to_string(&reg).unwrap();
        let back: PathIndexRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reg);
    }

    #[test]
    fn definitions_without_a_kind_load_as_range_indexes() {
        let json = r#"{"indexes":[{"name":"old","label":null,"paths":[{"segments":["meta","a"],"wildcard":false}]}]}"#;
        let reg: PathIndexRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(reg.list()[0].kind, IndexKind::Range);
        assert_eq!(reg.specs().len(), 1);
        assert!(reg.text_specs().is_empty());
    }

    #[test]
    fn range_and_text_specs_are_kept_apart() {
        let mut reg = PathIndexRegistry::default();
        reg.insert(PathIndex {
            name: "range".into(),
            kind: IndexKind::Range,
            label: None,
            paths: vec![p(&["meta", "a"], false)],
        });
        reg.insert(PathIndex {
            name: "text".into(),
            kind: IndexKind::Text,
            label: Some("Doc".into()),
            paths: vec![p(&["title"], false)],
        });
        assert_eq!(reg.specs().len(), 1);
        let text = reg.text_specs();
        assert_eq!(text.len(), 1);
        assert_eq!(text[0].label(), Some("Doc"));
        assert_eq!(text[0].path(), ["title".to_string()]);
    }
}
