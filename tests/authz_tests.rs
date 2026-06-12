//! Integration tests for the authorization & RBAC engine (Phase 15 task
//! `00094`).
//!
//! Where the inline unit tests pin individual methods, these exercise the
//! [`AccessPolicy`] as an embedder would: building a role hierarchy for one of
//! drevo's five target scenarios (CBT journal, story / book editor, IT task
//! manager, ERP, bug tracker), assigning roles to subjects, and asserting the
//! resulting authorization decisions across a realistic workflow.

use drevo::authz::{AccessPolicy, Action, Decision, DenyReason, Resource, Role, Scope};

/// CBT journal — the most privacy-sensitive scenario. A therapist may read and
/// annotate a client's journal entries, but `SealedRecord` entries the client
/// marked private are denied even though the therapist otherwise reads
/// everything. The client themselves is an editor over their own graph.
#[test]
fn cbt_journal_privacy_scoped_deny() {
    let mut policy = AccessPolicy::new();

    // therapist: reads + annotates (update) every entry, but a narrow deny
    // carves out the sealed kind.
    policy
        .define_role(
            Role::new("therapist")
                .allow(Action::ReadNode, Scope::All)
                .allow(Action::UpdateNode, Scope::kind("JournalEntry"))
                .allow(Action::Traverse, Scope::All)
                .allow(Action::Search, Scope::All)
                .deny(Action::ReadNode, Scope::kind("SealedRecord")),
        )
        .unwrap();
    // client: full editor over their own journal.
    policy.define_role(Role::editor("client")).unwrap();

    policy.assign_role("dr_jung", "therapist").unwrap();
    policy.assign_role("patient_amy", "client").unwrap();

    // therapist reads + annotates a normal entry
    assert!(policy.is_allowed("dr_jung", Action::ReadNode, &Resource::kind("JournalEntry")));
    assert!(policy.is_allowed(
        "dr_jung",
        Action::UpdateNode,
        &Resource::kind("JournalEntry")
    ));
    // but the sealed record is explicitly denied, overriding the broad read
    assert_eq!(
        policy.evaluate("dr_jung", Action::ReadNode, &Resource::kind("SealedRecord")),
        Decision::Deny(DenyReason::ExplicitDeny)
    );
    // therapist cannot delete entries — never granted (closed-world)
    assert_eq!(
        policy.evaluate(
            "dr_jung",
            Action::DeleteNode,
            &Resource::kind("JournalEntry")
        ),
        Decision::Deny(DenyReason::NoMatchingGrant)
    );
    // the client owns their journal end to end
    assert!(policy.is_allowed(
        "patient_amy",
        Action::DeleteNode,
        &Resource::kind("SealedRecord")
    ));
}

/// Story / book editor — an inheritance chain: `proofreader` ⊂ `author` ⊂
/// `editor_in_chief`. Each tier adds capability without re-listing the lower
/// ones.
#[test]
fn story_editor_inheritance_chain() {
    let mut policy = AccessPolicy::new();

    policy
        .define_role(
            Role::new("proofreader")
                .allow(Action::ReadNode, Scope::All)
                .allow(Action::Search, Scope::All),
        )
        .unwrap();
    policy
        .define_role(
            Role::new("author")
                .inherits("proofreader")
                .allow(Action::CreateNode, Scope::kind("Chapter"))
                .allow(Action::UpdateNode, Scope::kind("Chapter")),
        )
        .unwrap();
    policy
        .define_role(
            Role::new("editor_in_chief")
                .inherits("author")
                .allow(Action::DeleteNode, Scope::All)
                .allow(Action::Export, Scope::All),
        )
        .unwrap();
    assert!(policy.validate().is_ok());

    policy.assign_role("intern", "proofreader").unwrap();
    policy.assign_role("rowling", "author").unwrap();
    policy.assign_role("chief", "editor_in_chief").unwrap();

    // proofreader: read-only
    assert!(policy.is_allowed("intern", Action::ReadNode, &Resource::kind("Chapter")));
    assert!(!policy.is_allowed("intern", Action::UpdateNode, &Resource::kind("Chapter")));

    // author inherits read, adds chapter writes
    assert!(policy.is_allowed("rowling", Action::ReadNode, &Resource::kind("Chapter")));
    assert!(policy.is_allowed("rowling", Action::CreateNode, &Resource::kind("Chapter")));
    // ...but cannot delete or export (those are editor-in-chief only)
    assert!(!policy.is_allowed("rowling", Action::DeleteNode, &Resource::kind("Chapter")));
    assert!(!policy.is_allowed("rowling", Action::Export, &Resource::Global));

    // editor-in-chief inherits the whole chain
    assert!(policy.is_allowed("chief", Action::ReadNode, &Resource::kind("Chapter")));
    assert!(policy.is_allowed("chief", Action::CreateNode, &Resource::kind("Chapter")));
    assert!(policy.is_allowed("chief", Action::DeleteNode, &Resource::kind("Chapter")));
    assert!(policy.is_allowed("chief", Action::Export, &Resource::Global));
}

/// IT task manager — scoped permissions: a contractor may only touch `Task`
/// nodes of their own project, modelled as a kind, and may execute read
/// queries but not administer anything.
#[test]
fn task_manager_scoped_contractor() {
    let mut policy = AccessPolicy::new();
    policy
        .define_role(
            Role::new("contractor")
                .allow(Action::ReadNode, Scope::kind("Task"))
                .allow(Action::UpdateNode, Scope::kind("Task"))
                .allow(Action::ReadEdge, Scope::All)
                .allow(Action::Traverse, Scope::All),
        )
        .unwrap();
    policy.assign_role("freelancer_sam", "contractor").unwrap();

    // can read/update Tasks
    assert!(policy.is_allowed(
        "freelancer_sam",
        Action::UpdateNode,
        &Resource::kind("Task")
    ));
    // cannot touch Milestone nodes (out of scope)
    assert!(!policy.is_allowed(
        "freelancer_sam",
        Action::UpdateNode,
        &Resource::kind("Milestone")
    ));
    // cannot create Tasks (only update existing)
    assert!(!policy.is_allowed(
        "freelancer_sam",
        Action::CreateNode,
        &Resource::kind("Task")
    ));
    // cannot run admin ops
    assert!(!policy.is_allowed("freelancer_sam", Action::Compact, &Resource::Global));
}

/// ERP — segregation of duties: the `approver` may approve (update) purchase
/// orders but is denied the ability to create them, and vice-versa for the
/// `requisitioner`. A single user holding both roles still cannot bypass a
/// deny, because deny overrides allow.
#[test]
fn erp_segregation_of_duties() {
    let mut policy = AccessPolicy::new();
    policy
        .define_role(
            Role::new("requisitioner")
                .allow(Action::CreateNode, Scope::kind("PurchaseOrder"))
                .allow(Action::ReadNode, Scope::All)
                // explicitly may NOT approve
                .deny(Action::UpdateNode, Scope::kind("PurchaseOrder")),
        )
        .unwrap();
    policy
        .define_role(
            Role::new("approver")
                .allow(Action::UpdateNode, Scope::kind("PurchaseOrder"))
                .allow(Action::ReadNode, Scope::All),
        )
        .unwrap();

    policy.assign_role("buyer_lee", "requisitioner").unwrap();
    policy.assign_role("manager_ng", "approver").unwrap();

    assert!(policy.is_allowed(
        "buyer_lee",
        Action::CreateNode,
        &Resource::kind("PurchaseOrder")
    ));
    assert!(policy.is_allowed(
        "manager_ng",
        Action::UpdateNode,
        &Resource::kind("PurchaseOrder")
    ));

    // The conflicted user holds both roles — the requisitioner deny must win
    // over the approver allow (separation of duties cannot be bypassed).
    policy.assign_role("conflicted", "requisitioner").unwrap();
    policy.assign_role("conflicted", "approver").unwrap();
    assert!(policy.is_allowed(
        "conflicted",
        Action::CreateNode,
        &Resource::kind("PurchaseOrder")
    ));
    assert_eq!(
        policy.evaluate(
            "conflicted",
            Action::UpdateNode,
            &Resource::kind("PurchaseOrder")
        ),
        Decision::Deny(DenyReason::ExplicitDeny)
    );
}

/// Bug tracker — three classic tiers (reporter / developer / maintainer) plus
/// the admin preset. Verifies the full capability set and that revoking a role
/// immediately drops access.
#[test]
fn bug_tracker_tiers_and_revocation() {
    let mut policy = AccessPolicy::new();
    policy
        .define_role(
            Role::new("reporter")
                .allow(Action::CreateNode, Scope::kind("Bug"))
                .allow(Action::ReadNode, Scope::All)
                .allow(Action::Search, Scope::All),
        )
        .unwrap();
    policy
        .define_role(
            Role::new("developer")
                .inherits("reporter")
                .allow(Action::UpdateNode, Scope::kind("Bug"))
                .allow(Action::CreateEdge, Scope::All)
                .allow(Action::ExecuteQuery, Scope::All),
        )
        .unwrap();
    policy.define_role(Role::admin("maintainer")).unwrap();
    assert!(policy.validate().is_ok());

    policy.assign_role("triager_max", "developer").unwrap();
    policy.assign_role("lead_kim", "maintainer").unwrap();

    // developer: file + triage bugs, run queries
    assert!(policy.is_allowed("triager_max", Action::CreateNode, &Resource::kind("Bug")));
    assert!(policy.is_allowed("triager_max", Action::UpdateNode, &Resource::kind("Bug")));
    assert!(policy.is_allowed("triager_max", Action::ExecuteQuery, &Resource::Global));
    // ...but not compaction / role management
    assert!(!policy.is_allowed("triager_max", Action::Compact, &Resource::Global));
    assert!(!policy.is_allowed("triager_max", Action::ManageRoles, &Resource::Global));

    // maintainer (admin) can do literally everything
    for action in Action::ALL {
        assert!(
            policy.is_allowed("lead_kim", action, &Resource::Global),
            "maintainer should be allowed {action}"
        );
    }

    // revoke the developer role: access disappears at once
    assert!(policy.revoke_role("triager_max", "developer"));
    assert!(!policy.is_allowed("triager_max", Action::CreateNode, &Resource::kind("Bug")));
    // and an unknown subject is denied everywhere
    assert!(!policy.is_allowed("nobody", Action::ReadNode, &Resource::kind("Bug")));
}

/// The authenticated-username → subject bridge: the string a caller passes as
/// the subject is exactly what an authentication layer would resolve from a
/// session token, so the same policy serves any number of identities.
#[test]
fn subject_is_an_opaque_identity_string() {
    let mut policy = AccessPolicy::new();
    policy.define_role(Role::reader("viewer")).unwrap();
    // assign the same role to several distinct usernames
    for user in ["alice@corp", "bob@corp", "svc-ingest"] {
        policy.assign_role(user, "viewer").unwrap();
        assert!(policy.is_allowed(user, Action::ReadNode, &Resource::kind("Doc")));
        assert!(!policy.is_allowed(user, Action::DeleteNode, &Resource::kind("Doc")));
    }
    assert_eq!(policy.assigned_roles("alice@corp"), ["viewer"]);
}

/// A misconfigured policy (cycle in the hierarchy) is reported by `validate`
/// yet still evaluates without hanging — defensive termination.
#[test]
fn cyclic_hierarchy_is_reported_but_still_terminates() {
    let mut policy = AccessPolicy::new();
    policy.upsert_role(
        Role::new("a")
            .inherits("b")
            .allow(Action::ReadNode, Scope::All),
    );
    policy.upsert_role(
        Role::new("b")
            .inherits("a")
            .allow(Action::CreateNode, Scope::All),
    );
    policy.assign_role("u", "a").unwrap();

    // validate flags the cycle...
    assert!(policy.validate().is_err());
    // ...but evaluation still resolves the union of both roles and terminates
    assert!(policy.is_allowed("u", Action::ReadNode, &Resource::kind("X")));
    assert!(policy.is_allowed("u", Action::CreateNode, &Resource::kind("X")));
}
