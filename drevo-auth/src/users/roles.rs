//! Roles: creating, renaming and dropping them, giving them to users, and
//! the privileges they hold.

use std::collections::BTreeSet;

use super::{AuthManager, ManageError};
use crate::catalog::{HeldPrivilege, Role};
use crate::privileges::{
    decide, Access, Decision, Entry, Privilege, Request, SecurityContext, BUILTIN_ROLES, PUBLIC,
};

/// The longest role name, in characters.
pub const MAX_ROLE_NAME_LEN: usize = 65;

/// A role, as [`AuthManager::roles`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleInfo {
    /// The role's name.
    pub name: String,
    /// Whether it is one of the built-in roles.
    pub builtin: bool,
    /// The users who have it, by name. For `PUBLIC`, every user.
    pub users: Vec<String>,
}

/// Whose privileges [`AuthManager::privileges`] lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivilegeFilter {
    /// Every role's.
    All,
    /// One role's.
    Role(String),
    /// Those of a user's roles, `PUBLIC` included.
    User(String),
}

/// `PUBLIC` followed by `roles`.
pub(super) fn with_public(roles: &BTreeSet<String>) -> Vec<String> {
    std::iter::once(PUBLIC.to_string())
        .chain(roles.iter().cloned())
        .collect()
}

/// Check a role name: a letter, then letters, digits and `_`, up to
/// [`MAX_ROLE_NAME_LEN`] characters.
fn validate_role_name(name: &str) -> Result<(), ManageError> {
    let reason = if name.is_empty() {
        Some("it is empty")
    } else if name.chars().count() > MAX_ROLE_NAME_LEN {
        Some("it is longer than 65 characters")
    } else if !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        Some("it must start with a letter")
    } else if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some("only letters, digits and '_' are allowed")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(ManageError::InvalidRoleName {
            name: name.to_string(),
            reason,
        }),
        None => Ok(()),
    }
}

fn refuse_public(role: &str, action: &'static str) -> Result<(), ManageError> {
    if role == PUBLIC {
        Err(ManageError::PublicRole { action })
    } else {
        Ok(())
    }
}

impl AuthManager {
    /// Every role, by name.
    #[must_use]
    pub fn roles(&self) -> Vec<RoleInfo> {
        self.read(|c| {
            c.roles
                .keys()
                .map(|name| RoleInfo {
                    name: name.clone(),
                    builtin: BUILTIN_ROLES.contains(&name.as_str()),
                    users: c
                        .users
                        .values()
                        .filter(|u| name == PUBLIC || u.roles.contains(name))
                        .map(|u| u.name.clone())
                        .collect(),
                })
                .collect()
        })
    }

    /// A user's roles: `PUBLIC`, then the roles given, by name; `None` for an
    /// unknown user.
    #[must_use]
    pub fn user_roles(&self, user: &str) -> Option<Vec<String>> {
        self.read(|c| c.users.get(user).map(|u| with_public(&u.roles)))
    }

    /// Create a role, empty or with the privileges of `copy_of`.
    ///
    /// # Errors
    ///
    /// [`ManageError::InvalidRoleName`], [`ManageError::RoleExists`] (unless
    /// `if_not_exists`), [`ManageError::RoleNotFound`] for a missing
    /// `copy_of`, or a store failure.
    pub fn create_role(
        &self,
        name: &str,
        if_not_exists: bool,
        copy_of: Option<&str>,
    ) -> Result<(), ManageError> {
        validate_role_name(name)?;
        self.change(|c| {
            if c.roles.contains_key(name) {
                return if if_not_exists {
                    Ok(false)
                } else {
                    Err(ManageError::RoleExists(name.to_string()))
                };
            }
            let role = match copy_of {
                Some(source) => c
                    .roles
                    .get(source)
                    .cloned()
                    .ok_or_else(|| ManageError::RoleNotFound(source.to_string()))?,
                None => Role::default(),
            };
            c.roles.insert(name.to_string(), role);
            Ok(true)
        })
    }

    /// Drop a role; its users lose it.
    ///
    /// # Errors
    ///
    /// [`ManageError::PublicRole`] for `PUBLIC`, [`ManageError::RoleNotFound`]
    /// unless `if_exists`, or a store failure.
    pub fn drop_role(&self, name: &str, if_exists: bool) -> Result<(), ManageError> {
        refuse_public(name, "dropped")?;
        self.change(|c| {
            if c.roles.remove(name).is_none() {
                return if if_exists {
                    Ok(false)
                } else {
                    Err(ManageError::RoleNotFound(name.to_string()))
                };
            }
            for user in c.users.values_mut() {
                user.roles.remove(name);
            }
            Ok(true)
        })
    }

    /// Rename a role, keeping its privileges and users.
    ///
    /// # Errors
    ///
    /// [`ManageError::PublicRole`] for `PUBLIC`,
    /// [`ManageError::InvalidRoleName`] for the new name,
    /// [`ManageError::RoleNotFound`] unless `if_exists`,
    /// [`ManageError::RoleExists`] when the new name is taken, or a store
    /// failure.
    pub fn rename_role(&self, from: &str, to: &str, if_exists: bool) -> Result<(), ManageError> {
        refuse_public(from, "renamed")?;
        refuse_public(to, "renamed")?;
        validate_role_name(to)?;
        self.change(|c| {
            if !c.roles.contains_key(from) {
                return if if_exists {
                    Ok(false)
                } else {
                    Err(ManageError::RoleNotFound(from.to_string()))
                };
            }
            if c.roles.contains_key(to) {
                return Err(ManageError::RoleExists(to.to_string()));
            }
            if let Some(role) = c.roles.remove(from) {
                c.roles.insert(to.to_string(), role);
            }
            for user in c.users.values_mut() {
                if user.roles.remove(from) {
                    user.roles.insert(to.to_string());
                }
            }
            Ok(true)
        })
    }

    /// Give `role` to `user`. Giving a role the user has changes nothing.
    ///
    /// # Errors
    ///
    /// [`ManageError::PublicRole`] for `PUBLIC`, [`ManageError::RoleNotFound`],
    /// [`ManageError::UserNotFound`], or a store failure.
    pub fn grant_role(&self, role: &str, user: &str) -> Result<(), ManageError> {
        refuse_public(role, "granted")?;
        self.change(|c| {
            if !c.roles.contains_key(role) {
                return Err(ManageError::RoleNotFound(role.to_string()));
            }
            let u = c
                .users
                .get_mut(user)
                .ok_or_else(|| ManageError::UserNotFound(user.to_string()))?;
            Ok(u.roles.insert(role.to_string()))
        })
    }

    /// Take `role` from `user`. Taking a role the user does not have changes
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`ManageError::PublicRole`] for `PUBLIC`, [`ManageError::UserNotFound`],
    /// or a store failure.
    pub fn revoke_role(&self, role: &str, user: &str) -> Result<(), ManageError> {
        refuse_public(role, "revoked")?;
        self.change(|c| {
            let u = c
                .users
                .get_mut(user)
                .ok_or_else(|| ManageError::UserNotFound(user.to_string()))?;
            Ok(u.roles.remove(role))
        })
    }

    /// Give `role` each of `privileges`, granted or denied. A privilege the
    /// role holds already is kept once.
    ///
    /// # Errors
    ///
    /// [`ManageError::RoleNotFound`], or a store failure.
    pub fn grant(
        &self,
        access: Access,
        privileges: &[Privilege],
        role: &str,
    ) -> Result<(), ManageError> {
        self.change(|c| {
            let r = c
                .roles
                .get_mut(role)
                .ok_or_else(|| ManageError::RoleNotFound(role.to_string()))?;
            let mut changed = false;
            for privilege in privileges {
                changed |= r.privileges.insert(HeldPrivilege {
                    access,
                    privilege: privilege.clone(),
                });
            }
            Ok(changed)
        })
    }

    /// Take each of `privileges` from `role`: the grant, the deny, or (with
    /// `None`) both. A privilege the role does not hold is skipped.
    ///
    /// # Errors
    ///
    /// [`ManageError::RoleNotFound`], or a store failure.
    pub fn revoke(
        &self,
        access: Option<Access>,
        privileges: &[Privilege],
        role: &str,
    ) -> Result<(), ManageError> {
        self.change(|c| {
            let r = c
                .roles
                .get_mut(role)
                .ok_or_else(|| ManageError::RoleNotFound(role.to_string()))?;
            let before = r.privileges.len();
            r.privileges.retain(|held| {
                !(privileges.contains(&held.privilege) && access.is_none_or(|a| a == held.access))
            });
            Ok(r.privileges.len() != before)
        })
    }

    /// The privileges held, by role name; within a role, grants first.
    #[must_use]
    pub fn privileges(&self, filter: &PrivilegeFilter) -> Vec<Entry> {
        self.read(|c| {
            let wanted: Option<BTreeSet<String>> = match filter {
                PrivilegeFilter::All => None,
                PrivilegeFilter::Role(name) => Some(std::iter::once(name.clone()).collect()),
                PrivilegeFilter::User(name) => Some(
                    c.users
                        .get(name)
                        .map(|u| with_public(&u.roles).into_iter().collect())
                        .unwrap_or_default(),
                ),
            };
            c.roles
                .iter()
                .filter(|(name, _)| wanted.as_ref().is_none_or(|w| w.contains(*name)))
                .flat_map(|(name, role)| {
                    role.privileges.iter().map(move |held| Entry {
                        access: held.access,
                        privilege: held.privilege.clone(),
                        role: name.clone(),
                    })
                })
                .collect()
        })
    }

    /// The privileges of [`privileges`](Self::privileges) as the commands
    /// that give them, in the same order. Run on a catalog with the same
    /// roles and no privileges, they recreate them.
    #[must_use]
    pub fn as_commands(&self, filter: &PrivilegeFilter) -> Vec<String> {
        self.privileges(filter)
            .iter()
            .map(Entry::to_command)
            .collect()
    }

    /// Decide `request` for `user` against the privileges of the user's
    /// roles: a matching deny wins, and what nothing grants is refused. An
    /// unknown user is granted nothing.
    #[must_use]
    pub fn decide(&self, user: &str, request: &Request<'_>) -> Decision {
        self.read(|c| {
            let Some(u) = c.users.get(user) else {
                return Decision::NotGranted;
            };
            let roles = with_public(&u.roles);
            let held = roles
                .iter()
                .filter_map(|name| c.roles.get(name))
                .flat_map(|role| role.privileges.iter())
                .map(|held| (held.access, &held.privilege));
            decide(held, request)
        })
    }

    /// The security context of `user`, from the privileges of the user's
    /// roles, with `home` as the home database. An unknown user gets a
    /// context that allows nothing.
    #[must_use]
    pub fn security_context(&self, user: &str, home: &str) -> SecurityContext {
        let held: Vec<(Access, Privilege)> = self
            .privileges(&PrivilegeFilter::User(user.to_string()))
            .into_iter()
            .map(|e| (e.access, e.privilege))
            .collect();
        SecurityContext::new(held, home)
    }

    /// Give the bootstrap user the `admin` role, when it exists.
    pub(super) fn make_admin(&self, user: &str) -> Result<(), ManageError> {
        self.change(|c| {
            if !c.roles.contains_key("admin") {
                return Ok(false);
            }
            Ok(c.users
                .get_mut(user)
                .is_some_and(|u| u.roles.insert("admin".to_string())))
        })
    }
}
