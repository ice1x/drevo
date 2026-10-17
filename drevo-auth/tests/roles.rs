//! Roles in the user catalog: the built-in roles, creating, renaming and
//! dropping roles, giving them to users, granting and denying privileges,
//! and deciding what a user may do.
#![cfg(feature = "password")]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use drevo_auth::catalog::{FileStore, MemoryStore};
use drevo_auth::privileges::{
    parse, Access, Action, Decision, Element, Op, Privilege, Request, BUILTIN_ROLES, PUBLIC,
};
use drevo_auth::users::{AuthManager, Bootstrap, ManageError, NewUser, PrivilegeFilter};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_auth_roles_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn file(&self) -> PathBuf {
        self.0.join("security.json")
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn manager() -> AuthManager {
    AuthManager::open(Arc::new(MemoryStore::new())).unwrap()
}

/// A manager with `ann` and `bob`.
fn with_users() -> AuthManager {
    let m = manager();
    m.create_user(NewUser::new("ann", "ann-password")).unwrap();
    m.create_user(NewUser::new("bob", "bob-password")).unwrap();
    m
}

fn privileges(text: &str) -> Vec<Privilege> {
    parse(text).unwrap().privileges
}

/// Run a GRANT / DENY / REVOKE command against `m`.
fn apply(m: &AuthManager, text: &str) {
    let c = parse(text).unwrap();
    for role in &c.roles {
        match c.op {
            Op::Grant => m.grant(Access::Grant, &c.privileges, role).unwrap(),
            Op::Deny => m.grant(Access::Deny, &c.privileges, role).unwrap(),
            Op::Revoke(access) => m.revoke(access, &c.privileges, role).unwrap(),
        }
    }
}

fn names(m: &AuthManager) -> Vec<String> {
    m.roles().into_iter().map(|r| r.name).collect()
}

fn read(user_db: &'static str) -> Request<'static> {
    Request {
        action: Action::Read,
        database: user_db,
        home: user_db,
        element: Element::Node(&["Task"]),
        name: Some("title"),
    }
}

fn create() -> Request<'static> {
    Request {
        action: Action::Create,
        database: "tasks",
        home: "tasks",
        element: Element::Node(&["Task"]),
        name: None,
    }
}

// ── Built-in roles ───────────────────────────────────────────────────

#[test]
fn a_new_catalog_has_the_built_in_roles() {
    let m = with_users();
    let mut expected: Vec<String> = BUILTIN_ROLES.iter().map(|r| r.to_string()).collect();
    expected.sort();
    assert_eq!(names(&m), expected);
    let roles = m.roles();
    let public = roles.iter().find(|r| r.name == PUBLIC).unwrap();
    assert!(public.builtin);
    assert_eq!(public.users, ["ann", "bob"], "every user has PUBLIC");
    let admin = roles.iter().find(|r| r.name == "admin").unwrap();
    assert!(admin.users.is_empty());
}

#[test]
fn every_user_has_public_and_only_the_roles_given() {
    let m = with_users();
    assert_eq!(m.user_roles("ann").unwrap(), [PUBLIC]);
    m.grant_role("reader", "ann").unwrap();
    assert_eq!(m.user_roles("ann").unwrap(), [PUBLIC, "reader"]);
    assert_eq!(m.user("ann").unwrap().roles, [PUBLIC, "reader"]);
    assert!(m.user_roles("nobody").is_none());
}

#[test]
fn the_bootstrap_administrator_has_the_admin_role() {
    let m = manager();
    m.bootstrap(&Bootstrap::parse(Some("root/root-password")).unwrap())
        .unwrap();
    assert_eq!(m.user_roles("root").unwrap(), [PUBLIC, "admin"]);
    let m = manager();
    m.bootstrap(&Bootstrap::Generated).unwrap();
    assert_eq!(m.user_roles("neo4j").unwrap(), [PUBLIC, "admin"]);
}

// ── Creating, renaming and dropping roles ────────────────────────────

#[test]
fn roles_are_created_once_unless_if_not_exists() {
    let m = manager();
    m.create_role("triage", false, None).unwrap();
    assert!(names(&m).contains(&"triage".to_string()));
    assert_eq!(
        m.create_role("triage", false, None),
        Err(ManageError::RoleExists("triage".into()))
    );
    m.create_role("triage", true, None).unwrap();
    assert_eq!(
        m.create_role("reader", false, None),
        Err(ManageError::RoleExists("reader".into()))
    );
}

#[test]
fn role_names_follow_neo4j_rules() {
    let m = manager();
    for good in ["qa", "QA_team", "t1", "a_"] {
        m.create_role(good, false, None)
            .unwrap_or_else(|e| panic!("{good}: {e}"));
    }
    for bad in ["", "1qa", "qa team", "qa-team", "ünï", &"r".repeat(66)] {
        assert!(
            matches!(
                m.create_role(bad, false, None),
                Err(ManageError::InvalidRoleName { .. })
            ),
            "{bad:?}"
        );
    }
}

#[test]
fn a_copy_takes_the_privileges_but_not_the_users() {
    let m = with_users();
    m.grant_role("editor", "ann").unwrap();
    m.create_role("contributor", false, Some("editor")).unwrap();
    let editor = m.privileges(&PrivilegeFilter::Role("editor".into()));
    let copy = m.privileges(&PrivilegeFilter::Role("contributor".into()));
    assert_eq!(copy.len(), editor.len());
    assert!(copy.iter().all(|e| e.role == "contributor"));
    let info = m
        .roles()
        .into_iter()
        .find(|r| r.name == "contributor")
        .unwrap();
    assert!(info.users.is_empty());
    assert!(!info.builtin);
    assert_eq!(
        m.create_role("x", false, Some("missing")),
        Err(ManageError::RoleNotFound("missing".into()))
    );
}

#[test]
fn dropping_a_role_takes_it_from_its_users() {
    let m = with_users();
    m.create_role("triage", false, None).unwrap();
    m.grant_role("triage", "ann").unwrap();
    m.drop_role("triage", false).unwrap();
    assert_eq!(m.user_roles("ann").unwrap(), [PUBLIC]);
    assert_eq!(
        m.drop_role("triage", false),
        Err(ManageError::RoleNotFound("triage".into()))
    );
    m.drop_role("triage", true).unwrap();
    // Built-in roles other than PUBLIC can be dropped, as in Neo4j.
    m.drop_role("publisher", false).unwrap();
    assert!(matches!(
        m.drop_role(PUBLIC, false),
        Err(ManageError::PublicRole { .. })
    ));
}

#[test]
fn renaming_a_role_keeps_its_privileges_and_users() {
    let m = with_users();
    m.create_role("qa", false, None).unwrap();
    apply(&m, "GRANT TRAVERSE ON GRAPH * TO qa");
    m.grant_role("qa", "bob").unwrap();
    m.rename_role("qa", "testers", false).unwrap();
    assert_eq!(m.user_roles("bob").unwrap(), [PUBLIC, "testers"]);
    assert_eq!(
        m.privileges(&PrivilegeFilter::Role("testers".into())).len(),
        2
    );
    assert!(m.privileges(&PrivilegeFilter::Role("qa".into())).is_empty());
    assert_eq!(
        m.rename_role("testers", "reader", false),
        Err(ManageError::RoleExists("reader".into()))
    );
    assert_eq!(
        m.rename_role("qa", "x", false),
        Err(ManageError::RoleNotFound("qa".into()))
    );
    m.rename_role("qa", "x", true).unwrap();
    assert!(matches!(
        m.rename_role(PUBLIC, "everyone", false),
        Err(ManageError::PublicRole { .. })
    ));
    assert!(matches!(
        m.rename_role("testers", "bad name", false),
        Err(ManageError::InvalidRoleName { .. })
    ));
}

// ── Giving roles to users ────────────────────────────────────────────

#[test]
fn roles_are_granted_to_and_revoked_from_users() {
    let m = with_users();
    m.grant_role("reader", "ann").unwrap();
    m.grant_role("reader", "ann").unwrap(); // twice is once
    m.grant_role("editor", "ann").unwrap();
    assert_eq!(m.user_roles("ann").unwrap(), [PUBLIC, "editor", "reader"]);
    m.revoke_role("reader", "ann").unwrap();
    m.revoke_role("reader", "ann").unwrap(); // not held: nothing to do
    assert_eq!(m.user_roles("ann").unwrap(), [PUBLIC, "editor"]);
    let reader = m.roles().into_iter().find(|r| r.name == "reader").unwrap();
    assert!(reader.users.is_empty());
}

#[test]
fn granting_unknown_roles_or_users_fails() {
    let m = with_users();
    assert_eq!(
        m.grant_role("ghost", "ann"),
        Err(ManageError::RoleNotFound("ghost".into()))
    );
    assert_eq!(
        m.grant_role("reader", "zed"),
        Err(ManageError::UserNotFound("zed".into()))
    );
    assert_eq!(
        m.revoke_role("reader", "zed"),
        Err(ManageError::UserNotFound("zed".into()))
    );
    assert!(matches!(
        m.grant_role(PUBLIC, "ann"),
        Err(ManageError::PublicRole { .. })
    ));
    assert!(matches!(
        m.revoke_role(PUBLIC, "ann"),
        Err(ManageError::PublicRole { .. })
    ));
}

#[test]
fn a_dropped_user_leaves_its_roles() {
    let m = with_users();
    m.grant_role("reader", "ann").unwrap();
    m.drop_user("ann", false).unwrap();
    let reader = m.roles().into_iter().find(|r| r.name == "reader").unwrap();
    assert!(reader.users.is_empty());
}

// ── Privileges ───────────────────────────────────────────────────────

#[test]
fn grant_deny_and_revoke_change_a_roles_privileges() {
    let m = manager();
    m.create_role("qa", false, None).unwrap();
    apply(&m, "GRANT MATCH {*} ON GRAPH bugs NODES Bug TO qa");
    apply(&m, "DENY READ {details} ON GRAPH bugs NODES Bug TO qa");
    let held = m.privileges(&PrivilegeFilter::Role("qa".into()));
    assert_eq!(held.len(), 2);
    assert_eq!(
        held.iter().map(|e| e.to_command()).collect::<Vec<_>>(),
        [
            "GRANT MATCH {*} ON GRAPH bugs NODE Bug TO `qa`",
            "DENY READ {details} ON GRAPH bugs NODE Bug TO `qa`",
        ]
    );
    // Granting what is held changes nothing.
    apply(&m, "GRANT MATCH {*} ON GRAPH bugs NODES Bug TO qa");
    assert_eq!(m.privileges(&PrivilegeFilter::Role("qa".into())).len(), 2);
    // REVOKE GRANT leaves the deny; REVOKE removes both.
    apply(&m, "GRANT READ {details} ON GRAPH bugs NODES Bug TO qa");
    apply(
        &m,
        "REVOKE GRANT READ {details} ON GRAPH bugs NODES Bug FROM qa",
    );
    let left = m.privileges(&PrivilegeFilter::Role("qa".into()));
    assert!(left.iter().any(|e| e.access == Access::Deny));
    apply(&m, "GRANT READ {details} ON GRAPH bugs NODES Bug TO qa");
    apply(&m, "REVOKE READ {details} ON GRAPH bugs NODES Bug FROM qa");
    assert_eq!(m.privileges(&PrivilegeFilter::Role("qa".into())).len(), 1);
    // Revoking what is not held is not an error.
    apply(&m, "REVOKE WRITE ON GRAPH * FROM qa");
}

#[test]
fn privileges_go_to_existing_roles_only() {
    let m = manager();
    assert_eq!(
        m.grant(
            Access::Grant,
            &privileges("GRANT WRITE ON GRAPH * TO x"),
            "ghost"
        ),
        Err(ManageError::RoleNotFound("ghost".into()))
    );
    assert_eq!(
        m.revoke(None, &privileges("GRANT WRITE ON GRAPH * TO x"), "ghost"),
        Err(ManageError::RoleNotFound("ghost".into()))
    );
}

#[test]
fn a_users_privileges_are_those_of_its_roles_and_public() {
    let m = with_users();
    m.grant_role("reader", "ann").unwrap();
    let ann = m.privileges(&PrivilegeFilter::User("ann".into()));
    let roles: std::collections::BTreeSet<&str> = ann.iter().map(|e| e.role.as_str()).collect();
    assert_eq!(roles.into_iter().collect::<Vec<_>>(), [PUBLIC, "reader"]);
    assert!(m
        .privileges(&PrivilegeFilter::User("zed".into()))
        .is_empty());
    let all = m.privileges(&PrivilegeFilter::All);
    assert!(all.len() > ann.len());
}

// ── Deciding ─────────────────────────────────────────────────────────

#[test]
fn what_a_user_may_do_follows_its_roles() {
    // IT task manager: ann reads tasks, bob edits them, a sealed board is
    // hidden from everyone but admins.
    let m = with_users();
    m.grant_role("reader", "ann").unwrap();
    m.grant_role("editor", "bob").unwrap();
    assert_eq!(m.decide("ann", &read("tasks")), Decision::Granted);
    assert_eq!(m.decide("ann", &create()), Decision::NotGranted);
    assert_eq!(m.decide("bob", &create()), Decision::Granted);

    m.create_role("sealed", false, None).unwrap();
    apply(&m, "DENY TRAVERSE ON GRAPH tasks NODES Task TO sealed");
    m.grant_role("sealed", "bob").unwrap();
    let traverse = Request {
        action: Action::Traverse,
        ..read("tasks")
    };
    assert_eq!(m.decide("bob", &traverse), Decision::Denied, "deny wins");
    assert_eq!(m.decide("ann", &traverse), Decision::Granted);
    assert_eq!(m.decide("nobody", &traverse), Decision::NotGranted);
}

#[test]
fn a_change_to_a_role_applies_to_its_users_at_once() {
    let m = with_users();
    m.grant_role("reader", "ann").unwrap();
    apply(&m, "DENY READ {title} ON GRAPH * TO reader");
    assert_eq!(m.decide("ann", &read("tasks")), Decision::Denied);
    apply(&m, "REVOKE DENY READ {title} ON GRAPH * FROM reader");
    assert_eq!(m.decide("ann", &read("tasks")), Decision::Granted);
}

// ── Listing as commands ──────────────────────────────────────────────

#[test]
fn the_privileges_as_commands_recreate_themselves_on_an_empty_catalog() {
    let m = with_users();
    m.create_role("qa", false, None).unwrap();
    for text in [
        "GRANT MATCH {*} ON GRAPH bugs NODES Bug, Feature TO qa",
        "DENY READ {`security details`} ON GRAPH bugs NODES Bug TO qa",
        "GRANT SET LABEL Triaged ON GRAPH bugs TO qa",
        "GRANT TRANSACTION MANAGEMENT (ann) ON HOME DATABASE TO qa",
        "DENY EXECUTE PROCEDURE apoc.export.* ON DBMS TO qa",
        "DENY STOP ON DATABASE * TO admin",
    ] {
        apply(&m, text);
    }
    m.drop_role("publisher", false).unwrap();
    let before = m.privileges(&PrivilegeFilter::All);
    let commands = m.as_commands(&PrivilegeFilter::All);
    assert_eq!(commands.len(), before.len());

    // An empty catalog with the same roles and none of their privileges.
    let fresh = manager();
    for role in fresh.roles() {
        if role.name != PUBLIC {
            fresh.drop_role(&role.name, false).unwrap();
        }
    }
    fresh
        .revoke(
            None,
            &fresh
                .privileges(&PrivilegeFilter::Role(PUBLIC.into()))
                .into_iter()
                .map(|e| e.privilege)
                .collect::<Vec<_>>(),
            PUBLIC,
        )
        .unwrap();
    assert!(fresh.privileges(&PrivilegeFilter::All).is_empty());
    for role in m.roles() {
        if role.name != PUBLIC {
            fresh.create_role(&role.name, false, None).unwrap();
        }
    }
    for command in &commands {
        apply(&fresh, command);
    }
    assert_eq!(fresh.privileges(&PrivilegeFilter::All), before);
}

// ── Storage ──────────────────────────────────────────────────────────

#[test]
fn roles_and_privileges_survive_reopening_a_file_store() {
    let dir = TmpDir::new();
    {
        let m = AuthManager::open(Arc::new(FileStore::new(dir.file()))).unwrap();
        m.create_user(NewUser::new("ann", "ann-password")).unwrap();
        m.create_role("qa", false, None).unwrap();
        apply(&m, "DENY READ {details} ON GRAPH bugs NODES Bug TO qa");
        m.grant_role("qa", "ann").unwrap();
        m.drop_role("architect", false).unwrap();
    }
    let m = AuthManager::open(Arc::new(FileStore::new(dir.file()))).unwrap();
    assert_eq!(m.user_roles("ann").unwrap(), [PUBLIC, "qa"]);
    assert_eq!(
        m.as_commands(&PrivilegeFilter::Role("qa".into())),
        ["DENY READ {details} ON GRAPH bugs NODE Bug TO `qa`"]
    );
    assert!(
        !names(&m).contains(&"architect".to_string()),
        "a drop is kept"
    );
}

#[test]
fn a_catalog_from_before_roles_gives_its_users_the_admin_role() {
    // Before roles existed, every user could do everything: upgrading keeps
    // that by making them administrators.
    let dir = TmpDir::new();
    let old = AuthManager::open(Arc::new(FileStore::new(dir.file()))).unwrap();
    old.create_user(NewUser::new("ann", "ann-password"))
        .unwrap();
    drop(old);
    let mut json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.file()).unwrap()).unwrap();
    json["format_version"] = 1.into();
    json.as_object_mut().unwrap().remove("roles");
    json["users"]["ann"]
        .as_object_mut()
        .unwrap()
        .remove("roles");
    fs::write(dir.file(), json.to_string()).unwrap();

    let m = AuthManager::open(Arc::new(FileStore::new(dir.file()))).unwrap();
    assert_eq!(m.user_roles("ann").unwrap(), [PUBLIC, "admin"]);
    assert_eq!(names(&m).len(), BUILTIN_ROLES.len());
    // The upgraded catalog is saved in the current format at once.
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.file()).unwrap()).unwrap();
    assert_eq!(saved["format_version"], 2);
}

// ── Security contexts ────────────────────────────────────────────────

#[test]
fn a_users_security_context_holds_its_roles_privileges() {
    let m = with_users();
    m.grant_role("reader", "ann").unwrap();
    let ann = m.security_context("ann", "tasks");
    assert!(ann.reads_everything("tasks"));
    assert!(!ann.writes_everything("tasks"));
    let bob = m.security_context("bob", "tasks");
    assert!(bob.can_access("tasks"), "PUBLIC reaches the home database");
    assert!(!bob.can_access("erp"));
    assert!(!bob.can_traverse_node("tasks", &["Task"]));
    let nobody = m.security_context("nobody", "tasks");
    assert!(!nobody.is_unrestricted());
    assert!(
        !nobody.can_access("tasks"),
        "an unknown user may do nothing"
    );
}

#[test]
fn every_change_moves_the_generation_on() {
    let m = with_users();
    let g0 = m.generation();
    m.grant_role("reader", "ann").unwrap();
    let g1 = m.generation();
    assert!(g1 > g0);
    apply(&m, "DENY TRAVERSE ON GRAPH * NODES Secret TO reader");
    assert!(m.generation() > g1);
    // A change that changes nothing keeps it.
    let g2 = m.generation();
    m.grant_role("reader", "ann").unwrap();
    assert_eq!(m.generation(), g2);
    // A context built after the change sees it.
    assert!(!m
        .security_context("ann", "tasks")
        .can_traverse_node("tasks", &["Secret"]));
}
