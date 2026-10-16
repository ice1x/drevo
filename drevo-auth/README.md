# drevo-auth

Authentication and role-based access control for Rust servers:

- **Authentication** (`authn`): users with Argon2id password hashes, and
  session tokens. A client presents a username and password or a token; the
  store answers with the username it proves, or why not.
- **User management** (`users`, `catalog`): create, change and drop users in
  a stored catalog — JSON in one file, written atomically and readable by its
  owner only — with a password policy and a first-user bootstrap in the style
  of Neo4j's `NEO4J_AUTH`.
- **Authorization** (`authz`): roles that allow or deny actions on resources,
  roles that inherit other roles, and an access policy that assigns roles to
  users and decides each request. A matching deny always wins over an allow,
  and anything not allowed is denied.

It knows nothing about the data it protects or the network, so it fits any
server: authenticate the connection, then ask the policy before each action.
It is the security layer of [drevo](https://github.com/ice1x/drevo), a graph
database.

## Example

```rust
use drevo_auth::authn::{Credentials, UserStore};
use drevo_auth::authz::{AccessPolicy, Action, Resource, Role, Scope};

let mut users = UserStore::new();
users.add_user("ann", "s3cret").unwrap();

let mut policy = AccessPolicy::new();
policy
    .define_role(
        Role::reader("planner")
            .allow(Action::CreateNode, Scope::kind("Task"))
            .deny(Action::DeleteNode, Scope::All),
    )
    .unwrap();
policy.assign_role("ann", "planner").unwrap();

let login = Credentials::Basic { username: "ann".into(), password: "s3cret".into() };
let user = users.authenticate(&login).unwrap();
assert!(policy.is_allowed(&user, Action::CreateNode, &Resource::kind("Task")));
assert!(!policy.is_allowed(&user, Action::DeleteNode, &Resource::kind("Task")));

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

## Features

- `password` (default): the `authn`, `catalog` and `users` modules and their
  Argon2id and JSON dependencies. Without it the crate is the access policy
  alone, which needs nothing beyond `std` and builds for
  `wasm32-unknown-unknown`.

## License

MIT OR Apache-2.0.
