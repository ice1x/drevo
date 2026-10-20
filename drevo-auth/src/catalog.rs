//! The security catalog: the stored users and roles, and where they are kept.
//!
//! A [`Catalog`] holds every user with their Argon2id password hash, account
//! state and roles, and every role with its privileges. A [`CatalogStore`]
//! loads and saves it:
//!
//! - [`FileStore`] keeps it as JSON in one file. A save writes a temporary
//!   file, flushes it to disk and renames it over the old one, so the file is
//!   always either the previous or the new catalog, never half of one. On Unix
//!   the file is readable by its owner only (mode `0600`).
//! - [`MemoryStore`] keeps it in memory, for tests and embedded use.
//!
//! A file that cannot be read as a catalog is an error, never an empty
//! catalog: starting empty would let a server create a new administrator over
//! a damaged file.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::privileges::{builtin_privileges, Access, Privilege, BUILTIN_ROLES};

/// The catalog format this version reads and writes. A file with a higher
/// `format_version` is refused rather than misread. Format 1 had no roles;
/// [`AuthManager::open`](crate::users::AuthManager::open) upgrades it.
pub const FORMAT_VERSION: u32 = 2;

/// Whether a user may log in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserStatus {
    /// The user may log in.
    #[default]
    Active,
    /// The user exists but may not log in.
    Suspended,
}

/// A stored user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// The user's name.
    pub name: String,
    /// The Argon2id PHC string of the password (`$argon2id$…`).
    pub password_hash: String,
    /// Whether the user may log in.
    #[serde(default)]
    pub status: UserStatus,
    /// Whether the user must set a new password before doing anything else.
    #[serde(default)]
    pub password_change_required: bool,
    /// When the user was created, in Unix milliseconds.
    #[serde(default)]
    pub created_at_ms: i64,
    /// When the password was last set, in Unix milliseconds.
    #[serde(default)]
    pub password_changed_at_ms: i64,
    /// The roles given to the user, besides `PUBLIC`, which every user has.
    #[serde(default)]
    pub roles: BTreeSet<String>,
}

/// A privilege a role holds, granted or denied.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HeldPrivilege {
    /// Granted or denied.
    pub access: Access,
    /// What.
    pub privilege: Privilege,
}

/// A stored role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    /// The role's privileges.
    #[serde(default)]
    pub privileges: BTreeSet<HeldPrivilege>,
}

/// The built-in roles with their starting privileges, all granted.
#[must_use]
pub fn builtin_roles() -> BTreeMap<String, Role> {
    BUILTIN_ROLES
        .iter()
        .map(|name| {
            let privileges = builtin_privileges(name)
                .unwrap_or_default()
                .into_iter()
                .map(|privilege| HeldPrivilege {
                    access: Access::Grant,
                    privilege,
                })
                .collect();
            ((*name).to_string(), Role { privileges })
        })
        .collect()
}

/// Every user and role, by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    /// The format the catalog was written in; see [`FORMAT_VERSION`].
    pub format_version: u32,
    /// Users by name.
    #[serde(default)]
    pub users: BTreeMap<String, User>,
    /// Roles by name. A catalog without roles (format 1) starts with the
    /// built-in ones.
    #[serde(default = "builtin_roles")]
    pub roles: BTreeMap<String, Role>,
}

impl Default for Catalog {
    /// No users, and the built-in roles.
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            users: BTreeMap::new(),
            roles: builtin_roles(),
        }
    }
}

/// Why a catalog could not be loaded or saved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// The file could not be read or written.
    #[error("cannot access the security catalog at {path}: {message}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The operating system's message.
        message: String,
    },
    /// The file is not a valid catalog.
    #[error("the security catalog at {path} is damaged: {reason}")]
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// The file was written by a newer version.
    #[error("the security catalog uses format {found}; this version reads up to {supported}")]
    UnsupportedVersion {
        /// The file's `format_version`.
        found: u32,
        /// The highest version this build reads.
        supported: u32,
    },
}

/// Where a [`Catalog`] is kept.
pub trait CatalogStore: Send + Sync {
    /// The stored catalog, or `None` when nothing has been saved yet.
    ///
    /// # Errors
    ///
    /// A [`StoreError`] when the stored catalog cannot be read.
    fn load(&self) -> Result<Option<Catalog>, StoreError>;

    /// Replace the stored catalog with `catalog`. When this returns `Ok`, the
    /// change is stored.
    ///
    /// # Errors
    ///
    /// A [`StoreError`] when the catalog cannot be written; the stored catalog
    /// is then unchanged.
    fn save(&self, catalog: &Catalog) -> Result<(), StoreError>;
}

/// A catalog kept in memory.
#[derive(Debug, Default)]
pub struct MemoryStore {
    catalog: Mutex<Option<Catalog>>,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl CatalogStore for MemoryStore {
    fn load(&self) -> Result<Option<Catalog>, StoreError> {
        Ok(self
            .catalog
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone())
    }

    fn save(&self, catalog: &Catalog) -> Result<(), StoreError> {
        *self.catalog.lock().unwrap_or_else(|p| p.into_inner()) = Some(catalog.clone());
        Ok(())
    }
}

/// How a [`FileStore`] turns the catalog's JSON into the file's bytes and
/// back — to encrypt the file, say.
pub trait FileCodec: Send + Sync + std::fmt::Debug {
    /// The bytes to write for `json`.
    ///
    /// # Errors
    /// When they cannot be made; nothing is written.
    fn encode(&self, json: Vec<u8>) -> std::io::Result<Vec<u8>>;

    /// The JSON in `file`, the bytes read from the store's file.
    ///
    /// # Errors
    /// When the file cannot be decoded, such as with the wrong key.
    fn decode(&self, file: Vec<u8>) -> std::io::Result<Vec<u8>>;
}

/// A catalog kept as JSON in one file.
#[derive(Debug, Clone)]
pub struct FileStore {
    path: PathBuf,
    codec: Option<std::sync::Arc<dyn FileCodec>>,
}

impl FileStore {
    /// A store at `path`. Nothing is read or written until the first
    /// [`load`](CatalogStore::load) or [`save`](CatalogStore::save).
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            codec: None,
        }
    }

    /// Write the file through `codec` and read it back through it.
    #[must_use]
    pub fn with_codec(mut self, codec: std::sync::Arc<dyn FileCodec>) -> Self {
        self.codec = Some(codec);
        self
    }

    /// The file this store uses.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn io(&self, e: &std::io::Error) -> StoreError {
        StoreError::Io {
            path: self.path.clone(),
            message: e.to_string(),
        }
    }

    fn corrupt(&self, reason: impl Into<String>) -> StoreError {
        StoreError::Corrupt {
            path: self.path.clone(),
            reason: reason.into(),
        }
    }
}

impl CatalogStore for FileStore {
    fn load(&self) -> Result<Option<Catalog>, StoreError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(self.io(&e)),
        };
        let bytes = match &self.codec {
            Some(codec) => codec.decode(bytes).map_err(|e| self.io(&e))?,
            None => bytes,
        };
        // Check the version before the shape, so a newer format is reported as
        // such rather than as damage.
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| self.corrupt(e.to_string()))?;
        let version = value
            .get("format_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| self.corrupt("no format_version"))?;
        let version = u32::try_from(version).unwrap_or(u32::MAX);
        if version > FORMAT_VERSION {
            return Err(StoreError::UnsupportedVersion {
                found: version,
                supported: FORMAT_VERSION,
            });
        }
        let catalog: Catalog =
            serde_json::from_value(value).map_err(|e| self.corrupt(e.to_string()))?;
        Ok(Some(catalog))
    }

    fn save(&self, catalog: &Catalog) -> Result<(), StoreError> {
        use std::io::Write;

        let json = serde_json::to_vec_pretty(catalog).map_err(|e| self.corrupt(e.to_string()))?;
        let json = match &self.codec {
            Some(codec) => codec.encode(json).map_err(|e| self.io(&e))?,
            None => json,
        };
        let tmp = self.path.with_extension("json.tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let written = options.open(&tmp).and_then(|mut file| {
            file.write_all(&json)?;
            file.sync_all()
        });
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(self.io(&e));
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(self.io(&e));
        }
        // Make the rename itself durable (best-effort: not every platform can
        // open a directory).
        #[cfg(unix)]
        if let Some(dir) = self.path.parent() {
            if let Ok(dir) = std::fs::File::open(if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            }) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_catalog_without_optional_fields_still_loads() {
        let json = r#"{"format_version": 1, "users": {"ann": {"name": "ann", "password_hash": "$argon2id$x"}}}"#;
        let catalog: Catalog = serde_json::from_str(json).unwrap();
        let ann = &catalog.users["ann"];
        assert_eq!(ann.status, UserStatus::Active);
        assert!(!ann.password_change_required);
    }

    #[test]
    fn status_is_stored_in_lower_case() {
        assert_eq!(
            serde_json::to_string(&UserStatus::Suspended).unwrap(),
            r#""suspended""#
        );
    }

    #[test]
    fn a_catalog_without_a_version_is_damaged() {
        let dir = std::env::temp_dir().join(format!("drevo_auth_cat_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("security.json");
        std::fs::write(&file, r#"{"users": {}}"#).unwrap();
        let err = FileStore::new(&file).load().unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(err, StoreError::Corrupt { .. }), "{err:?}");
    }

    /// Reverses the bytes, and fails to decode anything not written by it.
    #[derive(Debug)]
    struct Reversed;

    impl FileCodec for Reversed {
        fn encode(&self, mut json: Vec<u8>) -> std::io::Result<Vec<u8>> {
            json.reverse();
            json.insert(0, b'#');
            Ok(json)
        }

        fn decode(&self, mut file: Vec<u8>) -> std::io::Result<Vec<u8>> {
            if file.first() != Some(&b'#') {
                return Err(std::io::Error::other("not written by this codec"));
            }
            file.remove(0);
            file.reverse();
            Ok(file)
        }
    }

    #[test]
    fn a_codec_shapes_the_file_both_ways() {
        let dir = std::env::temp_dir().join(format!("drevo_auth_codec_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("security.json");
        let store = FileStore::new(&file).with_codec(std::sync::Arc::new(Reversed));
        let catalog = Catalog {
            format_version: FORMAT_VERSION,
            ..Catalog::default()
        };
        store.save(&catalog).unwrap();
        let raw = std::fs::read(&file).unwrap();
        assert_eq!(raw.first(), Some(&b'#'), "written through the codec");
        assert_eq!(store.load().unwrap(), Some(catalog));
        // Without the codec the file is not a catalog; a plain file does not
        // decode through it.
        assert!(FileStore::new(&file).load().is_err());
        std::fs::write(&file, br#"{"format_version": 1, "users": {}}"#).unwrap();
        let err = store.load().unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(err, StoreError::Io { .. }), "{err:?}");
    }
}
