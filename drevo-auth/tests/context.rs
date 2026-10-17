//! A session's `SecurityContext`: what the signed-in user may see and do,
//! compiled once from the privileges of the user's roles.

use drevo_auth::privileges::{
    builtin_privileges, parse, Access, Action, Element, Op, Privilege, SecurityContext,
};

/// The context of a user holding `commands` (and nothing else).
fn holding(commands: &[&str]) -> SecurityContext {
    let mut held: Vec<(Access, Privilege)> = Vec::new();
    for text in commands {
        let c = parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        let access = if c.op == Op::Deny {
            Access::Deny
        } else {
            Access::Grant
        };
        held.extend(c.privileges.into_iter().map(|p| (access, p)));
    }
    SecurityContext::new(held, "tasks")
}

fn role(name: &str) -> SecurityContext {
    let mut held: Vec<(Access, Privilege)> = builtin_privileges("PUBLIC")
        .unwrap()
        .into_iter()
        .map(|p| (Access::Grant, p))
        .collect();
    held.extend(
        builtin_privileges(name)
            .unwrap()
            .into_iter()
            .map(|p| (Access::Grant, p)),
    );
    SecurityContext::new(held, "tasks")
}

const BUG: Element<'static> = Element::Node(&["Bug"]);
const SECRET_BUG: Element<'static> = Element::Node(&["Bug", "Secret"]);

#[test]
fn an_unrestricted_context_allows_everything() {
    let c = SecurityContext::unrestricted();
    assert!(c.is_unrestricted());
    assert!(c.reads_everything("any"));
    assert!(c.writes_everything("any"));
    assert!(c.can_access("any"));
    assert!(c.can_traverse_node("any", &["Secret"]));
    assert!(c.can_read_property("any", SECRET_BUG, "salary"));
    assert!(c.can_write(Action::Delete, "any", BUG, None));
    assert!(c.can_database(Action::IndexManagement, "any"));
    assert!(c.can_execute_procedure("db.labels"));
    assert!(c.has_dbms(Action::UserManagement));
}

#[test]
fn a_reader_reads_every_database_and_writes_nothing() {
    let c = role("reader");
    assert!(!c.is_unrestricted());
    assert!(c.reads_everything("bugs"));
    assert!(!c.writes_everything("bugs"));
    assert!(c.can_access("bugs"));
    assert!(c.can_traverse_node("bugs", &["Bug"]));
    assert!(c.can_traverse_relationship("bugs", "BLOCKS"));
    assert!(c.can_read_property("bugs", BUG, "title"));
    assert!(!c.can_write(Action::Create, "bugs", BUG, None));
    assert!(!c.can_write(Action::SetProperty, "bugs", BUG, Some("status")));
    assert!(!c.can_database(Action::IndexManagement, "bugs"));
    assert!(!c.has_dbms(Action::UserManagement));
}

#[test]
fn an_editor_writes_and_an_architect_manages_indexes() {
    let editor = role("editor");
    assert!(editor.writes_everything("bugs"));
    assert!(editor.can_write(
        Action::Delete,
        "bugs",
        Element::Relationship("BLOCKS"),
        None
    ));
    assert!(editor.can_write(Action::SetLabel, "bugs", BUG, Some("Triaged")));
    assert!(!editor.can_database(Action::IndexManagement, "bugs"));
    assert!(role("architect").can_database(Action::IndexManagement, "bugs"));
    assert!(role("architect").can_database(Action::ConstraintManagement, "bugs"));
}

#[test]
fn an_admin_has_every_dbms_privilege() {
    let c = role("admin");
    for action in [
        Action::UserManagement,
        Action::RoleManagement,
        Action::PrivilegeManagement,
        Action::DatabaseManagement,
        Action::ExecuteAdminProcedures,
    ] {
        assert!(c.has_dbms(action), "{action:?}");
    }
    assert!(c.can_database(Action::Start, "bugs"));
}

#[test]
fn public_alone_reaches_the_home_database_but_sees_nothing() {
    let c = role("PUBLIC");
    assert!(c.can_access("tasks"), "home database");
    assert!(!c.can_access("bugs"));
    assert!(!c.reads_everything("tasks"));
    assert!(!c.can_traverse_node("tasks", &["Task"]));
    assert!(c.can_execute_procedure("db.labels"));
    assert!(c.can_execute_function("apoc.text.join"));
}

#[test]
fn a_denied_label_hides_its_nodes_and_ends_the_fast_path() {
    let c = holding(&[
        "GRANT ACCESS ON DATABASE * TO r",
        "GRANT MATCH {*} ON GRAPH * TO r",
        "DENY TRAVERSE ON GRAPH bugs NODES Secret TO r",
    ]);
    assert!(!c.reads_everything("bugs"), "a deny means filtering");
    assert!(c.can_traverse_node("bugs", &["Bug"]));
    assert!(!c.can_traverse_node("bugs", &["Bug", "Secret"]));
    assert!(
        c.can_traverse_node("other", &["Secret"]),
        "the deny names one graph"
    );
}

#[test]
fn a_denied_property_reads_as_hidden() {
    let c = holding(&[
        "GRANT MATCH {*} ON GRAPH * TO r",
        "DENY READ {salary} ON GRAPH * NODES Employee TO r",
    ]);
    let employee = Element::Node(&["Employee"]);
    assert!(c.can_traverse_node("hr", &["Employee"]));
    assert!(c.can_read_property("hr", employee, "name"));
    assert!(!c.can_read_property("hr", employee, "salary"));
    assert!(c.can_read_property("hr", Element::Node(&["Contractor"]), "salary"));
}

#[test]
fn procedures_follow_their_globs() {
    let c = holding(&[
        "GRANT EXECUTE PROCEDURE db.* ON DBMS TO r",
        "DENY EXECUTE PROCEDURE db.create* ON DBMS TO r",
    ]);
    assert!(c.can_execute_procedure("db.labels"));
    assert!(!c.can_execute_procedure("db.createLabel"));
    assert!(!c.can_execute_procedure("apoc.help"));
    assert!(!c.can_execute_function("db.labels"));
}

#[test]
fn nothing_held_allows_nothing() {
    let c = SecurityContext::new(Vec::new(), "tasks");
    assert!(!c.is_unrestricted());
    assert!(!c.can_access("tasks"));
    assert!(!c.reads_everything("tasks"));
    assert!(!c.can_traverse_node("tasks", &[]));
    assert!(!c.can_execute_procedure("db.labels"));
}

#[test]
fn repeated_checks_give_the_same_answers() {
    let c = holding(&[
        "GRANT MATCH {*} ON GRAPH * TO r",
        "DENY READ {salary} ON GRAPH * NODES Employee TO r",
    ]);
    let employee = Element::Node(&["Employee"]);
    for _ in 0..3 {
        assert!(!c.can_read_property("hr", employee, "salary"));
        assert!(c.can_read_property("hr", employee, "name"));
    }
    // Label order does not change the answer.
    let c = holding(&[
        "GRANT TRAVERSE ON GRAPH * TO r",
        "DENY TRAVERSE ON GRAPH * NODES Secret TO r",
    ]);
    assert!(!c.can_traverse_node("g", &["Secret", "Bug"]));
    assert!(!c.can_traverse_node("g", &["Bug", "Secret"]));
    // A clone shares the compiled table and answers alike.
    let copy = c.clone();
    assert!(!copy.can_traverse_node("g", &["Bug", "Secret"]));
    assert!(copy.can_traverse_node("g", &["Bug"]));
}

#[test]
fn the_context_is_cheap_to_share_across_threads() {
    fn send_sync<T: Send + Sync + Clone>() {}
    send_sync::<SecurityContext>();
}
