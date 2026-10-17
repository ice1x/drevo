//! Cypher user-management commands (Neo4j 5 syntax), recognised before the
//! graph parser: `CREATE USER`, `ALTER USER`, `DROP USER`, `SHOW USERS`,
//! `SHOW CURRENT USER` and `ALTER CURRENT USER SET PASSWORD`.

use drevo::cypher::security_admin::{parse, Secret, SecurityCommand, UserStatus};

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

fn lit(s: &str) -> Secret {
    Secret::Literal(s.to_string())
}

// ── CREATE USER ──────────────────────────────────────────────────────

#[test]
fn create_user_with_a_password() {
    assert_eq!(
        ok("CREATE USER ann SET PASSWORD 'first-password'"),
        SecurityCommand::CreateUser {
            name: "ann".into(),
            password: lit("first-password"),
            change_required: None,
            status: None,
            if_not_exists: false,
        }
    );
}

#[test]
fn keywords_are_case_insensitive_and_a_trailing_semicolon_is_allowed() {
    assert_eq!(
        ok("create user ann set plaintext password 'pw-12345';"),
        ok("CREATE USER ann SET PASSWORD 'pw-12345'")
    );
}

#[test]
fn create_user_options() {
    assert_eq!(
        ok("CREATE USER ann IF NOT EXISTS SET PASSWORD 'pw-12345' CHANGE NOT REQUIRED SET STATUS SUSPENDED"),
        SecurityCommand::CreateUser {
            name: "ann".into(),
            password: lit("pw-12345"),
            change_required: Some(false),
            status: Some(UserStatus::Suspended),
            if_not_exists: true,
        }
    );
    let SecurityCommand::CreateUser {
        change_required,
        status,
        ..
    } = ok(
        "CREATE USER ann SET PASSWORD 'pw-12345' SET PASSWORD CHANGE REQUIRED SET STATUS ACTIVE",
    )
    else {
        panic!()
    };
    assert_eq!(change_required, Some(true));
    assert_eq!(status, Some(UserStatus::Active));
}

#[test]
fn passwords_may_be_double_quoted_and_escaped_or_a_parameter() {
    let password = |q: &str| match ok(q) {
        SecurityCommand::CreateUser { password, .. } => password,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        password(r#"CREATE USER ann SET PASSWORD "double-quoted""#),
        lit("double-quoted")
    );
    assert_eq!(
        password(r"CREATE USER ann SET PASSWORD 'it\'s secret'"),
        lit("it's secret")
    );
    assert_eq!(
        password(r#"CREATE USER ann SET PASSWORD "say \"hi\" now""#),
        lit(r#"say "hi" now"#)
    );
    assert_eq!(
        password(r"CREATE USER ann SET PASSWORD 'back\\slash'"),
        lit(r"back\slash")
    );
    assert_eq!(
        password("CREATE USER ann SET PASSWORD $pw"),
        Secret::Param("pw".into())
    );
}

#[test]
fn a_name_may_be_quoted_with_backticks() {
    let SecurityCommand::CreateUser { name, .. } =
        ok("CREATE USER `ann.lee@x.io` SET PASSWORD 'pw-12345'")
    else {
        panic!()
    };
    assert_eq!(name, "ann.lee@x.io");
    let SecurityCommand::DropUser { name, .. } = ok("DROP USER ann.lee@x.io") else {
        panic!()
    };
    assert_eq!(name, "ann.lee@x.io");
}

#[test]
fn malformed_create_user_is_a_syntax_error() {
    for (query, says) in [
        ("CREATE USER ann", "PASSWORD"),
        ("CREATE USER SET PASSWORD 'x'", "user name"),
        ("CREATE USER ann SET PASSWORD", "password"),
        ("CREATE USER ann SET PASSWORD 'unterminated", "unterminated"),
        (
            "CREATE USER ann SET PASSWORD 'pw-12345' SET COLOUR 'blue'",
            "COLOUR",
        ),
        (
            "CREATE USER ann SET PASSWORD 'pw-12345' CHANGE MAYBE REQUIRED",
            "REQUIRED",
        ),
        ("CREATE USER ann SET STATUS ACTIVE", "PASSWORD"),
        (
            "CREATE USER ann SET PASSWORD 'pw-12345' SET STATUS SLEEPY",
            "ACTIVE",
        ),
    ] {
        let message = err(query);
        assert!(
            message.contains(says),
            "{query:?}: {message:?} should mention {says:?}"
        );
    }
}

#[test]
fn unsupported_forms_say_so() {
    assert!(err("CREATE USER ann SET ENCRYPTED PASSWORD '1,abc,def'").contains("ENCRYPTED"));
    assert!(err("CREATE USER ann SET PASSWORD 'pw-12345' SET HOME DATABASE x").contains("HOME"));
    assert!(err("CREATE OR REPLACE USER ann SET PASSWORD 'pw-12345'").contains("OR REPLACE"));
}

#[test]
fn a_syntax_error_never_echoes_the_password() {
    for query in [
        "CREATE USER ann SET PASSWORD 'hunter2-secret' SET COLOUR 'blue'",
        "CREATE USER ann SET PASSWORD 'hunter2-secret' bogus",
        "ALTER CURRENT USER SET PASSWORD FROM 'hunter2-secret' INTO 'x'",
    ] {
        let message = err(query);
        assert!(!message.contains("hunter2"), "{query:?}: {message:?}");
    }
}

// ── ALTER USER / DROP USER ───────────────────────────────────────────

#[test]
fn alter_user_sets_any_combination() {
    assert_eq!(
        ok("ALTER USER ann SET PASSWORD 'pw-12345' CHANGE NOT REQUIRED SET STATUS ACTIVE"),
        SecurityCommand::AlterUser {
            name: "ann".into(),
            if_exists: false,
            password: Some(lit("pw-12345")),
            change_required: Some(false),
            status: Some(UserStatus::Active),
        }
    );
    assert_eq!(
        ok("ALTER USER ann IF EXISTS SET PASSWORD CHANGE REQUIRED"),
        SecurityCommand::AlterUser {
            name: "ann".into(),
            if_exists: true,
            password: None,
            change_required: Some(true),
            status: None,
        }
    );
    assert_eq!(
        ok("ALTER USER ann SET STATUS SUSPENDED"),
        SecurityCommand::AlterUser {
            name: "ann".into(),
            if_exists: false,
            password: None,
            change_required: None,
            status: Some(UserStatus::Suspended),
        }
    );
}

#[test]
fn alter_user_without_a_change_is_a_syntax_error() {
    assert!(err("ALTER USER ann").contains("SET"));
    assert!(err("ALTER USER ann SET STATUS ACTIVE SET STATUS SUSPENDED").contains("STATUS"));
}

#[test]
fn drop_user() {
    assert_eq!(
        ok("DROP USER ann"),
        SecurityCommand::DropUser {
            name: "ann".into(),
            if_exists: false
        }
    );
    assert_eq!(
        ok("drop user ann if exists;"),
        SecurityCommand::DropUser {
            name: "ann".into(),
            if_exists: true
        }
    );
    assert!(err("DROP USER").contains("user name"));
    assert!(err("DROP USER ann now").contains("now"));
}

// ── SHOW / current user ──────────────────────────────────────────────

#[test]
fn show_users_and_current_user() {
    assert_eq!(ok("SHOW USERS"), SecurityCommand::ShowUsers);
    assert_eq!(ok("show users;"), SecurityCommand::ShowUsers);
    assert_eq!(ok("SHOW CURRENT USER"), SecurityCommand::ShowCurrentUser);
    assert!(err("SHOW USERS YIELD user").contains("YIELD"));
}

#[test]
fn alter_current_user_password() {
    assert_eq!(
        ok("ALTER CURRENT USER SET PASSWORD FROM 'old-password' TO 'new-password'"),
        SecurityCommand::AlterCurrentUserPassword {
            current: lit("old-password"),
            new: lit("new-password"),
        }
    );
    assert_eq!(
        ok("ALTER CURRENT USER SET PASSWORD FROM $old TO $new"),
        SecurityCommand::AlterCurrentUserPassword {
            current: Secret::Param("old".into()),
            new: Secret::Param("new".into()),
        }
    );
    assert!(err("ALTER CURRENT USER SET PASSWORD 'x'").contains("FROM"));
}

// ── Everything else is not a user command ────────────────────────────

#[test]
fn graph_queries_and_other_admin_commands_pass_through() {
    for query in [
        "CREATE (u:User {name: 'ann'})",
        "CREATE (:USER)",
        "MATCH (u:User) RETURN u",
        "MATCH (n) DETACH DELETE n",
        "SHOW DATABASES",
        "CREATE DATABASE users",
        "SHOW INDEXES",
        "CREATE INDEX FOR (u:User) ON (u.name)",
        "",
        "   ",
    ] {
        assert!(parse(query).is_none(), "{query:?} is not a user command");
    }
}

#[test]
fn debug_output_redacts_literal_passwords() {
    let text = format!(
        "{:?} {:?}",
        ok("CREATE USER ann SET PASSWORD 'hunter2-secret'"),
        ok("ALTER CURRENT USER SET PASSWORD FROM 'hunter2-old' TO 'hunter2-new'")
    );
    assert!(!text.contains("hunter2"), "{text}");
    assert!(text.contains("ann"));
}
