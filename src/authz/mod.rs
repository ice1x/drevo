//! Authorization & role-based access control — Phase 15 task `00094`.
//!
//! Production deployments need to answer *who may do what* once the Bolt
//! authentication layer ([`crate::bolt::auth`], task `00074`) has answered
//! *who are you*. This module is that authorization half: a dependency-free,
//! always-compiled RBAC engine that decides whether a named subject may
//! perform an [`Action`](crate::authz::Action) against a
//! [`Resource`](crate::authz::Resource).
//!
//! Three layers, smallest to largest:
//!
//! * [`permission`](crate::authz::permission) — the vocabulary. An
//!   [`Action`](crate::authz::Action) is a verb (node / edge CRUD,
//!   [`Traverse`](crate::authz::Action::Traverse),
//!   [`Search`](crate::authz::Action::Search),
//!   [`ExecuteQuery`](crate::authz::Action::ExecuteQuery), the data-management
//!   ops, RBAC self-administration); a [`Scope`](crate::authz::Scope) is the
//!   noun breadth ([`Scope::All`](crate::authz::Scope::All) or a single
//!   [`Scope::Kind`](crate::authz::Scope::Kind)); a
//!   [`Permission`](crate::authz::Permission) pairs an action + scope with an
//!   [`Effect`](crate::authz::Effect) (allow / deny). The decision-time
//!   counterpart of a scope is a [`Resource`](crate::authz::Resource).
//! * [`role`](crate::authz::role) — a [`Role`](crate::authz::Role) is a named,
//!   reusable bundle of permissions that may inherit other roles. Presets
//!   ([`Role::reader`](crate::authz::Role::reader) /
//!   [`Role::editor`](crate::authz::Role::editor) /
//!   [`Role::admin`](crate::authz::Role::admin)) cover the common cases; the
//!   chainable builder ([`allow`](crate::authz::Role::allow) /
//!   [`deny`](crate::authz::Role::deny) /
//!   [`inherits`](crate::authz::Role::inherits)) covers the rest.
//! * [`policy`](crate::authz::policy) — the
//!   [`AccessPolicy`](crate::authz::AccessPolicy) engine holds the role
//!   registry and the subject→role assignments, resolves a subject's effective
//!   (transitive) roles, and renders an authorization
//!   [`Decision`](crate::authz::Decision).
//!
//! ## Evaluation model
//!
//! Authorization is **deny-overrides and closed-world**: when a subject's
//! effective permissions contain a matching
//! [`Effect::Deny`](crate::authz::Effect::Deny) the request is refused even if
//! an [`Effect::Allow`](crate::authz::Effect::Allow) also matches; with no
//! matching permission at all the request is denied by default. This lets a
//! broad grant be carved out by a narrow denial — "read every node *except*
//! the `SealedRecord` kind" is one allow plus one deny.
//!
//! ## Scope (`00094`)
//!
//! Like the planner (`00085`–`00089`) and MVCC (`00081`) engines in their
//! early tasks, this is a self-contained mechanism: it reasons over roles,
//! permissions, and subjects, but is **not yet wired into the executor, the
//! HTTP API, or the Bolt session**. It keeps its own
//! [`AuthzError`](crate::authz::AuthzError) channel rather than touching the
//! crate-wide [`crate::error::DrevoError`]. Enforcing a
//! [`Decision`](crate::authz::Decision) at each request boundary — mapping the
//! authenticated Bolt username to a subject and checking it before a mutation
//! runs — is later-task work; this is the policy substrate that work needs.
//!
//! Dependency-free, always compiled, and WASM-safe (`std::collections` only,
//! no threads, no I/O).
//!
//! ## Example
//!
//! ```
//! use drevo::authz::{AccessPolicy, Action, Resource, Role, Scope};
//!
//! let mut policy = AccessPolicy::new();
//! // A read-only analyst, and an editor that inherits it and adds writes.
//! policy.define_role(Role::reader("analyst")).unwrap();
//! policy
//!     .define_role(Role::new("editor").inherits("analyst").allow(Action::CreateNode, Scope::All))
//!     .unwrap();
//! policy.assign_role("alice", "editor").unwrap();
//!
//! assert!(policy.is_allowed("alice", Action::ReadNode, &Resource::kind("Task")));
//! assert!(policy.is_allowed("alice", Action::CreateNode, &Resource::kind("Task")));
//! assert!(!policy.is_allowed("alice", Action::Compact, &Resource::Global));
//! ```

pub mod permission;
pub mod policy;
pub mod role;

pub use permission::{Action, Effect, Permission, Resource, Scope};
pub use policy::{AccessPolicy, AuthzError, Decision, DenyReason};
pub use role::Role;
