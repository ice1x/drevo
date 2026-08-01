//! Semantic-index state machine (Phase 21, task `00218`… control plane).
//!
//! A per-`(label, embedding property)` state machine that governs whether — and
//! how — nodes of a label are turned into vectors for semantic search. It is
//! **off by default**: until an operator calls `enable`, nothing is embedded
//! and no outbound embedding calls happen.
//!
//! This module is the pure, dependency-free **control plane** — the state, its
//! legal transitions, and a registry of targets, all serialisable so a later
//! slice can persist it in redb and drive it from `drevo.embeddings.*` Cypher
//! procedures. It performs no embedding and touches no storage itself.
//!
//! # State machine
//!
//! ```text
//! Disabled ──enable(mode)──► Enabled{mode} ──begin_reindex──► Reindexing
//!    ▲  ▲                        │                                │
//!    │  └────────disable─────────┘         finish_reindex ────────┘
//!    └────drop──── (removes the target entirely)
//! ```
//!
//! `mode ∈ {Manual, Auto}` is an option of an *enabled* target, switchable live
//! via `set_mode`:
//!
//! - [`IndexMode::Manual`](crate::semantic_index::IndexMode::Manual) — embeddings
//!   are produced only by an explicit reindex; writes stay pure.
//! - [`IndexMode::Auto`](crate::semantic_index::IndexMode::Auto) — a write that
//!   changes the embedded text additionally marks the node dirty for a
//!   background worker (Phase 2). The control plane is identical; only the
//!   write-path behaviour differs.
//!
//! # Data semantics (enforced by later slices, documented here)
//!
//! - `disable` is **control-plane only** — stored vectors are kept.
//! - `drop` is the **explicit, destructive** removal of a target's vectors.
//! - A `SET` that changes the embedded text drops the now-stale vector and marks
//!   the node dirty (hash-precise); an edit to unrelated properties costs
//!   nothing. So the search path never serves a stale vector.

use serde::{Deserialize, Serialize};

/// How embeddings are produced for an enabled target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexMode {
    /// Embeddings are produced only by an explicit reindex. Writes stay pure.
    Manual,
    /// A write that changes the embedded text also enqueues the node for a
    /// background worker (Phase 2). Same control plane as [`IndexMode::Manual`].
    Auto,
}

impl IndexMode {
    /// Parse a mode name (`"manual"` / `"auto"`, case-insensitive) as accepted
    /// by the `drevo.embeddings.*` procedures.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::BadMode`] for any other value.
    pub fn parse(s: &str) -> Result<Self, IndexError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "manual" => Ok(Self::Manual),
            "auto" => Ok(Self::Auto),
            other => Err(IndexError::BadMode(other.to_string())),
        }
    }

    /// The canonical lowercase name, matching the wire form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }
}

/// Lifecycle state of a semantic index for one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexState {
    /// Not indexing — nothing is embedded (the default for every target).
    Disabled,
    /// Indexing is on; new embeddings are produced per the target's mode.
    Enabled,
    /// A full reindex is in progress (a transient sub-state of `Enabled`).
    Reindexing,
}

impl IndexState {
    /// The canonical lowercase name (matches `status()` output).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Enabled => "enabled",
            Self::Reindexing => "reindexing",
        }
    }
}

/// One semantic-index target: a `(label, embedding property)` pair and its
/// configuration + lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticIndex {
    /// Node label this target indexes (e.g. `Entity`).
    pub label: String,
    /// Property holding the source text to embed (e.g. `body`).
    pub text_property: String,
    /// Property the produced vector is stored in (e.g. `embedding`).
    pub embedding_property: String,
    /// Current lifecycle state.
    pub state: IndexState,
    /// Embedding mode (meaningful while `Enabled`/`Reindexing`; retained across
    /// `disable` so re-`enable` keeps the operator's choice).
    pub mode: IndexMode,
    /// Optional embedding-model override; `None` uses the server default.
    pub model: Option<String>,
}

impl SemanticIndex {
    /// True while embeddings should be produced (either steady-state or during
    /// a reindex).
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self.state, IndexState::Enabled | IndexState::Reindexing)
    }
}

/// Errors from control-plane operations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IndexError {
    /// A target for this `(label, property)` is already enabled.
    #[error("semantic index for ({label}, {property}) is already enabled")]
    AlreadyEnabled {
        /// The node label.
        label: String,
        /// The embedding property.
        property: String,
    },
    /// No target exists for this `(label, property)`.
    #[error("no semantic index for ({label}, {property})")]
    NotFound {
        /// The node label.
        label: String,
        /// The embedding property.
        property: String,
    },
    /// The requested transition is not legal from the current state.
    #[error("cannot {action} a semantic index that is {from}")]
    InvalidTransition {
        /// The action attempted (e.g. `set_mode`, `disable`).
        action: &'static str,
        /// The state it was attempted from.
        from: &'static str,
    },
    /// A mode string other than `manual` / `auto`.
    #[error("unknown embedding mode `{0}` (expected `manual` or `auto`)")]
    BadMode(String),
}

/// A registry of semantic-index targets, keyed by `(label, embedding property)`.
///
/// This is the unit a later slice persists in redb and exposes through the
/// `drevo.embeddings.*` procedures. Empty by default — the whole subsystem is
/// off until something is `enable`d.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticIndexRegistry {
    // A Vec (not a map) so the serialised form is a plain JSON array — a
    // `(String, String)` map key would not round-trip through JSON. N is tiny
    // (a handful of targets), so linear lookup is fine.
    targets: Vec<SemanticIndex>,
}

impl SemanticIndexRegistry {
    /// An empty registry (the default — nothing indexed).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn position(&self, label: &str, property: &str) -> Option<usize> {
        self.targets
            .iter()
            .position(|t| t.label == label && t.embedding_property == property)
    }

    /// Look up a target by `(label, embedding property)`.
    #[must_use]
    pub fn get(&self, label: &str, property: &str) -> Option<&SemanticIndex> {
        self.position(label, property).map(|i| &self.targets[i])
    }

    /// All targets, in insertion order — the source for `status()`.
    #[must_use]
    pub fn list(&self) -> &[SemanticIndex] {
        &self.targets
    }

    /// Enable semantic indexing for `(label, embedding_property)`.
    ///
    /// Creates the target (or re-enables a previously `Disabled` one, updating
    /// its configuration). The state becomes [`IndexState::Enabled`].
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::AlreadyEnabled`] if a target for this pair is
    /// already active (use `set_mode` / `reindex` instead).
    pub fn enable(
        &mut self,
        label: &str,
        text_property: &str,
        embedding_property: &str,
        mode: IndexMode,
        model: Option<String>,
    ) -> Result<&SemanticIndex, IndexError> {
        match self.position(label, embedding_property) {
            Some(i) if self.targets[i].is_active() => Err(IndexError::AlreadyEnabled {
                label: label.to_string(),
                property: embedding_property.to_string(),
            }),
            Some(i) => {
                let t = &mut self.targets[i];
                t.text_property = text_property.to_string();
                t.state = IndexState::Enabled;
                t.mode = mode;
                t.model = model;
                Ok(&self.targets[i])
            }
            None => {
                self.targets.push(SemanticIndex {
                    label: label.to_string(),
                    text_property: text_property.to_string(),
                    embedding_property: embedding_property.to_string(),
                    state: IndexState::Enabled,
                    mode,
                    model,
                });
                let i = self.targets.len() - 1;
                Ok(&self.targets[i])
            }
        }
    }

    fn get_mut(&mut self, label: &str, property: &str) -> Result<&mut SemanticIndex, IndexError> {
        match self.position(label, property) {
            Some(i) => Ok(&mut self.targets[i]),
            None => Err(IndexError::NotFound {
                label: label.to_string(),
                property: property.to_string(),
            }),
        }
    }

    /// Stop indexing a target **without discarding its vectors** (control-plane
    /// only). Idempotent on an already-disabled target.
    ///
    /// # Errors
    ///
    /// [`IndexError::NotFound`] if the target is unknown, or
    /// [`IndexError::InvalidTransition`] while a reindex is in progress
    /// (finish or drop it first).
    pub fn disable(&mut self, label: &str, property: &str) -> Result<(), IndexError> {
        let t = self.get_mut(label, property)?;
        if t.state == IndexState::Reindexing {
            return Err(IndexError::InvalidTransition {
                action: "disable",
                from: IndexState::Reindexing.as_str(),
            });
        }
        t.state = IndexState::Disabled;
        Ok(())
    }

    /// Switch an enabled target's [`IndexMode`] live.
    ///
    /// # Errors
    ///
    /// [`IndexError::NotFound`], or [`IndexError::InvalidTransition`] if the
    /// target is `Disabled` (enable it first).
    pub fn set_mode(
        &mut self,
        label: &str,
        property: &str,
        mode: IndexMode,
    ) -> Result<(), IndexError> {
        let t = self.get_mut(label, property)?;
        if t.state == IndexState::Disabled {
            return Err(IndexError::InvalidTransition {
                action: "set_mode",
                from: IndexState::Disabled.as_str(),
            });
        }
        t.mode = mode;
        Ok(())
    }

    /// Transition an enabled target into [`IndexState::Reindexing`].
    ///
    /// # Errors
    ///
    /// [`IndexError::NotFound`], or [`IndexError::InvalidTransition`] if the
    /// target is `Disabled` or already `Reindexing`.
    pub fn begin_reindex(&mut self, label: &str, property: &str) -> Result<(), IndexError> {
        let t = self.get_mut(label, property)?;
        match t.state {
            IndexState::Enabled => {
                t.state = IndexState::Reindexing;
                Ok(())
            }
            other => Err(IndexError::InvalidTransition {
                action: "reindex",
                from: other.as_str(),
            }),
        }
    }

    /// Return a reindexing target to [`IndexState::Enabled`].
    ///
    /// # Errors
    ///
    /// [`IndexError::NotFound`], or [`IndexError::InvalidTransition`] if the
    /// target was not `Reindexing`.
    pub fn finish_reindex(&mut self, label: &str, property: &str) -> Result<(), IndexError> {
        let t = self.get_mut(label, property)?;
        if t.state != IndexState::Reindexing {
            return Err(IndexError::InvalidTransition {
                action: "finish_reindex",
                from: t.state.as_str(),
            });
        }
        t.state = IndexState::Enabled;
        Ok(())
    }

    /// Remove a target from the registry entirely (the control-plane half of
    /// the destructive `drop` — the caller removes the vectors separately).
    /// Allowed from any state.
    ///
    /// # Errors
    ///
    /// [`IndexError::NotFound`] if the target is unknown.
    pub fn drop_target(
        &mut self,
        label: &str,
        property: &str,
    ) -> Result<SemanticIndex, IndexError> {
        match self.position(label, property) {
            Some(i) => Ok(self.targets.remove(i)),
            None => Err(IndexError::NotFound {
                label: label.to_string(),
                property: property.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> SemanticIndexRegistry {
        SemanticIndexRegistry::new()
    }

    #[test]
    fn default_registry_is_empty() {
        assert!(reg().list().is_empty());
    }

    #[test]
    fn mode_parse_and_roundtrip() {
        assert_eq!(IndexMode::parse("manual").unwrap(), IndexMode::Manual);
        assert_eq!(IndexMode::parse("  AUTO ").unwrap(), IndexMode::Auto);
        assert_eq!(IndexMode::Manual.as_str(), "manual");
        assert!(matches!(
            IndexMode::parse("semantic"),
            Err(IndexError::BadMode(_))
        ));
    }

    #[test]
    fn enable_creates_an_active_target_with_config() {
        let mut r = reg();
        let t = r
            .enable("Entity", "body", "embedding", IndexMode::Manual, None)
            .unwrap();
        assert_eq!(t.label, "Entity");
        assert_eq!(t.text_property, "body");
        assert_eq!(t.embedding_property, "embedding");
        assert_eq!(t.state, IndexState::Enabled);
        assert_eq!(t.mode, IndexMode::Manual);
        assert!(t.is_active());
        assert_eq!(r.list().len(), 1);
    }

    #[test]
    fn enable_twice_while_active_is_rejected() {
        let mut r = reg();
        r.enable("Entity", "body", "embedding", IndexMode::Manual, None)
            .unwrap();
        let err = r
            .enable("Entity", "body", "embedding", IndexMode::Auto, None)
            .unwrap_err();
        assert!(matches!(err, IndexError::AlreadyEnabled { .. }));
    }

    #[test]
    fn disable_keeps_the_target_and_re_enable_reconfigures() {
        let mut r = reg();
        r.enable("Entity", "body", "embedding", IndexMode::Auto, None)
            .unwrap();
        r.disable("Entity", "embedding").unwrap();
        assert_eq!(
            r.get("Entity", "embedding").unwrap().state,
            IndexState::Disabled
        );
        // Still present (control-plane only) — re-enable updates config.
        r.enable(
            "Entity",
            "title",
            "embedding",
            IndexMode::Manual,
            Some("m".into()),
        )
        .unwrap();
        let t = r.get("Entity", "embedding").unwrap();
        assert_eq!(t.state, IndexState::Enabled);
        assert_eq!(t.text_property, "title");
        assert_eq!(t.mode, IndexMode::Manual);
        assert_eq!(t.model.as_deref(), Some("m"));
        assert_eq!(r.list().len(), 1);
    }

    #[test]
    fn set_mode_switches_live_but_not_while_disabled() {
        let mut r = reg();
        r.enable("Entity", "body", "embedding", IndexMode::Manual, None)
            .unwrap();
        r.set_mode("Entity", "embedding", IndexMode::Auto).unwrap();
        assert_eq!(r.get("Entity", "embedding").unwrap().mode, IndexMode::Auto);
        r.disable("Entity", "embedding").unwrap();
        let err = r
            .set_mode("Entity", "embedding", IndexMode::Manual)
            .unwrap_err();
        assert!(matches!(
            err,
            IndexError::InvalidTransition {
                action: "set_mode",
                ..
            }
        ));
    }

    #[test]
    fn reindex_lifecycle() {
        let mut r = reg();
        r.enable("Entity", "body", "embedding", IndexMode::Manual, None)
            .unwrap();
        r.begin_reindex("Entity", "embedding").unwrap();
        assert_eq!(
            r.get("Entity", "embedding").unwrap().state,
            IndexState::Reindexing
        );
        // Can't begin twice, and can't disable mid-reindex.
        assert!(r.begin_reindex("Entity", "embedding").is_err());
        assert!(matches!(
            r.disable("Entity", "embedding").unwrap_err(),
            IndexError::InvalidTransition {
                action: "disable",
                ..
            }
        ));
        r.finish_reindex("Entity", "embedding").unwrap();
        assert_eq!(
            r.get("Entity", "embedding").unwrap().state,
            IndexState::Enabled
        );
    }

    #[test]
    fn begin_reindex_requires_enabled() {
        let mut r = reg();
        r.enable("Entity", "body", "embedding", IndexMode::Manual, None)
            .unwrap();
        r.disable("Entity", "embedding").unwrap();
        assert!(matches!(
            r.begin_reindex("Entity", "embedding").unwrap_err(),
            IndexError::InvalidTransition {
                action: "reindex",
                from: "disabled"
            }
        ));
    }

    #[test]
    fn finish_reindex_requires_reindexing() {
        let mut r = reg();
        r.enable("Entity", "body", "embedding", IndexMode::Manual, None)
            .unwrap();
        assert!(matches!(
            r.finish_reindex("Entity", "embedding").unwrap_err(),
            IndexError::InvalidTransition {
                action: "finish_reindex",
                ..
            }
        ));
    }

    #[test]
    fn drop_removes_the_target_from_any_state() {
        let mut r = reg();
        r.enable("Entity", "body", "embedding", IndexMode::Manual, None)
            .unwrap();
        r.begin_reindex("Entity", "embedding").unwrap();
        let removed = r.drop_target("Entity", "embedding").unwrap();
        assert_eq!(removed.label, "Entity");
        assert!(r.get("Entity", "embedding").is_none());
        assert!(r.list().is_empty());
    }

    #[test]
    fn operations_on_unknown_target_are_not_found() {
        let mut r = reg();
        assert!(matches!(
            r.disable("X", "embedding").unwrap_err(),
            IndexError::NotFound { .. }
        ));
        assert!(matches!(
            r.set_mode("X", "embedding", IndexMode::Auto).unwrap_err(),
            IndexError::NotFound { .. }
        ));
        assert!(matches!(
            r.drop_target("X", "embedding").unwrap_err(),
            IndexError::NotFound { .. }
        ));
    }

    #[test]
    fn distinct_embedding_properties_are_independent_targets() {
        let mut r = reg();
        r.enable("Entity", "body", "embedding", IndexMode::Manual, None)
            .unwrap();
        r.enable("Entity", "title", "title_vec", IndexMode::Auto, None)
            .unwrap();
        assert_eq!(r.list().len(), 2);
        r.disable("Entity", "embedding").unwrap();
        assert_eq!(
            r.get("Entity", "embedding").unwrap().state,
            IndexState::Disabled
        );
        assert_eq!(
            r.get("Entity", "title_vec").unwrap().state,
            IndexState::Enabled
        );
    }

    #[test]
    fn registry_json_roundtrips() {
        let mut r = reg();
        r.enable(
            "Entity",
            "body",
            "embedding",
            IndexMode::Auto,
            Some("m".into()),
        )
        .unwrap();
        r.begin_reindex("Entity", "embedding").unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let back: SemanticIndexRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        // Mode/state serialise as their lowercase wire names.
        assert!(json.contains("\"auto\""));
        assert!(json.contains("\"reindexing\""));
    }
}
