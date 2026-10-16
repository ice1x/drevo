//! The vocabulary an [`AccessPolicy`](crate::authz::AccessPolicy) reasons
//! over: [`Action`]s a subject may attempt, the [`Scope`] a grant covers, the
//! concrete [`Resource`] being accessed at decision time, and the
//! [`Permission`] that pairs an action + scope with an [`Effect`].

use std::fmt;

/// A single operation a subject may attempt against the graph.
///
/// The set mirrors the surface a production deployment needs to gate: node /
/// edge CRUD, the read-side traversal + search paths, Cypher execution, the
/// data-management operations ([`Export`](Action::Export) /
/// [`Import`](Action::Import) / [`Compact`](Action::Compact)), and RBAC
/// self-administration ([`ManageRoles`](Action::ManageRoles)). Actions are the
/// *verbs*; the [`Scope`] a [`Permission`] carries is the *noun breadth*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    /// Read a node by id / uuid / title, or list nodes by kind.
    ReadNode,
    /// Create a new node.
    CreateNode,
    /// Update an existing node's properties.
    UpdateNode,
    /// Delete a node (and, by cascade, its incident edges).
    DeleteNode,
    /// Read an edge.
    ReadEdge,
    /// Create a new edge between two nodes.
    CreateEdge,
    /// Update an existing edge's properties / weight.
    UpdateEdge,
    /// Delete an edge.
    DeleteEdge,
    /// Walk the graph: BFS / DFS / shortest-path / subgraph extraction.
    Traverse,
    /// Run a full-text search query.
    Search,
    /// Execute a Cypher statement.
    ExecuteQuery,
    /// Export the graph to the JSON / GraphML wire format.
    Export,
    /// Import a graph from the JSON wire format.
    Import,
    /// Trigger storage compaction.
    Compact,
    /// Administer the RBAC policy itself — define roles, assign / revoke them.
    ManageRoles,
}

impl Action {
    /// Every [`Action`] variant, for callers that want to grant or audit the
    /// full set at once (for example an `admin` role via
    /// [`Role::grant_every_action`](crate::authz::Role::grant_every_action)).
    pub const ALL: [Action; 15] = [
        Action::ReadNode,
        Action::CreateNode,
        Action::UpdateNode,
        Action::DeleteNode,
        Action::ReadEdge,
        Action::CreateEdge,
        Action::UpdateEdge,
        Action::DeleteEdge,
        Action::Traverse,
        Action::Search,
        Action::ExecuteQuery,
        Action::Export,
        Action::Import,
        Action::Compact,
        Action::ManageRoles,
    ];

    /// Whether this action mutates graph data — the node / edge create,
    /// update, and delete operations plus [`Import`](Action::Import). The
    /// read-side actions, [`Export`](Action::Export),
    /// [`Compact`](Action::Compact), and [`ManageRoles`](Action::ManageRoles)
    /// are not data writes. Used to build read-only roles without enumerating
    /// every verb by hand.
    pub fn is_write(self) -> bool {
        matches!(
            self,
            Action::CreateNode
                | Action::UpdateNode
                | Action::DeleteNode
                | Action::CreateEdge
                | Action::UpdateEdge
                | Action::DeleteEdge
                | Action::Import
        )
    }

    /// Whether this action is a privileged administrative operation:
    /// [`Export`](Action::Export), [`Import`](Action::Import),
    /// [`Compact`](Action::Compact), or
    /// [`ManageRoles`](Action::ManageRoles). (Note [`Import`](Action::Import)
    /// is reported by both [`is_write`](Action::is_write) and this method — it
    /// both mutates data and is an administrative bulk operation.)
    pub fn is_admin(self) -> bool {
        matches!(
            self,
            Action::Export | Action::Import | Action::Compact | Action::ManageRoles
        )
    }

    /// A stable, lowercase, dotted string name for the action — handy for
    /// audit logs and error messages (`"node.create"`, `"role.manage"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Action::ReadNode => "node.read",
            Action::CreateNode => "node.create",
            Action::UpdateNode => "node.update",
            Action::DeleteNode => "node.delete",
            Action::ReadEdge => "edge.read",
            Action::CreateEdge => "edge.create",
            Action::UpdateEdge => "edge.update",
            Action::DeleteEdge => "edge.delete",
            Action::Traverse => "graph.traverse",
            Action::Search => "graph.search",
            Action::ExecuteQuery => "query.execute",
            Action::Export => "data.export",
            Action::Import => "data.import",
            Action::Compact => "data.compact",
            Action::ManageRoles => "role.manage",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The breadth of a [`Permission`]: either *every* resource, or only graph
/// elements carrying one specific `kind`.
///
/// This is the "scoped permissions" half of RBAC — a grant of
/// [`Action::ReadNode`] under `Scope::Kind("JournalEntry")` lets a subject read
/// CBT journal entries but nothing else, whereas [`Scope::All`] reads every
/// node kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    /// Matches every resource, regardless of kind, including the
    /// kind-less [`Resource::Global`].
    All,
    /// Matches only a [`Resource::Kind`] whose kind string is exactly equal.
    Kind(String),
}

impl Scope {
    /// Convenience constructor for a kind scope from anything string-like.
    pub fn kind(kind: impl Into<String>) -> Self {
        Scope::Kind(kind.into())
    }

    /// Whether this scope covers `resource`.
    ///
    /// [`Scope::All`] covers everything (including [`Resource::Global`]);
    /// [`Scope::Kind`] covers only a [`Resource::Kind`] with the same kind
    /// string and never the kind-less [`Resource::Global`].
    pub fn matches(&self, resource: &Resource) -> bool {
        match self {
            Scope::All => true,
            Scope::Kind(k) => matches!(resource, Resource::Kind(rk) if rk == k),
        }
    }
}

/// The concrete thing being accessed when a request is evaluated.
///
/// Decision-time counterpart to [`Scope`]: a node / edge operation names the
/// element's [`Resource::Kind`], while a kind-less operation (an export, a
/// compaction, a role change) is evaluated against [`Resource::Global`] and so
/// can only be authorised by a [`Scope::All`] grant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Resource {
    /// A graph element of a particular `kind`.
    Kind(String),
    /// A resource with no kind dimension — a server- or database-level
    /// operation such as export, import, compaction, or role management.
    Global,
}

impl Resource {
    /// Convenience constructor for a kind resource from anything string-like.
    pub fn kind(kind: impl Into<String>) -> Self {
        Resource::Kind(kind.into())
    }
}

/// Whether a matching [`Permission`] grants or refuses access.
///
/// drevo's RBAC is **deny-overrides**: when a subject's effective permissions
/// contain both an [`Effect::Allow`] and an [`Effect::Deny`] that match the
/// same request, the [`Effect::Deny`] wins. This lets a broad
/// [`Scope::All`] grant be carved out by a narrow [`Scope::Kind`] denial
/// (for example "may read everything *except* `SealedRecord` nodes").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effect {
    /// A matching permission with this effect grants the request (unless a
    /// matching [`Effect::Deny`] also applies).
    Allow,
    /// A matching permission with this effect refuses the request outright —
    /// it overrides any matching [`Effect::Allow`].
    Deny,
}

/// A single grant: an [`Action`] over a [`Scope`], with an [`Effect`].
///
/// Permissions are held by [`Role`](crate::authz::Role)s, never assigned to
/// subjects directly — that indirection is what makes the model *role*-based.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Permission {
    /// The action this permission concerns.
    pub action: Action,
    /// The resource breadth this permission covers.
    pub scope: Scope,
    /// Whether the permission grants or refuses.
    pub effect: Effect,
}

impl Permission {
    /// Build an [`Effect::Allow`] permission for `action` over `scope`.
    pub fn allow(action: Action, scope: Scope) -> Self {
        Permission {
            action,
            scope,
            effect: Effect::Allow,
        }
    }

    /// Build an [`Effect::Deny`] permission for `action` over `scope`.
    pub fn deny(action: Action, scope: Scope) -> Self {
        Permission {
            action,
            scope,
            effect: Effect::Deny,
        }
    }

    /// Whether this permission is relevant to a request for `action` against
    /// `resource` — the action matches exactly and the [`Scope`] covers the
    /// [`Resource`]. The [`Effect`] is *not* consulted here; the policy engine
    /// inspects it after collecting every applicable permission.
    pub fn applies_to(&self, action: Action, resource: &Resource) -> bool {
        self.action == action && self.scope.matches(resource)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_all_has_every_variant_once() {
        // ALL must list each variant exactly once — a duplicate or omission
        // would silently break `grant_every_action`.
        assert_eq!(Action::ALL.len(), 15);
        for (i, a) in Action::ALL.iter().enumerate() {
            for b in &Action::ALL[i + 1..] {
                assert_ne!(a, b, "duplicate action in ALL: {a}");
            }
        }
    }

    #[test]
    fn is_write_covers_mutations_only() {
        for a in Action::ALL {
            let expected = matches!(
                a,
                Action::CreateNode
                    | Action::UpdateNode
                    | Action::DeleteNode
                    | Action::CreateEdge
                    | Action::UpdateEdge
                    | Action::DeleteEdge
                    | Action::Import
            );
            assert_eq!(a.is_write(), expected, "is_write wrong for {a}");
        }
        assert!(!Action::ReadNode.is_write());
        assert!(Action::CreateNode.is_write());
    }

    #[test]
    fn is_admin_covers_privileged_ops() {
        assert!(Action::Export.is_admin());
        assert!(Action::Import.is_admin());
        assert!(Action::Compact.is_admin());
        assert!(Action::ManageRoles.is_admin());
        assert!(!Action::ReadNode.is_admin());
        assert!(!Action::CreateNode.is_admin());
        // Import is both a write and an admin op.
        assert!(Action::Import.is_write() && Action::Import.is_admin());
    }

    #[test]
    fn action_as_str_is_unique_and_stable() {
        let mut seen = std::collections::BTreeSet::new();
        for a in Action::ALL {
            assert!(seen.insert(a.as_str()), "duplicate as_str for {a:?}");
            assert_eq!(a.to_string(), a.as_str());
        }
    }

    #[test]
    fn scope_all_matches_any_resource() {
        assert!(Scope::All.matches(&Resource::Global));
        assert!(Scope::All.matches(&Resource::kind("Task")));
    }

    #[test]
    fn scope_kind_matches_only_same_kind() {
        let s = Scope::kind("Task");
        assert!(s.matches(&Resource::kind("Task")));
        assert!(!s.matches(&Resource::kind("Bug")));
        // a kind scope never authorises a kind-less global resource
        assert!(!s.matches(&Resource::Global));
    }

    #[test]
    fn permission_applies_requires_action_and_scope() {
        let p = Permission::allow(Action::ReadNode, Scope::kind("Task"));
        assert!(p.applies_to(Action::ReadNode, &Resource::kind("Task")));
        // wrong action
        assert!(!p.applies_to(Action::CreateNode, &Resource::kind("Task")));
        // wrong kind
        assert!(!p.applies_to(Action::ReadNode, &Resource::kind("Bug")));
    }

    #[test]
    fn allow_and_deny_constructors_set_effect() {
        assert_eq!(
            Permission::allow(Action::ReadNode, Scope::All).effect,
            Effect::Allow
        );
        assert_eq!(
            Permission::deny(Action::DeleteNode, Scope::All).effect,
            Effect::Deny
        );
    }
}
