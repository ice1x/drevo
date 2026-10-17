//! The privilege types: what a role may do ([`Action`]), where ([`Target`],
//! [`Segment`]) and to what ([`Resource`]), granted or denied ([`Access`]).

use std::fmt;

#[cfg(feature = "password")]
use serde::{Deserialize, Serialize};

/// Whether a privilege allows or forbids. A matching deny always wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "password", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "password", serde(rename_all = "lowercase"))]
pub enum Access {
    /// `GRANT`: allows what the privilege names.
    Grant,
    /// `DENY`: forbids what the privilege names, whatever else is granted.
    Deny,
}

impl Access {
    /// The `access` column of `SHOW PRIVILEGES`: `GRANTED` or `DENIED`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Grant => "GRANTED",
            Self::Deny => "DENIED",
        }
    }

    /// The command keyword: `GRANT` or `DENY`.
    #[must_use]
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Grant => "GRANT",
            Self::Deny => "DENY",
        }
    }
}

/// What a privilege lets a role do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "password", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "password", serde(rename_all = "snake_case"))]
pub enum Action {
    // Graph.
    /// Find nodes and relationships.
    Traverse,
    /// Read properties.
    Read,
    /// Find and read: [`Traverse`](Self::Traverse) and [`Read`](Self::Read).
    Match,
    /// Create nodes and relationships.
    Create,
    /// Delete nodes and relationships.
    Delete,
    /// Add labels to nodes.
    SetLabel,
    /// Remove labels from nodes.
    RemoveLabel,
    /// Set properties.
    SetProperty,
    /// `MERGE`: match, create, and set properties.
    Merge,
    /// Every write: create, delete, set and remove labels, set properties.
    Write,
    /// Every graph privilege.
    AllGraph,
    // Database.
    /// Connect to a database.
    Access,
    /// Start a database.
    Start,
    /// Stop a database.
    Stop,
    /// Create, drop and show indexes.
    IndexManagement,
    /// Create, drop and show constraints.
    ConstraintManagement,
    /// Create new labels, relationship types and property names.
    NameManagement,
    /// Show and terminate other users' transactions.
    TransactionManagement,
    /// Every database privilege.
    AllDatabase,
    // DBMS.
    /// Create, alter, drop and show users.
    UserManagement,
    /// Create, drop, assign and show roles.
    RoleManagement,
    /// Grant, deny, revoke and show privileges.
    PrivilegeManagement,
    /// Create, drop and alter databases.
    DatabaseManagement,
    /// Run procedures whose names match.
    ExecuteProcedure,
    /// Run functions whose names match.
    ExecuteFunction,
    /// Run administration procedures.
    ExecuteAdminProcedures,
    /// Every DBMS privilege.
    AllDbms,
}

/// Which level a privilege applies at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Nodes and relationships of a graph.
    Graph,
    /// A database as a whole.
    Database,
    /// The server.
    Dbms,
}

impl Action {
    /// Every action, graph actions first, then database, then DBMS.
    pub const ALL: [Action; 27] = [
        Action::Traverse,
        Action::Read,
        Action::Match,
        Action::Create,
        Action::Delete,
        Action::SetLabel,
        Action::RemoveLabel,
        Action::SetProperty,
        Action::Merge,
        Action::Write,
        Action::AllGraph,
        Action::Access,
        Action::Start,
        Action::Stop,
        Action::IndexManagement,
        Action::ConstraintManagement,
        Action::NameManagement,
        Action::TransactionManagement,
        Action::AllDatabase,
        Action::UserManagement,
        Action::RoleManagement,
        Action::PrivilegeManagement,
        Action::DatabaseManagement,
        Action::ExecuteProcedure,
        Action::ExecuteFunction,
        Action::ExecuteAdminProcedures,
        Action::AllDbms,
    ];

    /// The level the action applies at.
    #[must_use]
    pub fn level(self) -> Level {
        use Action::*;
        match self {
            Traverse | Read | Match | Create | Delete | SetLabel | RemoveLabel | SetProperty
            | Merge | Write | AllGraph => Level::Graph,
            Access
            | Start
            | Stop
            | IndexManagement
            | ConstraintManagement
            | NameManagement
            | TransactionManagement
            | AllDatabase => Level::Database,
            UserManagement
            | RoleManagement
            | PrivilegeManagement
            | DatabaseManagement
            | ExecuteProcedure
            | ExecuteFunction
            | ExecuteAdminProcedures
            | AllDbms => Level::Dbms,
        }
    }

    /// The `action` column of `SHOW PRIVILEGES`, as Neo4j names it.
    #[must_use]
    pub fn name(self) -> &'static str {
        use Action::*;
        match self {
            Traverse => "traverse",
            Read => "read",
            Match => "match",
            Create => "create_element",
            Delete => "delete_element",
            SetLabel => "set_label",
            RemoveLabel => "remove_label",
            SetProperty => "set_property",
            Merge => "merge",
            Write => "write",
            AllGraph => "graph_actions",
            Access => "access",
            Start => "start_database",
            Stop => "stop_database",
            IndexManagement => "index",
            ConstraintManagement => "constraint",
            NameManagement => "token",
            TransactionManagement => "transaction_management",
            AllDatabase => "database_actions",
            UserManagement => "user_management",
            RoleManagement => "role_management",
            PrivilegeManagement => "privilege_management",
            DatabaseManagement => "database_management",
            ExecuteProcedure => "execute",
            ExecuteFunction => "execute_function",
            ExecuteAdminProcedures => "execute_admin",
            AllDbms => "dbms_actions",
        }
    }

    /// The command words, without their arguments: `SET PROPERTY`,
    /// `EXECUTE PROCEDURE`, `ALL GRAPH PRIVILEGES`.
    #[must_use]
    pub fn keyword(self) -> &'static str {
        use Action::*;
        match self {
            Traverse => "TRAVERSE",
            Read => "READ",
            Match => "MATCH",
            Create => "CREATE",
            Delete => "DELETE",
            SetLabel => "SET LABEL",
            RemoveLabel => "REMOVE LABEL",
            SetProperty => "SET PROPERTY",
            Merge => "MERGE",
            Write => "WRITE",
            AllGraph => "ALL GRAPH PRIVILEGES",
            Access => "ACCESS",
            Start => "START",
            Stop => "STOP",
            IndexManagement => "INDEX MANAGEMENT",
            ConstraintManagement => "CONSTRAINT MANAGEMENT",
            NameManagement => "NAME MANAGEMENT",
            TransactionManagement => "TRANSACTION MANAGEMENT",
            AllDatabase => "ALL DATABASE PRIVILEGES",
            UserManagement => "USER MANAGEMENT",
            RoleManagement => "ROLE MANAGEMENT",
            PrivilegeManagement => "PRIVILEGE MANAGEMENT",
            DatabaseManagement => "DATABASE MANAGEMENT",
            ExecuteProcedure => "EXECUTE PROCEDURE",
            ExecuteFunction => "EXECUTE FUNCTION",
            ExecuteAdminProcedures => "EXECUTE ADMIN PROCEDURES",
            AllDbms => "ALL DBMS PRIVILEGES",
        }
    }

    /// Whether the action takes a property list: `READ {name}`.
    pub(crate) fn takes_properties(self) -> bool {
        matches!(
            self,
            Self::Read | Self::Match | Self::SetProperty | Self::Merge
        )
    }

    /// Whether the action takes a label list: `SET LABEL Done`.
    pub(crate) fn takes_labels(self) -> bool {
        matches!(self, Self::SetLabel | Self::RemoveLabel)
    }

    /// Whether the action applies to nodes or relationships named after
    /// `NODES` / `RELATIONSHIPS` / `ELEMENTS`.
    pub(crate) fn takes_segment(self) -> bool {
        matches!(
            self,
            Self::Traverse
                | Self::Read
                | Self::Match
                | Self::Create
                | Self::Delete
                | Self::SetProperty
                | Self::Merge
        )
    }
}

/// Every graph, or database, or one named, or the user's home one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "password", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "password", serde(rename_all = "lowercase"))]
pub enum Target {
    /// `*`: every graph or database.
    All,
    /// `HOME GRAPH` / `HOME DATABASE`: the user's home database.
    Home,
    /// One graph or database by name.
    Named(String),
}

/// `*` or one name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "password", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "password", serde(rename_all = "lowercase"))]
pub enum Pattern {
    /// `*`: every name.
    All,
    /// One name; for procedures and functions a glob with `*` and `?`.
    Name(String),
}

impl Pattern {
    /// Whether `name` is covered.
    pub(crate) fn matches(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Name(n) => n == name,
        }
    }

    /// Whether `name` is covered, reading `*` (any run) and `?` (any one
    /// character) in this pattern as wildcards.
    pub(crate) fn matches_glob(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Name(glob) => glob_matches(glob.as_bytes(), name.as_bytes()),
        }
    }
}

fn glob_matches(glob: &[u8], name: &[u8]) -> bool {
    // Iterative wildcard match with backtracking to the last `*`.
    let (mut g, mut n) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while n < name.len() {
        if g < glob.len() && (glob[g] == b'?' || glob[g] == name[n]) {
            g += 1;
            n += 1;
        } else if g < glob.len() && glob[g] == b'*' {
            star = Some(g);
            mark = n;
            g += 1;
        } else if let Some(s) = star {
            g = s + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    glob[g..].iter().all(|&c| c == b'*')
}

/// Which elements, users, procedures or functions a privilege covers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "password", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "password", serde(rename_all = "lowercase"))]
pub enum Segment {
    /// Every node and relationship of the graph (`WRITE`, `SET LABEL`, …).
    Graph,
    /// Nodes with a label, or every node.
    Node(Pattern),
    /// Relationships of a type, or every relationship.
    Relationship(Pattern),
    /// The database or server as a whole.
    Database,
    /// Transactions of a user, or of every user.
    User(Pattern),
    /// Procedures whose names match.
    Procedure(Pattern),
    /// Functions whose names match.
    Function(Pattern),
}

/// The properties or labels a privilege covers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "password", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "password", serde(rename_all = "snake_case"))]
pub enum Resource {
    /// The graph or database as a whole.
    Graph,
    /// The database as a whole.
    Database,
    /// `{*}`: every property.
    AllProperties,
    /// One property.
    Property(String),
    /// `*`: every label.
    AllLabels,
    /// One label.
    Label(String),
}

/// One privilege: an action on one target, segment and resource. A command
/// that names several graphs, labels or properties holds one `Privilege`
/// for each, so revoking one of them leaves the others.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "password", derive(Serialize, Deserialize))]
pub struct Privilege {
    /// What it lets a role do.
    pub action: Action,
    /// Which graph or database; `None` for the DBMS.
    pub target: Option<Target>,
    /// Which elements, users, procedures or functions.
    pub segment: Segment,
    /// Which properties or labels.
    pub resource: Resource,
}

impl Privilege {
    /// The `resource` column of `SHOW PRIVILEGES`: `graph`, `database`,
    /// `all_properties`, `property(name)`, `all_labels` or `label(name)`.
    #[must_use]
    pub fn resource_name(&self) -> String {
        match &self.resource {
            Resource::Graph => "graph".to_string(),
            Resource::Database => "database".to_string(),
            Resource::AllProperties => "all_properties".to_string(),
            Resource::Property(p) => format!("property({p})"),
            Resource::AllLabels => "all_labels".to_string(),
            Resource::Label(l) => format!("label({l})"),
        }
    }

    /// The `graph` column of `SHOW PRIVILEGES`: `*`, `HOME` or a name.
    #[must_use]
    pub fn graph_name(&self) -> String {
        match &self.target {
            None | Some(Target::All) => "*".to_string(),
            Some(Target::Home) => "HOME".to_string(),
            Some(Target::Named(n)) => n.clone(),
        }
    }

    /// The `segment` column of `SHOW PRIVILEGES`: `NODE(Label)`,
    /// `RELATIONSHIP(*)`, `database`, `USER(name)`, `PROCEDURE(glob)`, …
    #[must_use]
    pub fn segment_name(&self) -> String {
        let p = |pattern: &Pattern| match pattern {
            Pattern::All => "*".to_string(),
            Pattern::Name(n) => n.clone(),
        };
        match &self.segment {
            Segment::Graph => "ELEMENT(*)".to_string(),
            Segment::Node(x) => format!("NODE({})", p(x)),
            Segment::Relationship(x) => format!("RELATIONSHIP({})", p(x)),
            Segment::Database => "database".to_string(),
            Segment::User(x) => format!("USER({})", p(x)),
            Segment::Procedure(x) => format!("PROCEDURE({})", p(x)),
            Segment::Function(x) => format!("FUNCTION({})", p(x)),
        }
    }
}

/// A name as a command writes it: bare when it is a plain identifier,
/// otherwise in backticks with inner backticks doubled.
pub(crate) fn quote(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !super::parse::is_reserved(name);
    if plain {
        name.to_string()
    } else {
        format!("`{}`", name.replace('`', "``"))
    }
}

fn pattern(p: &Pattern) -> String {
    match p {
        Pattern::All => "*".to_string(),
        Pattern::Name(n) => quote(n),
    }
}

/// A procedure or function glob is written as is: it may hold `.`, `*`
/// and `?`, which the parser reads without quotes.
fn glob(p: &Pattern) -> String {
    match p {
        Pattern::All => "*".to_string(),
        Pattern::Name(n) => n.clone(),
    }
}

impl fmt::Display for Privilege {
    /// The privilege as the part of a command between `GRANT` and `TO`:
    /// `READ {name} ON GRAPH erp NODE Customer`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let action = self.action;
        match action.level() {
            Level::Dbms => {
                f.write_str(action.keyword())?;
                match &self.segment {
                    Segment::Procedure(p) | Segment::Function(p) => write!(f, " {}", glob(p))?,
                    _ => {}
                }
                f.write_str(" ON DBMS")
            }
            Level::Database => {
                f.write_str(action.keyword())?;
                if let Segment::User(u) = &self.segment {
                    write!(f, " ({})", pattern(u))?;
                }
                match &self.target {
                    Some(Target::Home) => f.write_str(" ON HOME DATABASE"),
                    Some(Target::Named(n)) => write!(f, " ON DATABASE {}", quote(n)),
                    _ => f.write_str(" ON DATABASE *"),
                }
            }
            Level::Graph => {
                f.write_str(action.keyword())?;
                match &self.resource {
                    Resource::AllProperties => f.write_str(" {*}")?,
                    Resource::Property(p) => write!(f, " {{{}}}", quote(p))?,
                    Resource::AllLabels => f.write_str(" *")?,
                    Resource::Label(l) => write!(f, " {}", quote(l))?,
                    Resource::Graph | Resource::Database => {}
                }
                match &self.target {
                    Some(Target::Home) => f.write_str(" ON HOME GRAPH")?,
                    Some(Target::Named(n)) => write!(f, " ON GRAPH {}", quote(n))?,
                    _ => f.write_str(" ON GRAPH *")?,
                }
                match &self.segment {
                    Segment::Node(p) => write!(f, " NODE {}", pattern(p)),
                    Segment::Relationship(p) => write!(f, " RELATIONSHIP {}", pattern(p)),
                    _ => Ok(()),
                }
            }
        }
    }
}

/// A privilege held by a role, granted or denied.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Entry {
    /// Granted or denied.
    pub access: Access,
    /// What.
    pub privilege: Privilege,
    /// The role that holds it.
    pub role: String,
}

impl Entry {
    /// The command that gives the role this privilege:
    /// `GRANT TRAVERSE ON GRAPH * NODE * TO `reader``. Role names are always
    /// in backticks, as in Neo4j's `SHOW PRIVILEGES AS COMMANDS`.
    #[must_use]
    pub fn to_command(&self) -> String {
        format!(
            "{} {} TO `{}`",
            self.access.keyword(),
            self.privilege,
            self.role.replace('`', "``")
        )
    }

    /// The command that takes this privilege from the role:
    /// `REVOKE GRANT TRAVERSE ON GRAPH * NODE * FROM `reader``.
    #[must_use]
    pub fn to_revoke_command(&self) -> String {
        format!(
            "REVOKE {} {} FROM `{}`",
            self.access.keyword(),
            self.privilege,
            self.role.replace('`', "``")
        )
    }
}
