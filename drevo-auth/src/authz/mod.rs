//! Authorization: role-based access control.
//!
//! Once authentication has answered *who are you*, this module answers *who
//! may do what*: an RBAC engine that decides whether a named subject may
//! perform an [`Action`] against a
//! [`Resource`].
//!
//! Three layers, smallest to largest:
//!
//! * [`permission`] — the vocabulary. An
//!   [`Action`] is a verb (node / edge CRUD,
//!   [`Traverse`](crate::authz::Action::Traverse),
//!   [`Search`](crate::authz::Action::Search),
//!   [`ExecuteQuery`](crate::authz::Action::ExecuteQuery), the data-management
//!   ops, RBAC self-administration); a [`Scope`] is the
//!   noun breadth ([`Scope::All`] or a single
//!   [`Scope::Kind`]); a
//!   [`Permission`] pairs an action + scope with an
//!   [`Effect`] (allow / deny). The decision-time
//!   counterpart of a scope is a [`Resource`].
//! * [`role`] — a [`Role`] is a named,
//!   reusable bundle of permissions that may inherit other roles. Presets
//!   ([`Role::reader`] /
//!   [`Role::editor`] /
//!   [`Role::admin`]) cover the common cases; the
//!   chainable builder ([`allow`](crate::authz::Role::allow) /
//!   [`deny`](crate::authz::Role::deny) /
//!   [`inherits`](crate::authz::Role::inherits)) covers the rest.
//! * [`policy`] — the
//!   [`AccessPolicy`] engine holds the role
//!   registry and the subject→role assignments, resolves a subject's effective
//!   (transitive) roles, and renders an authorization
//!   [`Decision`].
//!
//! ## Evaluation model
//!
//! Authorization is **deny-overrides and closed-world**: when a subject's
//! effective permissions contain a matching
//! [`Effect::Deny`] the request is refused even if
//! an [`Effect::Allow`] also matches; with no
//! matching permission at all the request is denied by default. This lets a
//! broad grant be carved out by a narrow denial — "read every node *except*
//! the `SealedRecord` kind" is one allow plus one deny.
//!
//! ## Subjects
//!
//! A subject is any identity string — typically the username that
//! [`authn`](crate::authn) resolved from a password or session token. The
//! policy does not authenticate; the caller checks a
//! [`Decision`] before it performs the action.
//!
//! Configuration errors come back as [`AuthzError`].
//! This module uses `std::collections` only — no threads, no I/O — and builds
//! for `wasm32-unknown-unknown`.
//!
//! ## Example
//!
//! ```
//! use drevo_auth::authz::{AccessPolicy, Action, Resource, Role, Scope};
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
