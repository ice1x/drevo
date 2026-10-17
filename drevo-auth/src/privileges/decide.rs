//! Deciding a request against the privileges a user's roles hold.

use super::model::{Access, Action, Level, Pattern, Privilege, Resource, Segment, Target};

/// The element a graph request touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Element<'a> {
    /// No element: a database or DBMS request.
    None,
    /// A node with these labels.
    Node(&'a [&'a str]),
    /// A relationship of this type.
    Relationship(&'a str),
}

/// Something a user wants to do, to be decided by [`decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request<'a> {
    /// What.
    pub action: Action,
    /// The database (or graph) it happens in; empty for the DBMS.
    pub database: &'a str,
    /// The user's home database, for `HOME` privileges.
    pub home: &'a str,
    /// The node or relationship, for graph requests.
    pub element: Element<'a>,
    /// The property (`READ`, `SET PROPERTY`), label (`SET LABEL`), procedure
    /// or function name (`EXECUTE`), or the user whose transactions are
    /// managed (`TRANSACTION MANAGEMENT`).
    pub name: Option<&'a str>,
}

impl<'a> Request<'a> {
    /// A request on a database as a whole: `ACCESS`, `INDEX MANAGEMENT`, …
    #[must_use]
    pub fn database(action: Action, database: &'a str, home: &'a str) -> Self {
        Self {
            action,
            database,
            home,
            element: Element::None,
            name: None,
        }
    }

    /// A request on the server: `USER MANAGEMENT`, `EXECUTE PROCEDURE`
    /// (with the procedure's name), …
    #[must_use]
    pub fn dbms(action: Action, name: Option<&'a str>) -> Self {
        Self {
            action,
            database: "",
            home: "",
            element: Element::None,
            name,
        }
    }

    /// The same request about `name` (a property, label, procedure or user).
    #[must_use]
    pub fn with_name(mut self, name: &'a str) -> Self {
        self.name = Some(name);
        self
    }
}

/// The outcome of [`decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// A privilege grants it and none denies it.
    Granted,
    /// A privilege denies it, whatever else grants it.
    Denied,
    /// Nothing grants it.
    NotGranted,
}

impl Decision {
    /// Whether the request may go ahead.
    #[must_use]
    pub fn is_allowed(self) -> bool {
        self == Self::Granted
    }
}

/// Decide `request` against `held`: the privileges of every role the user
/// has, each granted or denied. A privilege that covers the request and is
/// denied wins over any that grants it; with neither, nothing is granted.
///
/// A privilege covers a request when its action includes the request's
/// (`WRITE` includes `CREATE`, `MATCH` includes `TRAVERSE` and `READ`,
/// `MERGE` includes `MATCH`, `CREATE` and `SET PROPERTY`, `ALL … PRIVILEGES`
/// include their level), and its graph or database, segment and resource
/// cover the request's. Denying `MATCH` of one property denies reading it,
/// not finding the element; denying `MATCH {*}` denies both.
pub fn decide<'p>(
    held: impl IntoIterator<Item = (Access, &'p Privilege)>,
    request: &Request<'_>,
) -> Decision {
    let mut granted = false;
    for (access, privilege) in held {
        if covers(access, privilege, request) {
            match access {
                Access::Deny => return Decision::Denied,
                Access::Grant => granted = true,
            }
        }
    }
    if granted {
        Decision::Granted
    } else {
        Decision::NotGranted
    }
}

/// How a held action covers a requested one.
enum Implied {
    /// It covers the request whatever property or label it names.
    Wholly,
    /// It covers the request for the properties or labels it names.
    ByResource,
}

fn implies(held: Action, access: Access, resource: &Resource, wanted: Action) -> Option<Implied> {
    if held == wanted {
        return Some(Implied::ByResource);
    }
    // Finding an element: granted by MATCH / MERGE of any property, denied
    // only by MATCH / MERGE of every property.
    let finds = access == Access::Grant || *resource == Resource::AllProperties;
    use Action::{
        AllDatabase, AllDbms, AllGraph, Create, Delete, Match, Merge, Read, RemoveLabel, SetLabel,
        SetProperty, Traverse, Write,
    };
    match (held, wanted) {
        (AllGraph, w) if w.level() == Level::Graph => Some(Implied::Wholly),
        (Write, Create | Delete | SetLabel | RemoveLabel | SetProperty) => Some(Implied::Wholly),
        (Match, Read) => Some(Implied::ByResource),
        (Match | Merge, Traverse) if finds => Some(Implied::Wholly),
        (Merge, Match | Read | SetProperty) => Some(Implied::ByResource),
        (Merge, Create) => Some(Implied::Wholly),
        (AllDatabase, w) if w.level() == Level::Database => Some(Implied::Wholly),
        (AllDbms, w) if w.level() == Level::Dbms => Some(Implied::Wholly),
        _ => None,
    }
}

fn covers(access: Access, p: &Privilege, request: &Request<'_>) -> bool {
    let Some(implied) = implies(p.action, access, &p.resource, request.action) else {
        return false;
    };
    target_covers(p.target.as_ref(), request)
        && segment_covers(&p.segment, request)
        && (matches!(implied, Implied::Wholly) || resource_covers(&p.resource, request))
}

fn target_covers(target: Option<&Target>, request: &Request<'_>) -> bool {
    match target {
        None | Some(Target::All) => true,
        Some(Target::Home) => !request.home.is_empty() && request.database == request.home,
        Some(Target::Named(name)) => name == request.database,
    }
}

fn segment_covers(segment: &Segment, request: &Request<'_>) -> bool {
    match (segment, request.element) {
        (Segment::Graph | Segment::Database, _) => true,
        (Segment::Node(Pattern::All), Element::Node(_)) => true,
        (Segment::Node(Pattern::Name(label)), Element::Node(labels)) => {
            labels.iter().any(|l| *l == label)
        }
        (Segment::Relationship(p), Element::Relationship(t)) => p.matches(t),
        (Segment::User(p), _) => request.name.is_none_or(|n| p.matches(n)),
        (Segment::Procedure(p) | Segment::Function(p), _) => {
            request.name.is_some_and(|n| p.matches_glob(n))
        }
        _ => false,
    }
}

fn resource_covers(resource: &Resource, request: &Request<'_>) -> bool {
    match resource {
        Resource::Graph | Resource::Database | Resource::AllProperties | Resource::AllLabels => {
            true
        }
        Resource::Property(p) | Resource::Label(p) => request.name == Some(p.as_str()),
    }
}
