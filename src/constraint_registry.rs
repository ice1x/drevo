//! Named node constraint definitions (issues #604, #606).
//!
//! `CREATE CONSTRAINT <name> FOR (n:Label) REQUIRE …` declares one of
//! Neo4j's node constraints:
//!
//! ```cypher
//! CREATE CONSTRAINT user_email FOR (n:User) REQUIRE n.email IS UNIQUE
//! CREATE CONSTRAINT book_id FOR (n:Book) REQUIRE (n.title, n.year) IS UNIQUE
//! CREATE CONSTRAINT user_name FOR (n:User) REQUIRE n.name IS NOT NULL
//! CREATE CONSTRAINT person_key FOR (n:Person) REQUIRE (n.first, n.last) IS NODE KEY
//! ```
//!
//! The graph enforces them ([`drevo_core::native::Constraint`]); this
//! registry keeps the names and is persisted in the `semantic.json` sidecar
//! next to the index definitions, so constraints survive a restart.

use serde::{Deserialize, Serialize};

/// What a constraint requires of the nodes carrying its label.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKind {
    /// `IS UNIQUE`: no two nodes share the value (or the tuple of values).
    #[default]
    Unique,
    /// `IS NOT NULL`: every node has the property.
    NotNull,
    /// `IS NODE KEY`: every node has all the properties, and no two share
    /// their combination.
    NodeKey,
}

impl ConstraintKind {
    /// The `type` column `SHOW CONSTRAINTS` reports, as in Neo4j 5.
    #[must_use]
    pub fn show_type(self) -> &'static str {
        match self {
            ConstraintKind::Unique => "UNIQUENESS",
            ConstraintKind::NotNull => "NODE_PROPERTY_EXISTENCE",
            ConstraintKind::NodeKey => "NODE_KEY",
        }
    }
}

/// One named node constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "StoredConstraint")]
pub struct SchemaConstraint {
    /// The constraint name, unique across constraints and indexes.
    pub name: String,
    /// The label whose nodes are constrained.
    pub label: String,
    /// What the constraint requires.
    pub kind: ConstraintKind,
    /// The constrained properties, in declaration order: one for `IS NOT
    /// NULL`, one or more otherwise.
    pub properties: Vec<String>,
}

/// The sidecar form: definitions written before composite constraints carry
/// a single `property` and no `kind`.
#[derive(Deserialize)]
struct StoredConstraint {
    name: String,
    label: String,
    #[serde(default)]
    kind: ConstraintKind,
    #[serde(default)]
    properties: Vec<String>,
    #[serde(default)]
    property: Option<String>,
}

impl From<StoredConstraint> for SchemaConstraint {
    fn from(s: StoredConstraint) -> Self {
        let mut properties = s.properties;
        if properties.is_empty() {
            properties.extend(s.property);
        }
        SchemaConstraint {
            name: s.name,
            label: s.label,
            kind: s.kind,
            properties,
        }
    }
}

impl SchemaConstraint {
    /// The engine-level constraint this definition installs.
    #[must_use]
    pub fn constraint(&self) -> drevo_core::native::Constraint {
        use drevo_core::native::Constraint;
        let kind = self.label.clone();
        match (self.kind, self.properties.as_slice()) {
            (ConstraintKind::Unique, [property]) => Constraint::UniqueNodeProperty {
                kind,
                property: property.clone(),
            },
            (ConstraintKind::Unique, _) => Constraint::UniqueNodeProperties {
                kind,
                properties: self.properties.clone(),
            },
            (ConstraintKind::NotNull, [property]) => Constraint::PropertyExists {
                kind,
                property: property.clone(),
            },
            // The parser allows a single property only; a hand-edited sidecar
            // with several is enforced as a node key would be on presence.
            (ConstraintKind::NotNull | ConstraintKind::NodeKey, _) => Constraint::NodeKey {
                kind,
                properties: self.properties.clone(),
            },
        }
    }

    /// Whether `other` declares the same rule (kind, label, properties).
    #[must_use]
    pub fn same_rule(&self, other: &SchemaConstraint) -> bool {
        self.kind == other.kind && self.label == other.label && self.properties == other.properties
    }
}

/// Errors from constraint DDL.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConstraintError {
    /// The name is taken by another constraint or an index.
    #[error("a constraint or index named `{0}` already exists")]
    AlreadyExists(String),
    /// The same rule is already declared under another name.
    #[error("an equivalent constraint already exists: `{0}`")]
    Equivalent(String),
    /// `DROP CONSTRAINT` (without `IF EXISTS`) named an unknown constraint.
    #[error("no constraint named `{0}`")]
    NotFound(String),
    /// Existing data already breaks the constraint, so it was not created.
    #[error("cannot create the constraint: {0}")]
    Violated(String),
}

/// Registered constraints, in creation order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConstraintRegistry {
    constraints: Vec<SchemaConstraint>,
}

impl ConstraintRegistry {
    /// The constraint named `name`, if registered.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&SchemaConstraint> {
        self.constraints.iter().find(|c| c.name == name)
    }

    /// The registered constraint declaring the same rule as `rule`, if any.
    #[must_use]
    pub fn equivalent(&self, rule: &SchemaConstraint) -> Option<&SchemaConstraint> {
        self.constraints.iter().find(|c| c.same_rule(rule))
    }

    /// Every registered constraint, in creation order.
    #[must_use]
    pub fn list(&self) -> &[SchemaConstraint] {
        &self.constraints
    }

    /// Register `constraint`. The caller has checked name and equivalence.
    pub fn insert(&mut self, constraint: SchemaConstraint) {
        self.constraints.push(constraint);
    }

    /// Remove the constraint named `name`, returning it.
    pub fn remove(&mut self, name: &str) -> Option<SchemaConstraint> {
        let at = self.constraints.iter().position(|c| c.name == name)?;
        Some(self.constraints.remove(at))
    }
}

/// The name `CREATE CONSTRAINT` gets when none is given:
/// `constraint_<label>_<properties…>`, then `_not_null` or `_node_key` for
/// those kinds; every non-alphanumeric character becomes `_`.
#[must_use]
pub fn generated_name(label: &str, kind: ConstraintKind, properties: &[String]) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect()
    };
    let mut name = format!("constraint_{}", clean(label));
    for p in properties {
        name.push('_');
        name.push_str(&clean(p));
    }
    match kind {
        ConstraintKind::Unique => {}
        ConstraintKind::NotNull => name.push_str("_not_null"),
        ConstraintKind::NodeKey => name.push_str("_node_key"),
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use drevo_core::native::Constraint;

    fn c(name: &str, label: &str, kind: ConstraintKind, properties: &[&str]) -> SchemaConstraint {
        SchemaConstraint {
            name: name.into(),
            label: label.into(),
            kind,
            properties: properties.iter().map(|p| (*p).to_string()).collect(),
        }
    }

    #[test]
    fn lookup_insert_remove() {
        let mut reg = ConstraintRegistry::default();
        reg.insert(c("email", "User", ConstraintKind::Unique, &["email"]));
        assert_eq!(reg.get("email").map(|c| c.label.as_str()), Some("User"));
        let same = c("other", "User", ConstraintKind::Unique, &["email"]);
        assert_eq!(
            reg.equivalent(&same).map(|c| c.name.as_str()),
            Some("email")
        );
        // Another kind on the same property is another rule.
        let not_null = c("nn", "User", ConstraintKind::NotNull, &["email"]);
        assert!(reg.equivalent(&not_null).is_none());
        assert!(reg.remove("email").is_some());
        assert!(reg.remove("email").is_none());
        assert!(reg.list().is_empty());
    }

    #[test]
    fn generated_names_are_readable() {
        let props = |p: &[&str]| p.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(
            generated_name("User", ConstraintKind::Unique, &props(&["email"])),
            "constraint_User_email"
        );
        assert_eq!(
            generated_name("My Label", ConstraintKind::Unique, &props(&["a.b"])),
            "constraint_My_Label_a_b"
        );
        assert_eq!(
            generated_name("User", ConstraintKind::NotNull, &props(&["email"])),
            "constraint_User_email_not_null"
        );
        assert_eq!(
            generated_name(
                "Person",
                ConstraintKind::NodeKey,
                &props(&["first", "last"])
            ),
            "constraint_Person_first_last_node_key"
        );
    }

    #[test]
    fn serde_round_trip() {
        let mut reg = ConstraintRegistry::default();
        reg.insert(c("email", "User", ConstraintKind::Unique, &["email"]));
        reg.insert(c(
            "key",
            "Person",
            ConstraintKind::NodeKey,
            &["first", "last"],
        ));
        let json = serde_json::to_string(&reg).unwrap();
        let back: ConstraintRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reg);
    }

    #[test]
    fn reads_single_property_definitions() {
        let json = r#"{"constraints":[{"name":"email","label":"User","property":"email"}]}"#;
        let reg: ConstraintRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(
            reg.list(),
            &[c("email", "User", ConstraintKind::Unique, &["email"])]
        );
    }

    #[test]
    fn installs_the_engine_constraint() {
        assert_eq!(
            c("email", "User", ConstraintKind::Unique, &["email"]).constraint(),
            Constraint::UniqueNodeProperty {
                kind: "User".into(),
                property: "email".into(),
            }
        );
        assert_eq!(
            c("b", "Book", ConstraintKind::Unique, &["title", "year"]).constraint(),
            Constraint::UniqueNodeProperties {
                kind: "Book".into(),
                properties: vec!["title".into(), "year".into()],
            }
        );
        assert_eq!(
            c("n", "User", ConstraintKind::NotNull, &["name"]).constraint(),
            Constraint::PropertyExists {
                kind: "User".into(),
                property: "name".into(),
            }
        );
        assert_eq!(
            c("k", "Person", ConstraintKind::NodeKey, &["id"]).constraint(),
            Constraint::NodeKey {
                kind: "Person".into(),
                properties: vec!["id".into()],
            }
        );
    }

    #[test]
    fn show_types_follow_neo4j() {
        assert_eq!(ConstraintKind::Unique.show_type(), "UNIQUENESS");
        assert_eq!(
            ConstraintKind::NotNull.show_type(),
            "NODE_PROPERTY_EXISTENCE"
        );
        assert_eq!(ConstraintKind::NodeKey.show_type(), "NODE_KEY");
    }
}
