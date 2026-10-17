//! The built-in roles and the privileges each starts with, as in Neo4j.

use super::model::{Action, Pattern, Privilege, Resource, Segment, Target};

/// The built-in roles, from least to most privileged. `PUBLIC` is every
/// user's role and cannot be granted, revoked, dropped or renamed.
pub const BUILTIN_ROLES: [&str; 6] = [
    PUBLIC,
    "reader",
    "editor",
    "publisher",
    "architect",
    "admin",
];

/// The role every user has.
pub const PUBLIC: &str = "PUBLIC";

fn graph(action: Action, segment: Segment, resource: Resource) -> Privilege {
    Privilege {
        action,
        target: Some(Target::All),
        segment,
        resource,
    }
}

fn database(action: Action, target: Target, segment: Segment) -> Privilege {
    Privilege {
        action,
        target: Some(target),
        segment,
        resource: Resource::Database,
    }
}

fn dbms(action: Action, segment: Segment) -> Privilege {
    Privilege {
        action,
        target: None,
        segment,
        resource: Resource::Database,
    }
}

/// The privileges a built-in role starts with (all granted), or `None` for
/// a name that is not a built-in role. Each role holds everything the one
/// before it holds:
///
/// - `PUBLIC`: access to the home database, and every procedure and function;
/// - `reader`: access to every database, and `MATCH {*}` on every node and
///   relationship;
/// - `editor`: and `WRITE` on every graph;
/// - `publisher`: and `NAME MANAGEMENT`;
/// - `architect`: and `INDEX MANAGEMENT` and `CONSTRAINT MANAGEMENT`;
/// - `admin`: and `START`, `STOP`, `TRANSACTION MANAGEMENT` and every DBMS
///   privilege.
#[must_use]
pub fn builtin_privileges(role: &str) -> Option<Vec<Privilege>> {
    let any = || Target::All;
    let public = vec![
        database(Action::Access, Target::Home, Segment::Database),
        dbms(Action::ExecuteProcedure, Segment::Procedure(Pattern::All)),
        dbms(Action::ExecuteFunction, Segment::Function(Pattern::All)),
    ];
    let reader = vec![
        database(Action::Access, any(), Segment::Database),
        graph(
            Action::Match,
            Segment::Node(Pattern::All),
            Resource::AllProperties,
        ),
        graph(
            Action::Match,
            Segment::Relationship(Pattern::All),
            Resource::AllProperties,
        ),
    ];
    let mut editor = reader.clone();
    editor.push(graph(Action::Write, Segment::Graph, Resource::Graph));
    let mut publisher = editor.clone();
    publisher.push(database(Action::NameManagement, any(), Segment::Database));
    let mut architect = publisher.clone();
    architect.push(database(Action::IndexManagement, any(), Segment::Database));
    architect.push(database(
        Action::ConstraintManagement,
        any(),
        Segment::Database,
    ));
    let mut admin = architect.clone();
    admin.push(database(Action::Start, any(), Segment::Database));
    admin.push(database(Action::Stop, any(), Segment::Database));
    admin.push(database(
        Action::TransactionManagement,
        any(),
        Segment::User(Pattern::All),
    ));
    admin.push(dbms(Action::AllDbms, Segment::Database));
    match role {
        PUBLIC => Some(public),
        "reader" => Some(reader),
        "editor" => Some(editor),
        "publisher" => Some(publisher),
        "architect" => Some(architect),
        "admin" => Some(admin),
        _ => None,
    }
}
