# drevo-auth

Authentication and role-based access control for Rust servers:

- **Authentication** (`authn`): users with Argon2id password hashes, and
  session tokens. A client presents a username and password or a token; the
  store answers with the username it proves, or why not.
- **User management** (`users`, `catalog`): create, change and drop users in
  a stored catalog — JSON in one file, written atomically and readable by its
  owner only — with a password policy and a first-user bootstrap in the style
  of Neo4j's `NEO4J_AUTH`.
- **Privileges** (`privileges`): Neo4j's privilege model — `TRAVERSE`, `READ`,
  `MATCH`, `WRITE`, … on graphs, `ACCESS`, `INDEX MANAGEMENT`, … on
  databases, `USER MANAGEMENT`, `EXECUTE PROCEDURE`, … on the server — read
  from and written back as `GRANT` / `DENY` / `REVOKE` commands. Roles in the
  catalog hold them, users get roles, and a request is decided with a
  matching deny winning over any grant. The built-in roles `PUBLIC`,
  `reader`, `editor`, `publisher`, `architect` and `admin` start with
  Neo4j's default privileges.
  A `SecurityContext` compiles one user's privileges for a session and
  answers each check from a cache.
- **OIDC** (`oidc`, optional): bearer tokens from an identity provider
  (Keycloak, Kanidm, Auth0, …). A JWT's RS256 or ES256 signature is checked
  against the provider's key set, cached by key id; then `iss`, `aud`, `exp`
  and `nbf` with a clock-skew allowance. The user's groups map to roles. The
  crate makes no network calls: the caller supplies the key set.
- **Audit** (`audit`): security events of a fixed shape — logins, logouts,
  password, user, role and privilege changes, refusals — with no field for
  a password or token, written as JSON lines to a size-rotated file, to
  stdout or to memory.

It knows nothing about the network, so it fits any server: authenticate the
connection, then ask the user's security context before each action.
It is the security layer of [drevo](https://github.com/ice1x/drevo), a graph
database.

## Example

```rust
use drevo_auth::authn::{Credentials, UserStore};
use drevo_auth::privileges::{parse, Access, Action, Element, Op, SecurityContext};

let mut users = UserStore::new();
users.add_user("ann", "s3cret").unwrap();
let login = Credentials::Basic { username: "ann".into(), password: "s3cret".into() };
let user = users.authenticate(&login).unwrap();
assert_eq!(user, "ann");

// A planner may create tasks but delete nothing.
let held: Vec<_> = [
    "GRANT MATCH {*} ON GRAPH tasks TO planner",
    "GRANT CREATE ON GRAPH tasks NODES Task TO planner",
    "DENY DELETE ON GRAPH tasks TO planner",
]
.iter()
.flat_map(|text| {
    let c = parse(text).unwrap();
    let access = if c.op == Op::Deny { Access::Deny } else { Access::Grant };
    c.privileges.into_iter().map(move |p| (access, p))
})
.collect();
let ann = SecurityContext::new(held, "tasks");
let task = Element::Node(&["Task"]);
assert!(ann.can_write(Action::Create, "tasks", task, None));
assert!(!ann.can_write(Action::Delete, "tasks", task, None));

// A session token lets the client reconnect without the password.
let token = users.issue_token(&user).unwrap();
assert_eq!(users.authenticate(&Credentials::Token(token)).unwrap(), "ann");
```

## Managing users

```rust
use std::sync::Arc;
use drevo_auth::catalog::MemoryStore; // or FileStore::new("security.json")
use drevo_auth::users::{AlterUser, AuthManager, Bootstrap, NewUser, UserStatus};

let users = AuthManager::open(Arc::new(MemoryStore::new())).unwrap();
// First start: create the administrator (or `Bootstrap::Generated` for `neo4j`
// with a random password that must be changed).
users.bootstrap(&Bootstrap::parse(Some("admin/long-password")).unwrap()).unwrap();

users.create_user(NewUser::new("ann", "first-password")).unwrap();
users.change_own_password("ann", "first-password", "second-password").unwrap();
users.alter_user("ann", AlterUser::default().status(UserStatus::Suspended)).unwrap();
assert_eq!(users.users().len(), 2);
```

## Roles and privileges

```rust
use std::sync::Arc;
use drevo_auth::catalog::MemoryStore;
use drevo_auth::privileges::{parse, Access, Action, Element, Request};
use drevo_auth::users::{AuthManager, NewUser, PrivilegeFilter};

let users = AuthManager::open(Arc::new(MemoryStore::new())).unwrap();
users.create_user(NewUser::new("ann", "first-password")).unwrap();
users.create_role("triage", false, Some("reader")).unwrap();
let deny = parse("DENY READ {reporter} ON GRAPH bugs NODES Bug TO triage").unwrap();
users.grant(Access::Deny, &deny.privileges, "triage").unwrap();
users.grant_role("triage", "ann").unwrap();

let read = |property| Request {
    action: Action::Read,
    database: "bugs",
    home: "bugs",
    element: Element::Node(&["Bug"]),
    name: Some(property),
};
assert!(users.decide("ann", &read("title")).is_allowed());
assert!(!users.decide("ann", &read("reporter")).is_allowed());

// The role's privileges, as the commands that recreate them.
for command in users.as_commands(&PrivilegeFilter::Role("triage".into())) {
    println!("{command}");
}
```

## Features

- `oidc`: the `oidc` module; adds `ring` for signature checks.
- `password` (default): the `authn`, `catalog` and `users` modules and their
  Argon2id and JSON dependencies. Without it the crate is the access policy
  alone, which needs nothing beyond `std` and builds for
  `wasm32-unknown-unknown`.

## License

MIT OR Apache-2.0.
