//! Named UNIQUE constraint definitions (issue #604).
//!
//! `CREATE CONSTRAINT <name> FOR (n:Label) REQUIRE n.prop IS UNIQUE` declares
//! that no two nodes with the label share a value of the property. The graph
//! enforces it ([`drevo_core::native::Constraint::UniqueNodeProperty`]); this
//! registry keeps the names and is persisted in the `semantic.json` sidecar
//! next to the index definitions, so constraints survive a restart.

use serde::{Deserialize, Serialize};

/// One named UNIQUE constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UniqueConstraint {
    /// The constraint name, unique across constraints and indexes.
    pub name: String,
    /// The label whose nodes are constrained.
    pub label: String,
    /// The property whose values must be unique.
    pub property: String,
}

impl UniqueConstraint {
    /// The engine-level constraint this definition installs.
    #[must_use]
    pub fn constraint(&self) -> drevo_core::native::Constraint {
        drevo_core::native::Constraint::UniqueNodeProperty {
            kind: self.label.clone(),
            property: self.property.clone(),
        }
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
    constraints: Vec<UniqueConstraint>,
}

impl ConstraintRegistry {
    /// The constraint named `name`, if registered.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&UniqueConstraint> {
        self.constraints.iter().find(|c| c.name == name)
    }

    /// The constraint declaring `label` / `property` unique, if any.
    #[must_use]
    pub fn equivalent(&self, label: &str, property: &str) -> Option<&UniqueConstraint> {
        self.constraints
            .iter()
            .find(|c| c.label == label && c.property == property)
    }

    /// Every registered constraint, in creation order.
    #[must_use]
    pub fn list(&self) -> &[UniqueConstraint] {
        &self.constraints
    }

    /// Register `constraint`. The caller has checked name and equivalence.
    pub fn insert(&mut self, constraint: UniqueConstraint) {
        self.constraints.push(constraint);
    }

    /// Remove the constraint named `name`, returning it.
    pub fn remove(&mut self, name: &str) -> Option<UniqueConstraint> {
        let at = self.constraints.iter().position(|c| c.name == name)?;
        Some(self.constraints.remove(at))
    }
}

/// The name `CREATE CONSTRAINT` gets when none is given:
/// `constraint_<label>_<property>`, every non-alphanumeric character as `_`.
#[must_use]
pub fn generated_name(label: &str, property: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect()
    };
    format!("constraint_{}_{}", clean(label), clean(property))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, label: &str, property: &str) -> UniqueConstraint {
        UniqueConstraint {
            name: name.into(),
            label: label.into(),
            property: property.into(),
        }
    }

    #[test]
    fn lookup_insert_remove() {
        let mut reg = ConstraintRegistry::default();
        reg.insert(c("email", "User", "email"));
        assert_eq!(reg.get("email").map(|c| c.label.as_str()), Some("User"));
        assert_eq!(
            reg.equivalent("User", "email").map(|c| c.name.as_str()),
            Some("email")
        );
        assert!(reg.equivalent("User", "name").is_none());
        assert!(reg.remove("email").is_some());
        assert!(reg.remove("email").is_none());
        assert!(reg.list().is_empty());
    }

    #[test]
    fn generated_names_are_readable() {
        assert_eq!(generated_name("User", "email"), "constraint_User_email");
        assert_eq!(generated_name("My Label", "a.b"), "constraint_My_Label_a_b");
    }

    #[test]
    fn serde_round_trip() {
        let mut reg = ConstraintRegistry::default();
        reg.insert(c("email", "User", "email"));
        let json = serde_json::to_string(&reg).unwrap();
        let back: ConstraintRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reg);
    }

    #[test]
    fn installs_the_engine_constraint() {
        assert_eq!(
            c("email", "User", "email").constraint(),
            drevo_core::native::Constraint::UniqueNodeProperty {
                kind: "User".into(),
                property: "email".into(),
            }
        );
    }
}
