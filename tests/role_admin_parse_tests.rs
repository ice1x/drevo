//! Cypher role and privilege commands (Neo4j 5 syntax), recognised before the
//! graph parser: `CREATE` / `DROP` / `RENAME ROLE`, `GRANT` / `REVOKE ROLE`,
//! `SHOW ROLES`, `GRANT` / `DENY` / `REVOKE` privileges, `SHOW PRIVILEGES`
//! and `SHOW SUPPORTED PRIVILEGES`.

use drevo::cypher::security_admin::{parse, PrivilegeScope, SecurityCommand};
use drevo_auth::privileges::{Access, Op};

fn ok(query: &str) -> SecurityCommand {
    match parse(query) {
        Some(Ok(cmd)) => cmd,
        other => panic!("{query:?} should parse, got {other:?}"),
    }
}

fn err(query: &str) -> String {
    match parse(query) {
        Some(Err(message)) => message,
        other => panic!("{query:?} should be a syntax error, got {other:?}"),
    }
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

// ── Roles ────────────────────────────────────────────────────────────

#[test]
fn create_role_forms() {
    assert_eq!(
        ok("CREATE ROLE triage"),
        SecurityCommand::CreateRole {
            name: "triage".into(),
            if_not_exists: false,
            copy_of: None,
        }
    );
    assert_eq!(
        ok("create role `bug triage` if not exists as copy of reader;"),
        SecurityCommand::CreateRole {
            name: "bug triage".into(),
            if_not_exists: true,
            copy_of: Some("reader".into()),
        }
    );
    assert!(err("CREATE OR REPLACE ROLE triage").contains("not supported"));
    assert!(err("CREATE ROLE").contains("role name"));
    assert!(err("CREATE ROLE triage AS reader").contains("COPY OF"));
}

#[test]
fn drop_and_rename_role() {
    assert_eq!(
        ok("DROP ROLE triage IF EXISTS"),
        SecurityCommand::DropRole {
            name: "triage".into(),
            if_exists: true,
        }
    );
    assert_eq!(
        ok("RENAME ROLE triage IF EXISTS TO testers"),
        SecurityCommand::RenameRole {
            from: "triage".into(),
            to: "testers".into(),
            if_exists: true,
        }
    );
    assert_eq!(
        ok("RENAME ROLE triage TO testers"),
        SecurityCommand::RenameRole {
            from: "triage".into(),
            to: "testers".into(),
            if_exists: false,
        }
    );
    assert!(err("RENAME ROLE triage testers").contains("TO"));
}

#[test]
fn grant_and_revoke_roles_to_users() {
    assert_eq!(
        ok("GRANT ROLE reader TO ann"),
        SecurityCommand::GrantRoles {
            roles: names(&["reader"]),
            users: names(&["ann"]),
        }
    );
    assert_eq!(
        ok("GRANT ROLES reader, editor TO ann, `bob.smith@corp`"),
        SecurityCommand::GrantRoles {
            roles: names(&["reader", "editor"]),
            users: names(&["ann", "bob.smith@corp"]),
        }
    );
    assert_eq!(
        ok("REVOKE ROLE editor FROM bob"),
        SecurityCommand::RevokeRoles {
            roles: names(&["editor"]),
            users: names(&["bob"]),
        }
    );
    assert!(err("GRANT ROLE reader FROM ann").contains("TO"));
    assert!(err("REVOKE ROLE reader TO ann").contains("FROM"));
}

#[test]
fn show_roles_forms() {
    assert_eq!(
        ok("SHOW ROLES"),
        SecurityCommand::ShowRoles {
            populated: false,
            with_users: false,
        }
    );
    assert_eq!(
        ok("SHOW ALL ROLES"),
        SecurityCommand::ShowRoles {
            populated: false,
            with_users: false,
        }
    );
    assert_eq!(
        ok("SHOW POPULATED ROLES WITH USERS"),
        SecurityCommand::ShowRoles {
            populated: true,
            with_users: true,
        }
    );
    assert!(err("SHOW ROLES YIELD role").contains("YIELD"));
}

// ── Privileges ───────────────────────────────────────────────────────

#[test]
fn privilege_commands_are_read_by_drevo_auth() {
    let SecurityCommand::Privilege(c) =
        ok("GRANT MATCH {*} ON GRAPH bugs NODES Bug, Feature TO triage;")
    else {
        panic!("not a privilege command");
    };
    assert_eq!(c.op, Op::Grant);
    assert_eq!(c.roles, ["triage"]);
    assert_eq!(c.privileges.len(), 2);
    let SecurityCommand::Privilege(c) = ok("deny write on graph * to triage") else {
        panic!();
    };
    assert_eq!(c.op, Op::Deny);
    let SecurityCommand::Privilege(c) = ok("REVOKE DENY WRITE ON GRAPH * FROM triage") else {
        panic!();
    };
    assert_eq!(c.op, Op::Revoke(Some(Access::Deny)));
    assert!(err("GRANT FLY ON GRAPH * TO triage").contains("privilege"));
    assert!(err("DENY TRAVERSE ON GRAPH *").contains("TO"));
}

#[test]
fn show_privileges_forms() {
    assert_eq!(
        ok("SHOW PRIVILEGES"),
        SecurityCommand::ShowPrivileges {
            scope: PrivilegeScope::All,
            as_commands: None,
        }
    );
    assert_eq!(
        ok("SHOW ALL PRIVILEGES AS COMMANDS"),
        SecurityCommand::ShowPrivileges {
            scope: PrivilegeScope::All,
            as_commands: Some(false),
        }
    );
    assert_eq!(
        ok("SHOW ROLE reader, editor PRIVILEGES AS REVOKE COMMANDS"),
        SecurityCommand::ShowPrivileges {
            scope: PrivilegeScope::Roles(names(&["reader", "editor"])),
            as_commands: Some(true),
        }
    );
    assert_eq!(
        ok("SHOW ROLES triage PRIVILEGE AS COMMAND"),
        SecurityCommand::ShowPrivileges {
            scope: PrivilegeScope::Roles(names(&["triage"])),
            as_commands: Some(false),
        }
    );
    assert_eq!(
        ok("SHOW USER ann PRIVILEGES"),
        SecurityCommand::ShowPrivileges {
            scope: PrivilegeScope::Users(names(&["ann"])),
            as_commands: None,
        }
    );
    assert_eq!(
        ok("SHOW USER PRIVILEGES"),
        SecurityCommand::ShowPrivileges {
            scope: PrivilegeScope::CurrentUser,
            as_commands: None,
        }
    );
    assert_eq!(
        ok("SHOW SUPPORTED PRIVILEGES"),
        SecurityCommand::ShowSupportedPrivileges
    );
    assert!(err("SHOW PRIVILEGES AS TABLE").contains("COMMANDS"));
    assert!(err("SHOW PRIVILEGES WHERE role = 'x'").contains("WHERE"));
}

// ── Graph queries are left alone ─────────────────────────────────────

#[test]
fn graph_queries_and_other_admin_commands_are_not_claimed() {
    for query in [
        "MATCH (r:Role) RETURN r",
        "CREATE (r:Role {name: 'x'})",
        "MATCH (n) SET n.role = 'reader'",
        "SHOW DATABASES",
        "SHOW INDEXES",
        "RETURN 'GRANT' AS g",
    ] {
        assert!(parse(query).is_none(), "{query}");
    }
}
