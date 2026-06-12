//! [`AccessPolicy`] — the policy engine that ties roles, subject→role
//! assignments, and a request together into an authorization [`Decision`].
//!
//! The engine resolves a subject's *effective roles* (its directly-assigned
//! roles plus everything they transitively inherit), collects the
//! [`Permission`]s those roles grant, and applies **deny-overrides**
//! evaluation: a matching [`Effect::Deny`] refuses
//! the request, otherwise a matching
//! [`Effect::Allow`] grants it, otherwise the
//! request is denied by default (closed-world).
//!
//! Inheritance resolution is cycle-safe — a `a → b → a` loop is broken by a
//! visited set so evaluation always terminates — and unknown parent / role
//! names are tolerated at evaluation time. [`AccessPolicy::validate`] reports
//! both kinds of structural defect up front for callers that want to fail
//! loudly at configuration time.

use std::collections::{BTreeMap, BTreeSet};

use super::permission::{Action, Effect, Permission, Resource};
use super::role::Role;

/// An error from configuring or validating an [`AccessPolicy`].
///
/// Kept in its own channel rather than lifted into the crate-wide
/// [`crate::error::DrevoError`]: like the planner and MVCC engines, the
/// authorization layer is a standalone mechanism that the executor / server
/// wiring will consume, so it owns its own recoverable error type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthzError {
    /// A role with this name is already defined and
    /// [`define_role`](AccessPolicy::define_role) refuses to clobber it. Use
    /// [`upsert_role`](AccessPolicy::upsert_role) to replace deliberately.
    #[error("role already defined: {0}")]
    DuplicateRole(String),

    /// An operation referenced a role name that is not registered.
    #[error("unknown role: {0}")]
    UnknownRole(String),

    /// Role inheritance forms a cycle reachable from the named role, so its
    /// effective permission set is not well-founded. Reported only by
    /// [`validate`](AccessPolicy::validate); evaluation breaks cycles silently.
    #[error("role inheritance cycle through: {0}")]
    RoleCycle(String),
}

/// Why a request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// No effective permission matched the request at all — the closed-world
    /// default.
    NoMatchingGrant,
    /// A matching [`Effect::Deny`] permission
    /// refused the request, overriding any matching allow.
    ExplicitDeny,
}

/// The outcome of evaluating a request against an [`AccessPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The request is permitted.
    Allow,
    /// The request is refused, with the reason it was refused.
    Deny(DenyReason),
}

impl Decision {
    /// Whether the decision permits the request.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow)
    }
}

/// The RBAC policy engine: a registry of [`Role`]s plus the map of which roles
/// each subject holds.
///
/// A *subject* is any opaque identity string — typically the username the
/// authentication layer (Bolt [`UserStore`](crate::bolt::auth::UserStore))
/// resolved from a session token. The policy answers, for a `(subject, action,
/// resource)` triple, whether the action is permitted.
#[derive(Debug, Clone, Default)]
pub struct AccessPolicy {
    roles: BTreeMap<String, Role>,
    assignments: BTreeMap<String, BTreeSet<String>>,
}

impl AccessPolicy {
    /// An empty policy: no roles, no assignments. With nothing defined every
    /// request is denied by default.
    pub fn new() -> Self {
        AccessPolicy::default()
    }

    /// Register a new role.
    ///
    /// # Errors
    /// [`AuthzError::DuplicateRole`] if a role with the same name already
    /// exists — use [`upsert_role`](AccessPolicy::upsert_role) to replace.
    pub fn define_role(&mut self, role: Role) -> Result<(), AuthzError> {
        if self.roles.contains_key(role.name()) {
            return Err(AuthzError::DuplicateRole(role.name().to_string()));
        }
        self.roles.insert(role.name().to_string(), role);
        Ok(())
    }

    /// Register a role, replacing any existing role of the same name.
    pub fn upsert_role(&mut self, role: Role) {
        self.roles.insert(role.name().to_string(), role);
    }

    /// Look up a registered role by name.
    pub fn role(&self, name: &str) -> Option<&Role> {
        self.roles.get(name)
    }

    /// The names of all registered roles, sorted.
    pub fn role_names(&self) -> Vec<&str> {
        self.roles.keys().map(String::as_str).collect()
    }

    /// Assign `role_name` to `subject`.
    ///
    /// # Errors
    /// [`AuthzError::UnknownRole`] if no role with that name is registered.
    pub fn assign_role(
        &mut self,
        subject: impl Into<String>,
        role_name: impl Into<String>,
    ) -> Result<(), AuthzError> {
        let role_name = role_name.into();
        if !self.roles.contains_key(&role_name) {
            return Err(AuthzError::UnknownRole(role_name));
        }
        self.assignments
            .entry(subject.into())
            .or_default()
            .insert(role_name);
        Ok(())
    }

    /// Remove a directly-assigned role from `subject`. Returns whether the
    /// assignment existed.
    pub fn revoke_role(&mut self, subject: &str, role_name: &str) -> bool {
        match self.assignments.get_mut(subject) {
            Some(set) => set.remove(role_name),
            None => false,
        }
    }

    /// The roles assigned *directly* to `subject`, sorted. Does not include
    /// inherited roles — see [`effective_roles`](AccessPolicy::effective_roles).
    pub fn assigned_roles(&self, subject: &str) -> Vec<&str> {
        self.assignments
            .get(subject)
            .into_iter()
            .flat_map(|set| set.iter().map(String::as_str))
            .collect()
    }

    /// The full set of role names in effect for `subject`: every
    /// directly-assigned role plus everything those roles transitively inherit.
    ///
    /// Resolution is cycle-safe (a visited set breaks inheritance loops) and
    /// skips parent names that are not registered. Only registered role names
    /// appear in the result.
    pub fn effective_roles(&self, subject: &str) -> BTreeSet<String> {
        let mut effective = BTreeSet::new();
        let mut stack: Vec<String> = self
            .assignments
            .get(subject)
            .into_iter()
            .flat_map(|set| set.iter().cloned())
            .collect();
        while let Some(name) = stack.pop() {
            // Only registered roles count, and each is visited once.
            let Some(role) = self.roles.get(&name) else {
                continue;
            };
            if !effective.insert(name) {
                continue;
            }
            for parent in role.parents() {
                if !effective.contains(parent) {
                    stack.push(parent.clone());
                }
            }
        }
        effective
    }

    /// Every [`Permission`] in effect for `subject`, gathered from all of its
    /// [`effective_roles`](AccessPolicy::effective_roles). Iteration order is
    /// deterministic (roles visited in sorted-name order). Useful for auditing
    /// a subject's full capability set.
    pub fn effective_permissions(&self, subject: &str) -> Vec<Permission> {
        let mut perms = Vec::new();
        for name in self.effective_roles(subject) {
            if let Some(role) = self.roles.get(&name) {
                perms.extend(role.permissions().iter().cloned());
            }
        }
        perms
    }

    /// Evaluate whether `subject` may perform `action` against `resource`,
    /// returning a full [`Decision`] (including the [`DenyReason`] on refusal).
    ///
    /// Deny-overrides: any matching [`Effect::Deny`]
    /// refuses; otherwise a matching
    /// [`Effect::Allow`] grants; otherwise the
    /// request is denied with [`DenyReason::NoMatchingGrant`].
    pub fn evaluate(&self, subject: &str, action: Action, resource: &Resource) -> Decision {
        let mut allowed = false;
        for name in self.effective_roles(subject) {
            let Some(role) = self.roles.get(&name) else {
                continue;
            };
            for p in role.permissions() {
                if p.applies_to(action, resource) {
                    match p.effect {
                        Effect::Deny => return Decision::Deny(DenyReason::ExplicitDeny),
                        Effect::Allow => allowed = true,
                    }
                }
            }
        }
        if allowed {
            Decision::Allow
        } else {
            Decision::Deny(DenyReason::NoMatchingGrant)
        }
    }

    /// Convenience boolean wrapper over [`evaluate`](AccessPolicy::evaluate).
    pub fn is_allowed(&self, subject: &str, action: Action, resource: &Resource) -> bool {
        self.evaluate(subject, action, resource).is_allowed()
    }

    /// Check the policy's structural integrity.
    ///
    /// # Errors
    /// * [`AuthzError::UnknownRole`] — a registered role inherits a parent
    ///   name that is not itself registered.
    /// * [`AuthzError::RoleCycle`] — role inheritance forms a cycle.
    ///
    /// Evaluation tolerates both defects (unknown parents are skipped, cycles
    /// are broken), so `validate` is an opt-in configuration-time guard for
    /// callers that prefer to fail fast.
    pub fn validate(&self) -> Result<(), AuthzError> {
        for role in self.roles.values() {
            for parent in role.parents() {
                if !self.roles.contains_key(parent) {
                    return Err(AuthzError::UnknownRole(parent.clone()));
                }
            }
        }
        // Cycle detection via DFS with a recursion stack.
        let mut visited: BTreeSet<&str> = BTreeSet::new();
        for start in self.roles.keys() {
            let mut on_stack: BTreeSet<&str> = BTreeSet::new();
            if self.has_cycle(start, &mut visited, &mut on_stack) {
                return Err(AuthzError::RoleCycle(start.clone()));
            }
        }
        Ok(())
    }

    /// DFS helper for [`validate`](AccessPolicy::validate). Returns true if a
    /// cycle is reachable from `name`. All parents are known to exist here
    /// because the dangling-parent check runs first.
    fn has_cycle<'a>(
        &'a self,
        name: &'a str,
        visited: &mut BTreeSet<&'a str>,
        on_stack: &mut BTreeSet<&'a str>,
    ) -> bool {
        if on_stack.contains(name) {
            return true;
        }
        if visited.contains(name) {
            return false;
        }
        visited.insert(name);
        on_stack.insert(name);
        if let Some(role) = self.roles.get(name) {
            for parent in role.parents() {
                // parent is registered (validate checked) — key back-reference
                let parent_key = self
                    .roles
                    .get_key_value(parent)
                    .map(|(k, _)| k.as_str())
                    .unwrap_or(parent.as_str());
                if self.has_cycle(parent_key, visited, on_stack) {
                    return true;
                }
            }
        }
        on_stack.remove(name);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::super::permission::Scope;
    use super::*;

    fn task(kind: &str) -> Resource {
        Resource::kind(kind)
    }

    #[test]
    fn empty_policy_denies_by_default() {
        let policy = AccessPolicy::new();
        let d = policy.evaluate("alice", Action::ReadNode, &task("Task"));
        assert_eq!(d, Decision::Deny(DenyReason::NoMatchingGrant));
        assert!(!d.is_allowed());
    }

    #[test]
    fn define_role_rejects_duplicates() {
        let mut policy = AccessPolicy::new();
        policy.define_role(Role::reader("reader")).unwrap();
        let err = policy.define_role(Role::reader("reader")).unwrap_err();
        assert_eq!(err, AuthzError::DuplicateRole("reader".into()));
    }

    #[test]
    fn upsert_replaces_role() {
        let mut policy = AccessPolicy::new();
        policy.upsert_role(Role::new("r").allow(Action::ReadNode, Scope::All));
        policy.upsert_role(Role::new("r").allow(Action::CreateNode, Scope::All));
        assert_eq!(policy.role("r").unwrap().permissions().len(), 1);
        assert_eq!(
            policy.role("r").unwrap().permissions()[0].action,
            Action::CreateNode
        );
    }

    #[test]
    fn assign_unknown_role_errors() {
        let mut policy = AccessPolicy::new();
        let err = policy.assign_role("alice", "ghost").unwrap_err();
        assert_eq!(err, AuthzError::UnknownRole("ghost".into()));
    }

    #[test]
    fn assigned_role_grants_its_permissions() {
        let mut policy = AccessPolicy::new();
        policy.define_role(Role::reader("reader")).unwrap();
        policy.assign_role("alice", "reader").unwrap();
        assert!(policy.is_allowed("alice", Action::ReadNode, &task("Task")));
        assert!(!policy.is_allowed("alice", Action::CreateNode, &task("Task")));
    }

    #[test]
    fn revoke_removes_access() {
        let mut policy = AccessPolicy::new();
        policy.define_role(Role::reader("reader")).unwrap();
        policy.assign_role("alice", "reader").unwrap();
        assert!(policy.revoke_role("alice", "reader"));
        assert!(!policy.is_allowed("alice", Action::ReadNode, &task("Task")));
        // revoking again reports nothing was removed
        assert!(!policy.revoke_role("alice", "reader"));
    }

    #[test]
    fn inheritance_unions_permissions() {
        let mut policy = AccessPolicy::new();
        policy.define_role(Role::reader("reader")).unwrap();
        policy
            .define_role(
                Role::new("editor")
                    .inherits("reader")
                    .allow(Action::CreateNode, Scope::All),
            )
            .unwrap();
        policy.assign_role("bob", "editor").unwrap();
        // own permission
        assert!(policy.is_allowed("bob", Action::CreateNode, &task("Task")));
        // inherited permission
        assert!(policy.is_allowed("bob", Action::ReadNode, &task("Task")));
        // not granted anywhere
        assert!(!policy.is_allowed("bob", Action::Compact, &Resource::Global));
    }

    #[test]
    fn deny_in_one_role_overrides_allow_in_another() {
        let mut policy = AccessPolicy::new();
        policy.define_role(Role::editor("editor")).unwrap();
        policy
            .define_role(Role::new("sealed-guard").deny(Action::DeleteNode, Scope::kind("Sealed")))
            .unwrap();
        policy.assign_role("carol", "editor").unwrap();
        policy.assign_role("carol", "sealed-guard").unwrap();
        // editor allows delete generally
        assert!(policy.is_allowed("carol", Action::DeleteNode, &task("Task")));
        // but the guard denies it on Sealed
        assert_eq!(
            policy.evaluate("carol", Action::DeleteNode, &task("Sealed")),
            Decision::Deny(DenyReason::ExplicitDeny)
        );
    }

    #[test]
    fn effective_roles_are_transitive_and_cycle_safe() {
        let mut policy = AccessPolicy::new();
        // a -> b -> a is a cycle; resolution must still terminate
        policy.upsert_role(Role::new("a").inherits("b"));
        policy.upsert_role(Role::new("b").inherits("a"));
        policy.assign_role("dave", "a").unwrap();
        let eff = policy.effective_roles("dave");
        assert!(eff.contains("a") && eff.contains("b"));
    }

    #[test]
    fn validate_flags_dangling_parent() {
        let mut policy = AccessPolicy::new();
        policy.upsert_role(Role::new("child").inherits("missing"));
        assert_eq!(
            policy.validate().unwrap_err(),
            AuthzError::UnknownRole("missing".into())
        );
    }

    #[test]
    fn validate_flags_cycle() {
        let mut policy = AccessPolicy::new();
        policy.upsert_role(Role::new("a").inherits("b"));
        policy.upsert_role(Role::new("b").inherits("a"));
        assert!(matches!(
            policy.validate().unwrap_err(),
            AuthzError::RoleCycle(_)
        ));
    }

    #[test]
    fn validate_accepts_a_clean_hierarchy() {
        let mut policy = AccessPolicy::new();
        policy.define_role(Role::reader("reader")).unwrap();
        policy
            .define_role(Role::new("editor").inherits("reader"))
            .unwrap();
        policy
            .define_role(Role::new("admin").inherits("editor"))
            .unwrap();
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn effective_permissions_lists_full_capability_set() {
        let mut policy = AccessPolicy::new();
        policy.define_role(Role::reader("reader")).unwrap();
        policy
            .define_role(
                Role::new("editor")
                    .inherits("reader")
                    .allow(Action::CreateNode, Scope::All),
            )
            .unwrap();
        policy.assign_role("erin", "editor").unwrap();
        let perms = policy.effective_permissions("erin");
        // reader has 4, editor adds 1
        assert_eq!(perms.len(), 5);
        assert!(perms
            .iter()
            .any(|p| p.action == Action::CreateNode && p.effect == Effect::Allow));
    }
}
