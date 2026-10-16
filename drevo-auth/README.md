# drevo-auth

Authentication and role-based access control for Rust servers:

- **Authentication** (`authn`): users with Argon2id password hashes, and
  session tokens. A client presents a username and password or a token; the
  store answers with the username it proves, or why not.
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

## Features

- `password` (default): the `authn` module and its Argon2id dependency. Without
  it the crate is the access policy alone, which needs nothing beyond `std` and
  builds for `wasm32-unknown-unknown`.

## License

MIT OR Apache-2.0.
