//! Neo4j-style privileges: parsing `GRANT` / `DENY` / `REVOKE`, writing them
//! back as commands, the built-in roles, and deciding requests with deny
//! overriding grant.

use drevo_auth::privileges::{
    builtin_privileges, decide, parse, Access, Action, Command, Decision, Element, Entry, Op,
    Privilege, Request, BUILTIN_ROLES,
};

fn command(text: &str) -> Command {
    parse(text).unwrap_or_else(|e| panic!("{text}: {e}"))
}

/// The privileges of a command, written back one per line.
fn bodies(text: &str) -> Vec<String> {
    command(text)
        .privileges
        .iter()
        .map(ToString::to_string)
        .collect()
}

// ── Parsing ──────────────────────────────────────────────────────────

#[test]
fn a_grant_without_a_segment_covers_nodes_and_relationships() {
    let c = command("GRANT TRAVERSE ON GRAPH * TO reader");
    assert_eq!(c.op, Op::Grant);
    assert_eq!(c.roles, ["reader"]);
    assert_eq!(
        bodies("GRANT TRAVERSE ON GRAPH * TO reader"),
        [
            "TRAVERSE ON GRAPH * NODE *",
            "TRAVERSE ON GRAPH * RELATIONSHIP *"
        ]
    );
}

#[test]
fn lists_expand_into_one_privilege_each() {
    // CBT journal: therapists read two fields of entries and sessions.
    let c = command(
        "GRANT READ {mood, situation} ON GRAPH journal NODES Entry, Session TO therapist, supervisor",
    );
    assert_eq!(c.roles, ["therapist", "supervisor"]);
    let got: Vec<String> = c.privileges.iter().map(ToString::to_string).collect();
    assert_eq!(
        got,
        [
            "READ {mood} ON GRAPH journal NODE Entry",
            "READ {situation} ON GRAPH journal NODE Entry",
            "READ {mood} ON GRAPH journal NODE Session",
            "READ {situation} ON GRAPH journal NODE Session",
        ]
    );
}

#[test]
fn keywords_are_case_insensitive_and_names_can_be_quoted() {
    let c = command("grant match {*} on graph `bug tracker` relationship `BLOCKS` to `qa team`");
    assert_eq!(c.roles, ["qa team"]);
    assert_eq!(
        c.privileges[0].to_string(),
        "MATCH {*} ON GRAPH `bug tracker` RELATIONSHIP BLOCKS"
    );
    let c = command("GRANT TRAVERSE ON GRAPH `odd``name` TO r");
    assert_eq!(
        c.privileges[0].to_string(),
        "TRAVERSE ON GRAPH `odd``name` NODE *"
    );
}

#[test]
fn every_graph_privilege_parses() {
    for (text, expected) in [
        (
            "GRANT CREATE ON GRAPH * NODES Task TO r",
            "CREATE ON GRAPH * NODE Task",
        ),
        (
            "GRANT DELETE ON GRAPH tasks RELATIONSHIPS * TO r",
            "DELETE ON GRAPH tasks RELATIONSHIP *",
        ),
        (
            "GRANT SET LABEL Done ON GRAPH * TO r",
            "SET LABEL Done ON GRAPH *",
        ),
        (
            "GRANT REMOVE LABEL * ON GRAPH * TO r",
            "REMOVE LABEL * ON GRAPH *",
        ),
        (
            "GRANT SET PROPERTY {status} ON GRAPH * NODES Task TO r",
            "SET PROPERTY {status} ON GRAPH * NODE Task",
        ),
        (
            "GRANT MERGE {*} ON GRAPH * NODES Task TO r",
            "MERGE {*} ON GRAPH * NODE Task",
        ),
        ("GRANT WRITE ON GRAPH * TO r", "WRITE ON GRAPH *"),
        (
            "GRANT ALL GRAPH PRIVILEGES ON GRAPH erp TO r",
            "ALL GRAPH PRIVILEGES ON GRAPH erp",
        ),
        (
            "GRANT ALL PRIVILEGES ON GRAPH erp TO r",
            "ALL GRAPH PRIVILEGES ON GRAPH erp",
        ),
        (
            "GRANT TRAVERSE ON HOME GRAPH NODES * TO r",
            "TRAVERSE ON HOME GRAPH NODE *",
        ),
    ] {
        assert_eq!(bodies(text), [expected], "{text}");
    }
}

#[test]
fn elements_means_nodes_and_relationships() {
    assert_eq!(
        bodies("DENY DELETE ON GRAPH * ELEMENTS Audit TO r"),
        [
            "DELETE ON GRAPH * NODE Audit",
            "DELETE ON GRAPH * RELATIONSHIP Audit"
        ]
    );
}

#[test]
fn database_privileges_parse() {
    for (text, expected) in [
        (
            "GRANT ACCESS ON HOME DATABASE TO r",
            vec!["ACCESS ON HOME DATABASE"],
        ),
        (
            "GRANT ACCESS ON DATABASES erp, crm TO r",
            vec!["ACCESS ON DATABASE erp", "ACCESS ON DATABASE crm"],
        ),
        (
            "GRANT START ON DATABASE * TO r",
            vec!["START ON DATABASE *"],
        ),
        ("GRANT STOP ON DATABASE * TO r", vec!["STOP ON DATABASE *"]),
        (
            "GRANT INDEX ON DATABASE * TO r",
            vec!["INDEX MANAGEMENT ON DATABASE *"],
        ),
        (
            "GRANT INDEX MANAGEMENT ON DATABASE * TO r",
            vec!["INDEX MANAGEMENT ON DATABASE *"],
        ),
        (
            "GRANT CONSTRAINTS ON DATABASE * TO r",
            vec!["CONSTRAINT MANAGEMENT ON DATABASE *"],
        ),
        (
            "GRANT NAME ON DATABASE * TO r",
            vec!["NAME MANAGEMENT ON DATABASE *"],
        ),
        (
            "GRANT TRANSACTION ON DATABASE * TO r",
            vec!["TRANSACTION MANAGEMENT (*) ON DATABASE *"],
        ),
        (
            "GRANT TRANSACTION MANAGEMENT (ann, bob) ON DATABASE erp TO r",
            vec![
                "TRANSACTION MANAGEMENT (ann) ON DATABASE erp",
                "TRANSACTION MANAGEMENT (bob) ON DATABASE erp",
            ],
        ),
        (
            "GRANT ALL DATABASE PRIVILEGES ON DATABASE * TO r",
            vec!["ALL DATABASE PRIVILEGES ON DATABASE *"],
        ),
        (
            "GRANT ALL PRIVILEGES ON DATABASE erp TO r",
            vec!["ALL DATABASE PRIVILEGES ON DATABASE erp"],
        ),
    ] {
        assert_eq!(bodies(text), expected, "{text}");
    }
}

#[test]
fn dbms_privileges_parse() {
    for (text, expected) in [
        (
            "GRANT USER MANAGEMENT ON DBMS TO r",
            vec!["USER MANAGEMENT ON DBMS"],
        ),
        (
            "GRANT ROLE MANAGEMENT ON DBMS TO r",
            vec!["ROLE MANAGEMENT ON DBMS"],
        ),
        (
            "GRANT PRIVILEGE MANAGEMENT ON DBMS TO r",
            vec!["PRIVILEGE MANAGEMENT ON DBMS"],
        ),
        (
            "GRANT DATABASE MANAGEMENT ON DBMS TO r",
            vec!["DATABASE MANAGEMENT ON DBMS"],
        ),
        (
            "GRANT EXECUTE PROCEDURES apoc.*, db.labels ON DBMS TO r",
            vec![
                "EXECUTE PROCEDURE apoc.* ON DBMS",
                "EXECUTE PROCEDURE db.labels ON DBMS",
            ],
        ),
        (
            "GRANT EXECUTE FUNCTION * ON DBMS TO r",
            vec!["EXECUTE FUNCTION * ON DBMS"],
        ),
        (
            "GRANT EXECUTE ADMIN PROCEDURES ON DBMS TO r",
            vec!["EXECUTE ADMIN PROCEDURES ON DBMS"],
        ),
        (
            "GRANT EXECUTE ADMINISTRATOR PROCEDURES ON DBMS TO r",
            vec!["EXECUTE ADMIN PROCEDURES ON DBMS"],
        ),
        (
            "GRANT ALL DBMS PRIVILEGES ON DBMS TO r",
            vec!["ALL DBMS PRIVILEGES ON DBMS"],
        ),
        (
            "GRANT ALL ON DBMS TO r",
            vec!["ALL DBMS PRIVILEGES ON DBMS"],
        ),
    ] {
        assert_eq!(bodies(text), expected, "{text}");
    }
}

#[test]
fn revoke_names_what_it_removes() {
    let c = command("REVOKE GRANT TRAVERSE ON GRAPH * NODES * FROM r");
    assert_eq!(c.op, Op::Revoke(Some(Access::Grant)));
    let c = command("REVOKE DENY WRITE ON GRAPH * FROM r1, r2");
    assert_eq!(c.op, Op::Revoke(Some(Access::Deny)));
    assert_eq!(c.roles, ["r1", "r2"]);
    let c = command("REVOKE ACCESS ON DATABASE * FROM r");
    assert_eq!(c.op, Op::Revoke(None));
    assert_eq!(command("DENY ACCESS ON DATABASE * TO r").op, Op::Deny);
}

/// Examples in the style of Neo4j 5's privilege documentation.
#[test]
fn neo4j_documentation_examples_parse() {
    for text in [
        "GRANT TRAVERSE ON GRAPH neo4j NODES Post TO regularUsers",
        "GRANT TRAVERSE ON GRAPH * RELATIONSHIPS * TO regularUsers",
        "DENY TRAVERSE ON GRAPH neo4j NODES Payments TO regularUsers",
        "GRANT READ {*} ON GRAPH neo4j NODES Post TO regularUsers",
        "DENY READ {secret} ON GRAPH neo4j NODES * TO regularUsers",
        "GRANT MATCH {*} ON GRAPH neo4j ELEMENTS Post TO regularUsers",
        "DENY MATCH {*} ON GRAPH neo4j NODES User WHERE x = 1 TO r",
        "GRANT CREATE ON GRAPH * NODES Foo TO roleAdder",
        "DENY DELETE ON GRAPH neo4j RELATIONSHIPS bar TO roleAdder",
        "GRANT SET LABEL foo ON GRAPH * TO labelAdder",
        "DENY REMOVE LABEL bar ON GRAPH * TO labelRemover",
        "GRANT SET PROPERTY {age} ON GRAPH * NODES Person TO propSetter",
        "GRANT MERGE {*} ON GRAPH * NODES * TO merger",
        "GRANT WRITE ON GRAPH * TO writer",
        "DENY ALL GRAPH PRIVILEGES ON GRAPH neo4j TO noAccess",
        "GRANT ACCESS ON DATABASE * TO regularUsers",
        "DENY ACCESS ON DATABASE neo4j TO regularUsers",
        "GRANT START ON DATABASE * TO databaseStarter",
        "GRANT INDEX MANAGEMENT ON DATABASE neo4j TO indexManager",
        "GRANT CONSTRAINT MANAGEMENT ON DATABASE * TO constraintManager",
        "GRANT NAME MANAGEMENT ON DATABASE * TO nameManager",
        "GRANT TRANSACTION MANAGEMENT (*) ON DATABASE * TO transactionManager",
        "GRANT ALL DATABASE PRIVILEGES ON DATABASE neo4j TO databaseAdmin",
        "GRANT USER MANAGEMENT ON DBMS TO userManager",
        "GRANT ROLE MANAGEMENT ON DBMS TO roleManager",
        "GRANT PRIVILEGE MANAGEMENT ON DBMS TO privilegeManager",
        "GRANT EXECUTE PROCEDURE db.schema.* ON DBMS TO procedureExecutor",
        "GRANT EXECUTE ADMIN PROCEDURES ON DBMS TO adminProcedureExecutor",
        "GRANT ALL DBMS PRIVILEGES ON DBMS TO dbmsManager",
    ] {
        if text.contains("WHERE") {
            // Property-based segments are not supported: a clear error.
            let err = parse(text).expect_err(text).to_string();
            assert!(err.contains("WHERE"), "{text}: {err}");
        } else {
            command(text);
        }
    }
}

#[test]
fn malformed_commands_say_what_was_expected() {
    for (text, says) in [
        ("GRANT TRAVERSE ON GRAPH *", "TO"),
        ("GRANT FLY ON GRAPH * TO r", "privilege"),
        ("GRANT WRITE ON GRAPH * NODES * TO r", "segment"),
        ("GRANT SET LABEL x ON GRAPH * NODES * TO r", "segment"),
        ("GRANT IMMUTABLE TRAVERSE ON GRAPH * TO r", "IMMUTABLE"),
        ("GRANT TRAVERSE ON DBMS TO r", "GRAPH"),
        ("GRANT ACCESS ON GRAPH * TO r", "DATABASE"),
        ("GRANT USER MANAGEMENT ON DATABASE * TO r", "DBMS"),
        ("GRANT READ ON GRAPH * TO r", "{"),
        ("GRANT READ {} ON GRAPH * TO r", "property"),
        ("GRANT TRAVERSE ON GRAPH * TO r extra", "extra"),
        ("GRANT TRAVERSE ON GRAPH * FROM r", "TO"),
        ("REVOKE TRAVERSE ON GRAPH * TO r", "FROM"),
        ("GRANT TRAVERSE ON GRAPH `unclosed TO r", "backtick"),
        ("GRANT TRAVERSE ON GRAPH * TO", "role"),
        ("SHOW USERS", "GRANT"),
    ] {
        let err = parse(text).expect_err(text).to_string();
        assert!(err.contains(says), "{text}: {err}");
    }
}

// ── Writing privileges back as commands ──────────────────────────────

#[test]
fn an_entry_is_written_as_the_command_that_grants_it() {
    let privilege =
        command("DENY READ {ssn} ON GRAPH hr NODES Employee TO r").privileges[0].clone();
    let entry = Entry {
        access: Access::Deny,
        privilege,
        role: "hr reader".to_string(),
    };
    assert_eq!(
        entry.to_command(),
        "DENY READ {ssn} ON GRAPH hr NODE Employee TO `hr reader`"
    );
}

#[test]
fn every_written_command_parses_back_to_the_same_privilege() {
    let mut all: Vec<(Access, Privilege)> = Vec::new();
    for role in BUILTIN_ROLES {
        for p in builtin_privileges(role).unwrap() {
            all.push((Access::Grant, p));
        }
    }
    for text in [
        "GRANT READ {mood, `odd name`} ON GRAPH `bug tracker`, erp NODES `A B`, C TO r",
        "DENY TRAVERSE ON HOME GRAPH RELATIONSHIPS * TO r",
        "DENY SET LABEL * ON GRAPH * TO r",
        "GRANT REMOVE LABEL Done ON GRAPH tasks TO r",
        "DENY MERGE {x} ON GRAPH * ELEMENTS * TO r",
        "GRANT TRANSACTION MANAGEMENT (`a b`) ON HOME DATABASE TO r",
        "DENY EXECUTE FUNCTION apoc.text.* ON DBMS TO r",
        "GRANT ALL DATABASE PRIVILEGES ON DATABASES a, b TO r",
    ] {
        let c = command(text);
        let access = if c.op == Op::Deny {
            Access::Deny
        } else {
            Access::Grant
        };
        all.extend(c.privileges.into_iter().map(|p| (access, p)));
    }
    for (access, privilege) in all {
        let entry = Entry {
            access,
            privilege: privilege.clone(),
            role: "role `x`".to_string(),
        };
        let text = entry.to_command();
        let back = command(&text);
        assert_eq!(back.privileges, [privilege], "{text}");
        assert_eq!(back.roles, ["role `x`"], "{text}");
        let expected_op = match access {
            Access::Grant => Op::Grant,
            Access::Deny => Op::Deny,
        };
        assert_eq!(back.op, expected_op, "{text}");
    }
}

#[test]
fn privileges_describe_themselves_as_show_privileges_rows() {
    let p = &command("GRANT READ {name} ON GRAPH erp NODES Customer TO r").privileges[0];
    assert_eq!(p.action.name(), "read");
    assert_eq!(p.resource_name(), "property(name)");
    assert_eq!(p.graph_name(), "erp");
    assert_eq!(p.segment_name(), "NODE(Customer)");
    let p = &command("GRANT ACCESS ON HOME DATABASE TO r").privileges[0];
    assert_eq!(
        (
            p.action.name(),
            p.resource_name(),
            p.graph_name(),
            p.segment_name()
        ),
        (
            "access",
            "database".to_string(),
            "HOME".to_string(),
            "database".to_string()
        )
    );
    let p = &command("GRANT EXECUTE PROCEDURE apoc.* ON DBMS TO r").privileges[0];
    assert_eq!(p.action.name(), "execute");
    assert_eq!(p.segment_name(), "PROCEDURE(apoc.*)");
    assert_eq!(Access::Grant.name(), "GRANTED");
    assert_eq!(Access::Deny.name(), "DENIED");
}

// ── Built-in roles ───────────────────────────────────────────────────

fn builtin_commands(role: &str) -> Vec<String> {
    builtin_privileges(role)
        .unwrap()
        .into_iter()
        .map(|privilege| {
            Entry {
                access: Access::Grant,
                privilege,
                role: role.to_string(),
            }
            .to_command()
        })
        .collect()
}

#[test]
fn the_built_in_roles_are_neo4js() {
    assert_eq!(
        BUILTIN_ROLES,
        [
            "PUBLIC",
            "reader",
            "editor",
            "publisher",
            "architect",
            "admin"
        ]
    );
    assert_eq!(
        builtin_commands("PUBLIC"),
        [
            "GRANT ACCESS ON HOME DATABASE TO `PUBLIC`",
            "GRANT EXECUTE PROCEDURE * ON DBMS TO `PUBLIC`",
            "GRANT EXECUTE FUNCTION * ON DBMS TO `PUBLIC`",
        ]
    );
    let reader = [
        "GRANT ACCESS ON DATABASE * TO `reader`",
        "GRANT MATCH {*} ON GRAPH * NODE * TO `reader`",
        "GRANT MATCH {*} ON GRAPH * RELATIONSHIP * TO `reader`",
    ];
    assert_eq!(builtin_commands("reader"), reader);
    let editor = builtin_commands("editor");
    assert_eq!(editor.len(), 4);
    assert!(editor.contains(&"GRANT WRITE ON GRAPH * TO `editor`".to_string()));
    let publisher = builtin_commands("publisher");
    assert!(publisher.contains(&"GRANT NAME MANAGEMENT ON DATABASE * TO `publisher`".to_string()));
    assert_eq!(publisher.len(), 5);
    let architect = builtin_commands("architect");
    for c in [
        "GRANT INDEX MANAGEMENT ON DATABASE * TO `architect`",
        "GRANT CONSTRAINT MANAGEMENT ON DATABASE * TO `architect`",
    ] {
        assert!(architect.contains(&c.to_string()), "{c}");
    }
    assert_eq!(architect.len(), 7);
    let admin = builtin_commands("admin");
    for c in [
        "GRANT START ON DATABASE * TO `admin`",
        "GRANT STOP ON DATABASE * TO `admin`",
        "GRANT TRANSACTION MANAGEMENT (*) ON DATABASE * TO `admin`",
        "GRANT ALL DBMS PRIVILEGES ON DBMS TO `admin`",
    ] {
        assert!(admin.contains(&c.to_string()), "{c}");
    }
    assert_eq!(admin.len(), 11);
    assert!(builtin_privileges("nobody").is_none());
}

// ── Deciding requests ────────────────────────────────────────────────

/// The entries of `commands`, as a role would hold them.
fn entries(commands: &[&str]) -> Vec<(Access, Privilege)> {
    commands
        .iter()
        .flat_map(|text| {
            let c = command(text);
            let access = if c.op == Op::Deny {
                Access::Deny
            } else {
                Access::Grant
            };
            c.privileges.into_iter().map(move |p| (access, p))
        })
        .collect()
}

fn builtin(role: &str) -> Vec<(Access, Privilege)> {
    builtin_privileges(role)
        .unwrap()
        .into_iter()
        .map(|p| (Access::Grant, p))
        .collect()
}

fn ask(held: &[(Access, Privilege)], request: &Request<'_>) -> Decision {
    decide(held.iter().map(|(a, p)| (*a, p)), request)
}

const TASK: Element<'static> = Element::Node(&["Task"]);

fn graph(
    action: Action,
    element: Element<'static>,
    name: Option<&'static str>,
) -> Request<'static> {
    Request {
        action,
        database: "tasks",
        home: "tasks",
        element,
        name,
    }
}

#[test]
fn a_reader_reads_and_traverses_but_does_not_write() {
    let reader = builtin("reader");
    assert_eq!(
        ask(&reader, &graph(Action::Traverse, TASK, None)),
        Decision::Granted
    );
    assert_eq!(
        ask(&reader, &graph(Action::Read, TASK, Some("title"))),
        Decision::Granted
    );
    assert_eq!(
        ask(&reader, &graph(Action::Create, TASK, None)),
        Decision::NotGranted
    );
    assert!(!ask(&reader, &graph(Action::Create, TASK, None)).is_allowed());
}

#[test]
fn an_editor_writes() {
    let editor = builtin("editor");
    for action in [
        Action::Create,
        Action::Delete,
        Action::SetLabel,
        Action::RemoveLabel,
    ] {
        assert!(
            ask(&editor, &graph(action, TASK, None)).is_allowed(),
            "{action:?}"
        );
    }
    assert!(ask(&editor, &graph(Action::SetProperty, TASK, Some("status"))).is_allowed());
    assert!(!ask(
        &editor,
        &Request::database(Action::IndexManagement, "tasks", "tasks")
    )
    .is_allowed());
}

#[test]
fn an_admin_manages_users_and_runs_anything() {
    let admin = builtin("admin");
    for action in [
        Action::UserManagement,
        Action::RoleManagement,
        Action::PrivilegeManagement,
        Action::DatabaseManagement,
    ] {
        assert!(
            ask(&admin, &Request::dbms(action, None)).is_allowed(),
            "{action:?}"
        );
    }
    assert!(ask(
        &admin,
        &Request::dbms(Action::ExecuteProcedure, Some("db.labels"))
    )
    .is_allowed());
    assert!(ask(
        &admin,
        &Request::database(Action::TransactionManagement, "erp", "erp").with_name("bob")
    )
    .is_allowed());
}

#[test]
fn public_reaches_only_the_home_database() {
    let public = builtin("PUBLIC");
    assert!(ask(
        &public,
        &Request::database(Action::Access, "tasks", "tasks")
    )
    .is_allowed());
    assert!(!ask(&public, &Request::database(Action::Access, "erp", "tasks")).is_allowed());
    assert!(ask(
        &public,
        &Request::dbms(Action::ExecuteProcedure, Some("db.labels"))
    )
    .is_allowed());
}

#[test]
fn deny_beats_grant_at_every_level() {
    // Each pair: a grant, a deny that overlaps it, the request they share.
    let cases: Vec<(&str, &str, Request<'static>)> = vec![
        // The same privilege.
        (
            "GRANT TRAVERSE ON GRAPH * NODES * TO r",
            "DENY TRAVERSE ON GRAPH * NODES * TO r",
            graph(Action::Traverse, TASK, None),
        ),
        // A narrow deny inside a broad grant: one label.
        (
            "GRANT TRAVERSE ON GRAPH * NODES * TO r",
            "DENY TRAVERSE ON GRAPH * NODES Task TO r",
            graph(Action::Traverse, Element::Node(&["Task", "Urgent"]), None),
        ),
        // One property.
        (
            "GRANT MATCH {*} ON GRAPH * NODES * TO r",
            "DENY READ {secret} ON GRAPH * NODES * TO r",
            graph(Action::Read, TASK, Some("secret")),
        ),
        // One graph.
        (
            "GRANT CREATE ON GRAPH * TO r",
            "DENY CREATE ON GRAPH tasks TO r",
            graph(Action::Create, TASK, None),
        ),
        // A broad deny over a narrow grant.
        (
            "GRANT CREATE ON GRAPH tasks NODES Task TO r",
            "DENY WRITE ON GRAPH * TO r",
            graph(Action::Create, TASK, None),
        ),
        (
            "GRANT SET PROPERTY {status} ON GRAPH * NODES Task TO r",
            "DENY ALL GRAPH PRIVILEGES ON GRAPH * TO r",
            graph(Action::SetProperty, TASK, Some("status")),
        ),
        (
            "GRANT SET LABEL Done ON GRAPH * TO r",
            "DENY SET LABEL * ON GRAPH * TO r",
            graph(Action::SetLabel, TASK, Some("Done")),
        ),
        // Relationships.
        (
            "GRANT DELETE ON GRAPH * RELATIONSHIPS * TO r",
            "DENY DELETE ON GRAPH * RELATIONSHIPS BLOCKS TO r",
            graph(Action::Delete, Element::Relationship("BLOCKS"), None),
        ),
        // Databases.
        (
            "GRANT ACCESS ON DATABASE * TO r",
            "DENY ACCESS ON DATABASE tasks TO r",
            Request::database(Action::Access, "tasks", "tasks"),
        ),
        (
            "GRANT ACCESS ON DATABASE tasks TO r",
            "DENY ACCESS ON HOME DATABASE TO r",
            Request::database(Action::Access, "tasks", "tasks"),
        ),
        (
            "GRANT INDEX MANAGEMENT ON DATABASE * TO r",
            "DENY ALL DATABASE PRIVILEGES ON DATABASE * TO r",
            Request::database(Action::IndexManagement, "tasks", "tasks"),
        ),
        (
            "GRANT TRANSACTION MANAGEMENT (*) ON DATABASE * TO r",
            "DENY TRANSACTION MANAGEMENT (bob) ON DATABASE * TO r",
            Request::database(Action::TransactionManagement, "tasks", "tasks").with_name("bob"),
        ),
        // The DBMS.
        (
            "GRANT USER MANAGEMENT ON DBMS TO r",
            "DENY ALL DBMS PRIVILEGES ON DBMS TO r",
            Request::dbms(Action::UserManagement, None),
        ),
        (
            "GRANT EXECUTE PROCEDURE * ON DBMS TO r",
            "DENY EXECUTE PROCEDURE apoc.* ON DBMS TO r",
            Request::dbms(Action::ExecuteProcedure, Some("apoc.export.json")),
        ),
    ];
    for (grant, deny, request) in cases {
        assert!(
            ask(&entries(&[grant]), &request).is_allowed(),
            "the grant alone allows: {grant}"
        );
        // Order does not matter.
        assert_eq!(
            ask(&entries(&[grant, deny]), &request),
            Decision::Denied,
            "{deny}"
        );
        assert_eq!(
            ask(&entries(&[deny, grant]), &request),
            Decision::Denied,
            "{deny}"
        );
    }
}

#[test]
fn a_deny_does_not_reach_past_what_it_names() {
    let held = entries(&[
        "GRANT MATCH {*} ON GRAPH * TO r",
        "DENY READ {secret} ON GRAPH * NODES Task TO r",
        "DENY TRAVERSE ON GRAPH * NODES Sealed TO r",
        "DENY ACCESS ON DATABASE erp TO r",
    ]);
    assert!(ask(&held, &graph(Action::Read, TASK, Some("title"))).is_allowed());
    assert!(ask(
        &held,
        &graph(Action::Read, Element::Node(&["Bug"]), Some("secret"))
    )
    .is_allowed());
    assert!(ask(&held, &graph(Action::Traverse, TASK, None)).is_allowed());
    assert!(!ask(
        &held,
        &graph(Action::Traverse, Element::Node(&["Sealed"]), None)
    )
    .is_allowed());
    assert!(ask(
        &held,
        &graph(Action::Traverse, Element::Relationship("Sealed"), None)
    )
    .is_allowed());
}

#[test]
fn match_implies_traverse_and_read() {
    let held = entries(&["GRANT MATCH {title} ON GRAPH * NODES Task TO r"]);
    assert!(ask(&held, &graph(Action::Traverse, TASK, None)).is_allowed());
    assert!(ask(&held, &graph(Action::Read, TASK, Some("title"))).is_allowed());
    assert!(!ask(&held, &graph(Action::Read, TASK, Some("owner"))).is_allowed());
    // Denying one property's MATCH denies reading it, not finding the node.
    let held = entries(&[
        "GRANT MATCH {*} ON GRAPH * TO r",
        "DENY MATCH {owner} ON GRAPH * TO r",
    ]);
    assert!(ask(&held, &graph(Action::Traverse, TASK, None)).is_allowed());
    assert!(!ask(&held, &graph(Action::Read, TASK, Some("owner"))).is_allowed());
    // Denying MATCH of every property hides the node itself.
    let held = entries(&[
        "GRANT MATCH {*} ON GRAPH * TO r",
        "DENY MATCH {*} ON GRAPH * TO r",
    ]);
    assert!(!ask(&held, &graph(Action::Traverse, TASK, None)).is_allowed());
}

#[test]
fn merge_implies_match_create_and_set_property() {
    let held = entries(&["GRANT MERGE {status} ON GRAPH * NODES Task TO r"]);
    assert!(ask(&held, &graph(Action::Traverse, TASK, None)).is_allowed());
    assert!(ask(&held, &graph(Action::Read, TASK, Some("status"))).is_allowed());
    assert!(ask(&held, &graph(Action::Create, TASK, None)).is_allowed());
    assert!(ask(&held, &graph(Action::SetProperty, TASK, Some("status"))).is_allowed());
    assert!(!ask(&held, &graph(Action::SetProperty, TASK, Some("owner"))).is_allowed());
    assert!(!ask(&held, &graph(Action::Delete, TASK, None)).is_allowed());
}

#[test]
fn nothing_held_is_nothing_granted() {
    assert_eq!(
        ask(&[], &graph(Action::Traverse, TASK, None)),
        Decision::NotGranted
    );
}

#[test]
fn procedure_names_match_globs() {
    let held = entries(&["GRANT EXECUTE PROCEDURE db.*, apoc.?ext.join ON DBMS TO r"]);
    for (name, allowed) in [
        ("db.labels", true),
        ("db.schema.visualization", true),
        ("apoc.text.join", true),
        ("apoc.next.join", true),
        ("apoc.texts.join", false),
        ("dbx.labels", false),
    ] {
        assert_eq!(
            ask(&held, &Request::dbms(Action::ExecuteProcedure, Some(name))).is_allowed(),
            allowed,
            "{name}"
        );
    }
}

#[test]
fn an_entry_is_written_as_the_command_that_takes_it_back() {
    let c = command("DENY READ {ssn} ON GRAPH hr NODES Employee TO r");
    let entry = Entry {
        access: Access::Deny,
        privilege: c.privileges[0].clone(),
        role: "hr".to_string(),
    };
    let revoke = entry.to_revoke_command();
    assert_eq!(
        revoke,
        "REVOKE DENY READ {ssn} ON GRAPH hr NODE Employee FROM `hr`"
    );
    let back = command(&revoke);
    assert_eq!(back.op, Op::Revoke(Some(Access::Deny)));
    assert_eq!(back.privileges, c.privileges);
}

#[test]
fn every_action_is_listed_once_with_its_level() {
    use drevo_auth::privileges::Level;
    assert_eq!(Action::ALL.len(), 27);
    for (i, a) in Action::ALL.iter().enumerate() {
        assert!(!Action::ALL[i + 1..].contains(a), "{a:?} twice");
    }
    let count = |level| Action::ALL.iter().filter(|a| a.level() == level).count();
    assert_eq!(
        (
            count(Level::Graph),
            count(Level::Database),
            count(Level::Dbms)
        ),
        (11, 8, 8)
    );
}
