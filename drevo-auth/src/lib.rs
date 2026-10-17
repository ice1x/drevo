// The crate page is the README (with its runnable example), which needs the
// default `password` feature; without it the page says what the build holds.
#![cfg_attr(feature = "password", doc = include_str!("../README.md"))]
#![cfg_attr(
    not(feature = "password"),
    doc = "Privileges in the style of Neo4j: the [`privileges`] module. The default \
           `password` feature adds authentication (`authn`): users with \
           Argon2id password hashes, and session tokens."
)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(feature = "password")]
pub mod audit;
#[cfg(feature = "password")]
pub mod authn;
#[cfg(feature = "password")]
pub mod catalog;
#[cfg(feature = "oidc")]
pub mod oidc;
pub mod privileges;
#[cfg(feature = "password")]
pub mod users;
