//! Logging in to the gRPC API and the Arrow Flight endpoint.
//!
//! When the server requires a login, every call carries an `authorization`
//! metadata entry, written as the HTTP header of the same name:
//! `Basic <base64 of user:password>` or `Bearer <session token>`. A missing
//! or refused login ends the call with `UNAUTHENTICATED`, too many failed
//! logins in a row with `RESOURCE_EXHAUSTED`, and a user who must change the
//! password first with `PERMISSION_DENIED` (the password is changed over
//! HTTP or Bolt with `ALTER CURRENT USER SET PASSWORD`).

#![cfg(any(feature = "grpc", feature = "arrow-flight"))]
#![allow(clippy::result_large_err)]

use tonic::metadata::MetadataMap;
use tonic::Status;

use crate::security::{Security, SecurityError, SecurityErrorKind, SignedIn, LOGIN_REFUSED};

/// The message for a user who must change the password before anything else.
pub(crate) const PASSWORD_CHANGE_REQUIRED: &str =
    "The password must be changed before anything else: run ALTER CURRENT USER SET PASSWORD \
     FROM 'current' TO 'new' over HTTP or Bolt";

/// The signed-in user of a call, or `None` when the server needs no login.
pub(crate) async fn require_login(
    security: &Security,
    metadata: &MetadataMap,
    origin: &crate::security::Origin,
) -> Result<Option<SignedIn>, Status> {
    if !security.requires_login() {
        return Ok(None);
    }
    let header = metadata
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let security = security.clone();
    let origin = origin.clone();
    // Checking a password hashes it with Argon2id: keep that off the async
    // workers.
    let user =
        tokio::task::spawn_blocking(move || security.login_http(&origin, header.as_deref(), None))
            .await
            .map_err(|e| Status::internal(format!("login task failed: {e}")))?
            .map_err(login_status)?;
    if user.password_change_required {
        return Err(Status::permission_denied(PASSWORD_CHANGE_REQUIRED));
    }
    Ok(Some(user))
}

/// The gRPC status for a refused login.
fn login_status(e: SecurityError) -> Status {
    match e.kind {
        SecurityErrorKind::RateLimited => Status::resource_exhausted(e.message),
        SecurityErrorKind::CredentialsExpired => {
            Status::permission_denied(PASSWORD_CHANGE_REQUIRED)
        }
        SecurityErrorKind::Unavailable => Status::unavailable(e.message),
        SecurityErrorKind::Internal => Status::internal(e.message),
        _ => Status::unauthenticated(LOGIN_REFUSED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_map_to_grpc_codes() {
        let err = |kind| SecurityError {
            kind,
            message: "m".into(),
        };
        assert_eq!(
            login_status(err(SecurityErrorKind::Unauthenticated)).code(),
            tonic::Code::Unauthenticated
        );
        assert_eq!(
            login_status(err(SecurityErrorKind::RateLimited)).code(),
            tonic::Code::ResourceExhausted
        );
        assert_eq!(
            login_status(err(SecurityErrorKind::CredentialsExpired)).code(),
            tonic::Code::PermissionDenied
        );
        assert_eq!(
            login_status(err(SecurityErrorKind::Unavailable)).code(),
            tonic::Code::Unavailable
        );
        // Anything else is a refused login, with the one refusal message.
        let other = login_status(err(SecurityErrorKind::InvalidArgument));
        assert_eq!(other.code(), tonic::Code::Unauthenticated);
        assert_eq!(other.message(), LOGIN_REFUSED);
    }

    #[tokio::test]
    async fn a_server_without_a_login_requirement_lets_every_call_in() {
        let none = require_login(
            &Security::default(),
            &MetadataMap::new(),
            &crate::security::Origin::new("grpc"),
        )
        .await
        .unwrap();
        assert!(none.is_none());
    }
}
