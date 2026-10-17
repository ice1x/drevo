//! Privileges in the style of Neo4j: what each role may do, granted or
//! denied, on graphs, databases and the server.
//!
//! A [`Privilege`] names an [`Action`] and where it applies:
//!
//! - **graph** privileges (`TRAVERSE`, `READ {props}`, `MATCH {props}`,
//!   `CREATE`, `DELETE`, `SET LABEL`, `REMOVE LABEL`, `SET PROPERTY {props}`,
//!   `MERGE {props}`, `WRITE`, `ALL GRAPH PRIVILEGES`) on a graph, for nodes
//!   with a label or relationships of a type;
//! - **database** privileges (`ACCESS`, `START`, `STOP`, `INDEX`,
//!   `CONSTRAINT`, `NAME` and `TRANSACTION MANAGEMENT`, `ALL DATABASE
//!   PRIVILEGES`) on a database;
//! - **DBMS** privileges (`USER`, `ROLE`, `PRIVILEGE` and `DATABASE
//!   MANAGEMENT`, `EXECUTE PROCEDURE`, `EXECUTE FUNCTION`, `EXECUTE ADMIN
//!   PROCEDURES`, `ALL DBMS PRIVILEGES`) on the server.
//!
//! [`parse`] reads the `GRANT`, `DENY` and `REVOKE` commands, and
//! [`Entry::to_command`] writes a held privilege back as the command that
//! gives it, so a role's privileges can be listed and recreated. A command
//! that lists several graphs, labels or properties gives one privilege for
//! each.
//!
//! [`decide`] answers a [`Request`]: a privilege that covers it and is
//! denied wins over any that grants it, and what nothing grants is refused.
//! [`builtin_privileges`] gives the starting privileges of Neo4j's built-in
//! roles ([`BUILTIN_ROLES`]).
//!
//! ```
//! use drevo_auth::privileges::{decide, parse, Access, Action, Element, Op, Request};
//!
//! // A bug tracker: QA reads every issue except the security ones' details.
//! let mut held = Vec::new();
//! for text in [
//!     "GRANT MATCH {*} ON GRAPH bugs NODES * TO qa",
//!     "DENY READ {details} ON GRAPH bugs NODES Security TO qa",
//! ] {
//!     let c = parse(text).unwrap();
//!     let access = if c.op == Op::Deny { Access::Deny } else { Access::Grant };
//!     held.extend(c.privileges.into_iter().map(|p| (access, p)));
//! }
//! let read = |labels: &'static [&'static str], property| Request {
//!     action: Action::Read,
//!     database: "bugs",
//!     home: "bugs",
//!     element: Element::Node(labels),
//!     name: Some(property),
//! };
//! let held = held.iter().map(|(a, p)| (*a, p));
//! assert!(decide(held.clone(), &read(&["Bug"], "details")).is_allowed());
//! assert!(!decide(held.clone(), &read(&["Bug", "Security"], "details")).is_allowed());
//! assert!(decide(held, &read(&["Bug", "Security"], "title")).is_allowed());
//! ```
//!
//! This module uses `std` only and builds for `wasm32-unknown-unknown`.

mod builtin;
mod decide;
mod model;
mod parse;

pub use builtin::{builtin_privileges, BUILTIN_ROLES, PUBLIC};
pub use decide::{decide, Decision, Element, Request};
pub use model::{Access, Action, Entry, Level, Pattern, Privilege, Resource, Segment, Target};
pub use parse::{parse, Command, Op, ParseError};
