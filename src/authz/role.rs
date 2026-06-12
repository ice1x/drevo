//! [`Role`] — a named, reusable bundle of [`Permission`]s, optionally
//! inheriting from other roles.
//!
//! Subjects are never granted permissions directly; they are assigned roles
//! by an [`AccessPolicy`](crate::authz::AccessPolicy), and a role's effective
//! permission set is the union of its own grants and those of every role it
//! (transitively) inherits. Inheritance keeps common bundles DRY — an `editor`
//! role can `inherits("reader")` and add only the write verbs.

use super::permission::{Action, Effect, Permission, Scope};

/// A named bundle of [`Permission`]s plus the names of the roles it inherits.
///
/// Construct with [`Role::new`] and the chainable builder methods
/// ([`allow`](Role::allow) / [`deny`](Role::deny) / [`grant`](Role::grant) /
/// [`inherits`](Role::inherits)), or reach for a preset
/// ([`Role::reader`] / [`Role::editor`] / [`Role::admin`]).
///
/// Inheritance is stored by *name*, not by value, so a role can name a parent
/// that is registered with the [`AccessPolicy`](crate::authz::AccessPolicy)
/// later. Dangling parents and cycles are caught by
/// [`AccessPolicy::validate`](crate::authz::AccessPolicy::validate); the
/// evaluator itself is cycle-safe and simply ignores unknown parents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    name: String,
    permissions: Vec<Permission>,
    parents: Vec<String>,
}

impl Role {
    /// Create an empty role with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Role {
            name: name.into(),
            permissions: Vec::new(),
            parents: Vec::new(),
        }
    }

    /// The role's unique name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The permissions granted *directly* by this role (not counting
    /// inherited ones).
    pub fn permissions(&self) -> &[Permission] {
        &self.permissions
    }

    /// The names of the roles this role inherits from.
    pub fn parents(&self) -> &[String] {
        &self.parents
    }

    /// Add an already-built [`Permission`] (chainable).
    pub fn grant(mut self, permission: Permission) -> Self {
        self.permissions.push(permission);
        self
    }

    /// Add an [`Effect::Allow`] permission for `action` over `scope`
    /// (chainable).
    pub fn allow(self, action: Action, scope: Scope) -> Self {
        self.grant(Permission::allow(action, scope))
    }

    /// Add an [`Effect::Deny`] permission for `action` over `scope`
    /// (chainable). Deny overrides any matching allow at evaluation time.
    pub fn deny(self, action: Action, scope: Scope) -> Self {
        self.grant(Permission::deny(action, scope))
    }

    /// Grant every [`Action`] as [`Effect::Allow`] over `scope` (chainable) —
    /// the building block of a superuser role. Pair with
    /// [`Scope::All`] for an unrestricted admin.
    pub fn grant_every_action(mut self, scope: Scope) -> Self {
        for action in Action::ALL {
            self.permissions
                .push(Permission::allow(action, scope.clone()));
        }
        self
    }

    /// Declare that this role inherits all permissions of `parent`
    /// (chainable). The parent need not be registered yet.
    pub fn inherits(mut self, parent: impl Into<String>) -> Self {
        self.parents.push(parent.into());
        self
    }

    /// A read-only preset: [`Action::ReadNode`], [`Action::ReadEdge`],
    /// [`Action::Traverse`], and [`Action::Search`] over
    /// [`Scope::All`]. No writes, no Cypher
    /// execution (which can mutate), no admin.
    pub fn reader(name: impl Into<String>) -> Self {
        Role::new(name)
            .allow(Action::ReadNode, Scope::All)
            .allow(Action::ReadEdge, Scope::All)
            .allow(Action::Traverse, Scope::All)
            .allow(Action::Search, Scope::All)
    }

    /// An editor preset: everything a [`reader`](Role::reader) can do, plus the
    /// node / edge create-update-delete writes and [`Action::ExecuteQuery`],
    /// all over [`Scope::All`]. Still no
    /// administrative data-management or role operations.
    pub fn editor(name: impl Into<String>) -> Self {
        Role::reader(name)
            .allow(Action::CreateNode, Scope::All)
            .allow(Action::UpdateNode, Scope::All)
            .allow(Action::DeleteNode, Scope::All)
            .allow(Action::CreateEdge, Scope::All)
            .allow(Action::UpdateEdge, Scope::All)
            .allow(Action::DeleteEdge, Scope::All)
            .allow(Action::ExecuteQuery, Scope::All)
    }

    /// A superuser preset: every [`Action`] over
    /// [`Scope::All`].
    pub fn admin(name: impl Into<String>) -> Self {
        Role::new(name).grant_every_action(Scope::All)
    }

    /// Whether this role directly grants a permission matching the request —
    /// without consulting inherited roles. Returns the matched [`Effect`], or
    /// `None` when no direct permission applies. The
    /// [`AccessPolicy`](crate::authz::AccessPolicy) uses the transitive
    /// closure instead; this is exposed for inspection and testing.
    pub fn direct_effect(
        &self,
        action: Action,
        resource: &super::permission::Resource,
    ) -> Option<Effect> {
        let mut allow = false;
        for p in &self.permissions {
            if p.applies_to(action, resource) {
                match p.effect {
                    Effect::Deny => return Some(Effect::Deny),
                    Effect::Allow => allow = true,
                }
            }
        }
        allow.then_some(Effect::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::super::permission::Resource;
    use super::*;

    #[test]
    fn builder_accumulates_permissions_and_parents() {
        let role = Role::new("triager")
            .allow(Action::ReadNode, Scope::kind("Bug"))
            .deny(Action::DeleteNode, Scope::All)
            .inherits("reader");
        assert_eq!(role.name(), "triager");
        assert_eq!(role.permissions().len(), 2);
        assert_eq!(role.parents(), ["reader"]);
    }

    #[test]
    fn reader_preset_is_read_only() {
        let role = Role::reader("r");
        assert_eq!(
            role.direct_effect(Action::ReadNode, &Resource::kind("Task")),
            Some(Effect::Allow)
        );
        assert_eq!(
            role.direct_effect(Action::Search, &Resource::kind("Task")),
            Some(Effect::Allow)
        );
        assert_eq!(
            role.direct_effect(Action::CreateNode, &Resource::kind("Task")),
            None
        );
    }

    #[test]
    fn editor_preset_can_write_but_not_admin() {
        let role = Role::editor("e");
        assert_eq!(
            role.direct_effect(Action::CreateNode, &Resource::kind("Task")),
            Some(Effect::Allow)
        );
        assert_eq!(
            role.direct_effect(Action::ExecuteQuery, &Resource::Global),
            Some(Effect::Allow)
        );
        assert_eq!(role.direct_effect(Action::Compact, &Resource::Global), None);
        assert_eq!(
            role.direct_effect(Action::ManageRoles, &Resource::Global),
            None
        );
    }

    #[test]
    fn admin_preset_grants_everything() {
        let role = Role::admin("root");
        for action in Action::ALL {
            assert_eq!(
                role.direct_effect(action, &Resource::Global),
                Some(Effect::Allow),
                "admin missing {action}"
            );
            assert_eq!(
                role.direct_effect(action, &Resource::kind("Anything")),
                Some(Effect::Allow)
            );
        }
    }

    #[test]
    fn direct_effect_deny_overrides_allow_within_role() {
        // A role that allows reading all nodes but denies the sealed kind.
        let role = Role::new("redactor")
            .allow(Action::ReadNode, Scope::All)
            .deny(Action::ReadNode, Scope::kind("SealedRecord"));
        assert_eq!(
            role.direct_effect(Action::ReadNode, &Resource::kind("JournalEntry")),
            Some(Effect::Allow)
        );
        assert_eq!(
            role.direct_effect(Action::ReadNode, &Resource::kind("SealedRecord")),
            Some(Effect::Deny)
        );
    }

    #[test]
    fn grant_every_action_scoped_to_a_kind() {
        let role = Role::new("kind-admin").grant_every_action(Scope::kind("Task"));
        assert_eq!(
            role.direct_effect(Action::DeleteNode, &Resource::kind("Task")),
            Some(Effect::Allow)
        );
        // scoped to Task only — a global op is not covered
        assert_eq!(role.direct_effect(Action::Compact, &Resource::Global), None);
    }
}
