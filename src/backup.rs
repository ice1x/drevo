//! Physical backups of a whole server, and restoring them into a new data
//! directory.
//!
//! A server backup is a directory with `server.json` and one engine backup
//! per database under `databases/<name>/` (see [`drevo_core::backup`]). The
//! default database's backup also carries the server's own files, the users
//! (`security.json`), the embeddings settings (`embeddings_config.json`) and the
//! text-to-Cypher settings (`text2cypher_config.json`),
//! so they are checked and restored the same way.
//!
//! ```text
//! backups/monday/
//!   server.json
//!   databases/drevo/   manifest.json  native.wal  semantic.json  security.json  …
//!   databases/erp/     manifest.json  native.wal  semantic.json  …
//! ```
//!
//! A backup taken with a previous one backs up each database that continues
//! it incrementally: only the log written since. A database created since,
//! or compacted since, is backed up in full, and the summary says why.
//!
//! Take one from a running server with `POST /admin/backup`, or from a
//! stopped one with `drevo-server backup --to DIR [--since DIR]`. Restore with
//! `drevo-server restore --from DIR [--from DIR …] --to DATA_DIR`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use drevo_core::backup::{BackupError, BackupFile, BackupKind, BackupManifest};

use crate::database_registry::DatabaseRegistry;

/// The server manifest's file name inside a server backup.
pub const SERVER_MANIFEST: &str = "server.json";

/// The server manifest format this version writes and reads.
pub const SERVER_FORMAT: u32 = 1;

/// The server's own files, kept with the default database's backup.
const SERVER_FILES: &[&str] = &[
    "security.json",
    "embeddings_config.json",
    "text2cypher_config.json",
];

/// Where each database's backup lives inside a server backup.
const DATABASES_DIR: &str = "databases";

/// Why a server backup or restore failed.
#[derive(Debug, thiserror::Error)]
pub enum ServerBackupError {
    /// The backup directory already holds files.
    #[error("`{0}` already exists; choose a new name")]
    Exists(String),
    /// The data directory to restore into holds files.
    #[error("`{0}` is not empty; restore into a new data directory")]
    NotEmpty(String),
    /// There is no server backup at that path.
    #[error("no server backup in `{0}`")]
    NoSuchBackup(String),
    /// The server keeps its data in memory.
    #[error("this server keeps its data in memory: there is nothing to back up")]
    InMemory,
    /// A server manifest is unreadable.
    #[error("unreadable server backup manifest in `{dir}`: {message}")]
    Manifest {
        /// The backup directory.
        dir: String,
        /// What is wrong with it.
        message: String,
    },
    /// The databases could not be opened.
    #[error("{0}")]
    Open(String),
    /// Backing up or restoring a database failed.
    #[error(transparent)]
    Engine(#[from] BackupError),
    /// A filesystem error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One database in a server backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseBackup {
    /// The database's name.
    pub name: String,
    /// Full, or incremental.
    pub kind: BackupKind,
    /// The log bytes this backup holds.
    pub wal_bytes: u64,
    /// The database's change-feed head when it was backed up.
    pub change_head: u64,
    /// Why it was backed up in full although a previous backup was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_because: Option<String>,
}

/// `server.json`: what a server backup holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerBackup {
    /// The manifest format ([`SERVER_FORMAT`]).
    pub format: u32,
    /// When it was taken, in Unix milliseconds.
    pub created_at_ms: i64,
    /// The drevo version that took it.
    pub version: String,
    /// The default database's name.
    pub default_database: String,
    /// Every database, by name.
    pub databases: Vec<DatabaseBackup>,
    /// The server's own files kept with the default database.
    pub files: Vec<BackupFile>,
}

impl ServerBackup {
    /// Read the server backup in `dir`.
    ///
    /// # Errors
    /// [`ServerBackupError::NoSuchBackup`] without a `server.json`,
    /// [`ServerBackupError::Manifest`] when it is unreadable or newer.
    pub fn read(dir: &Path) -> Result<Self, ServerBackupError> {
        let bytes = std::fs::read(dir.join(SERVER_MANIFEST))
            .map_err(|_| ServerBackupError::NoSuchBackup(dir.display().to_string()))?;
        let fail = |message: String| ServerBackupError::Manifest {
            dir: dir.display().to_string(),
            message,
        };
        let backup: Self = serde_json::from_slice(&bytes).map_err(|e| fail(e.to_string()))?;
        if backup.format != SERVER_FORMAT {
            return Err(fail(format!(
                "format {} is not {SERVER_FORMAT}; restore with the drevo that wrote it",
                backup.format
            )));
        }
        Ok(backup)
    }
}

/// What [`restore_server`] restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreSummary {
    /// The restored databases, by name.
    pub databases: Vec<String>,
    /// The data directory.
    pub data_dir: PathBuf,
}

/// Whether `dir` is missing or empty.
fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).map_or(true, |mut entries| entries.next().is_none())
}

/// Remove what a failed backup or restore wrote into `dir`: the directory
/// when it did not exist before, its contents when it was empty.
fn clean_up(dir: &Path, existed: bool) {
    if !existed {
        let _ = std::fs::remove_dir_all(dir);
    } else if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }
}

/// Back up every database of `registry` and the server files in `data_dir`
/// into `dir`, a new or empty directory, while the server keeps serving.
/// With `since`, a previous server backup, each database that continues it
/// is backed up incrementally.
///
/// # Errors
/// [`ServerBackupError::Exists`] when `dir` holds files,
/// [`ServerBackupError::NoSuchBackup`] for a missing `since`, and the
/// engine's errors. A failed backup leaves nothing behind.
pub fn backup_server(
    registry: &DatabaseRegistry,
    data_dir: &Path,
    dir: &Path,
    since: Option<&Path>,
) -> Result<ServerBackup, ServerBackupError> {
    if !is_empty_dir(dir) {
        return Err(ServerBackupError::Exists(dir.display().to_string()));
    }
    let previous = since.map(ServerBackup::read).transpose()?;
    let existed = dir.exists();
    let result = (|| {
        let default = registry.default_name();
        let mut databases = Vec::new();
        let mut files = Vec::new();
        for name in registry.list() {
            let Some(service) = registry.get(&name) else {
                continue;
            };
            let base = match (since, &previous) {
                (Some(since), Some(previous))
                    if previous.databases.iter().any(|d| d.name == name) =>
                {
                    Some(BackupManifest::read(
                        &since.join(DATABASES_DIR).join(&name),
                    )?)
                }
                _ => None,
            };
            let continues = base.is_some();
            let extra = if name == default {
                server_files(data_dir)?
            } else {
                Vec::new()
            };
            let manifest = service.backup_to(&dir.join(DATABASES_DIR).join(&name), base, extra)?;
            let full_because = manifest.full_because.clone().or_else(|| {
                (since.is_some() && !continues)
                    .then(|| "there is no earlier backup of this database to continue".to_string())
            });
            if name == default {
                files = manifest
                    .files
                    .iter()
                    .filter(|f| SERVER_FILES.contains(&f.name.as_str()))
                    .cloned()
                    .collect();
            }
            databases.push(DatabaseBackup {
                name,
                kind: manifest.kind,
                wal_bytes: manifest.wal_end - manifest.wal_start,
                change_head: manifest.change_head,
                full_because,
            });
        }
        let summary = ServerBackup {
            format: SERVER_FORMAT,
            created_at_ms: crate::model::now_ms(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            default_database: default.to_string(),
            databases,
            files,
        };
        // The manifest last, so an unfinished backup has none.
        let json = serde_json::to_vec_pretty(&summary).map_err(std::io::Error::other)?;
        let tmp = dir.join("server.json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, dir.join(SERVER_MANIFEST))?;
        Ok(summary)
    })();
    if result.is_err() {
        clean_up(dir, existed);
    }
    result
}

/// The server files present in `data_dir`, by name.
fn server_files(data_dir: &Path) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    let mut files = Vec::new();
    for name in SERVER_FILES {
        match std::fs::read(data_dir.join(name)) {
            Ok(bytes) => files.push(((*name).to_string(), bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(files)
}

/// Restore the server backups in `chain` — a full one, then any taken with
/// `since` the one before, oldest first — into `data_dir`, a new or empty
/// directory a server can then open. Each database is restored from its
/// latest full backup in the chain on; every file is checked first.
///
/// # Errors
/// [`ServerBackupError::NotEmpty`] when `data_dir` holds files, and the
/// engine's errors for a damaged backup or a broken chain. A failed restore
/// leaves nothing behind.
pub fn restore_server(
    chain: &[PathBuf],
    data_dir: &Path,
) -> Result<RestoreSummary, ServerBackupError> {
    if !is_empty_dir(data_dir) {
        return Err(ServerBackupError::NotEmpty(data_dir.display().to_string()));
    }
    let Some(last_dir) = chain.last() else {
        return Err(BackupError::BrokenChain("no backup given".into()).into());
    };
    let manifests = chain
        .iter()
        .map(|dir| ServerBackup::read(dir))
        .collect::<Result<Vec<_>, _>>()?;
    let last = manifests
        .last()
        .ok_or_else(|| ServerBackupError::NoSuchBackup(last_dir.display().to_string()))?;
    let existed = data_dir.exists();
    let result = (|| {
        let mut restored = Vec::new();
        for db in &last.databases {
            // From the latest full backup of this database on.
            let mut dirs: Vec<PathBuf> = Vec::new();
            for (dir, manifest) in chain.iter().zip(&manifests) {
                if let Some(entry) = manifest.databases.iter().find(|d| d.name == db.name) {
                    if entry.kind == BackupKind::Full {
                        dirs.clear();
                    }
                    dirs.push(dir.join(DATABASES_DIR).join(&db.name));
                }
            }
            let wal = if db.name == last.default_database {
                data_dir.join("native.wal")
            } else {
                data_dir
                    .join(DATABASES_DIR)
                    .join(&db.name)
                    .join("native.wal")
            };
            drevo_core::backup::restore(&dirs, &wal)?;
            restored.push(db.name.clone());
        }
        Ok(RestoreSummary {
            databases: restored,
            data_dir: data_dir.to_path_buf(),
        })
    })();
    if result.is_err() {
        clean_up(data_dir, existed);
    }
    result
}

/// Whether `name` may name a backup in the server's backup directory:
/// letters, digits, `_`, `-` and `.`, not starting with `.`, at most 100.
#[must_use]
pub fn valid_backup_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// A backup name from the time, `backup-YYYYMMDD-HHMMSS` in UTC.
#[must_use]
pub fn timestamp_name(unix_ms: i64) -> String {
    let secs = unix_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "backup-{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

const USAGE: &str = "usage:\n  drevo-server backup --to DIR [--since DIR]\n  \
                     drevo-server restore --from DIR [--from DIR …] --to DATA_DIR";

/// Run `drevo-server backup …` or `drevo-server restore …`; `None` when
/// `args` (without the program name) is not one of them, so the server
/// starts as usual. `env` reads the environment (`DREVO_DATA_DIR`).
///
/// `backup` works on a stopped server; a running one is backed up with
/// `POST /admin/backup`.
pub fn command(
    args: &[String],
    env: impl Fn(&str) -> Option<String>,
) -> Option<Result<String, String>> {
    let (verb, rest) = args.split_first()?;
    let usage = |why: &str| Err(format!("{why}\n{USAGE}"));
    let mut to = None;
    let mut since = None;
    let mut from = Vec::new();
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        let Some(value) = it.next() else {
            return Some(usage(&format!("`{flag}` needs a value")));
        };
        match (verb.as_str(), flag.as_str()) {
            (_, "--to") => to = Some(PathBuf::from(value)),
            ("backup", "--since") => since = Some(PathBuf::from(value)),
            ("restore", "--from") => from.push(PathBuf::from(value)),
            _ => return Some(usage(&format!("unknown option `{flag}`"))),
        }
    }
    match verb.as_str() {
        "backup" => {
            let Some(to) = to else {
                return Some(usage("`backup` needs `--to DIR`"));
            };
            // An encrypted server's stores open with its key.
            #[cfg(feature = "encryption")]
            match crate::server::encryption_key(&env) {
                Ok(key) => crate::native_service::NativeService::set_encryption_key(key),
                Err(e) => return Some(Err(e.to_string())),
            }
            let data_dir = PathBuf::from(env("DREVO_DATA_DIR").unwrap_or_else(|| "/data".into()));
            Some(backup_command(&data_dir, &to, since.as_deref()))
        }
        "restore" => {
            let Some(to) = to else {
                return Some(usage("`restore` needs `--to DATA_DIR`"));
            };
            if from.is_empty() {
                return Some(usage("`restore` needs at least one `--from DIR`"));
            }
            Some(
                restore_server(&from, &to)
                    .map(|s| {
                        format!(
                            "restored {} database(s) ({}) into {}",
                            s.databases.len(),
                            s.databases.join(", "),
                            s.data_dir.display()
                        )
                    })
                    .map_err(|e| e.to_string()),
            )
        }
        _ => None,
    }
}

fn backup_command(data_dir: &Path, to: &Path, since: Option<&Path>) -> Result<String, String> {
    let default = crate::native_service::NativeService::open(data_dir.join("native.wal")).map_err(
        |e| match e {
            crate::error::DrevoError::Locked => format!(
                "a server is running on {}: back it up with POST /admin/backup",
                data_dir.display()
            ),
            other => format!("cannot open {}: {other}", data_dir.display()),
        },
    )?;
    let registry = DatabaseRegistry::with_durable_dir(std::sync::Arc::new(default), data_dir)
        .map_err(|e| format!("cannot open the databases in {}: {e}", data_dir.display()))?;
    let summary = backup_server(&registry, data_dir, to, since).map_err(|e| e.to_string())?;
    let mut out = format!("backed up to {}\n", to.display());
    for db in &summary.databases {
        let kind = match db.kind {
            BackupKind::Full => "full",
            BackupKind::Incremental => "incremental",
        };
        out.push_str(&format!(
            "  {}: {kind}, {} log bytes",
            db.name, db.wal_bytes
        ));
        if let Some(why) = &db.full_because {
            out.push_str(&format!(" ({why})"));
        }
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names_are_plain() {
        for good in ["monday", "2026-10-17", "nightly_1.2", "a"] {
            assert!(valid_backup_name(good), "{good}");
        }
        for bad in [
            "",
            ".hidden",
            "..",
            "a/b",
            "a\\b",
            "x y",
            "ü",
            &"x".repeat(101),
        ] {
            assert!(!valid_backup_name(bad), "{bad}");
        }
    }

    #[test]
    fn timestamp_names_read_as_utc_dates() {
        assert_eq!(timestamp_name(0), "backup-19700101-000000");
        // 2026-10-17 15:30:12 UTC.
        assert_eq!(timestamp_name(1_792_251_012_000), "backup-20261017-153012");
        // A leap day.
        assert_eq!(timestamp_name(951_782_400_000), "backup-20000229-000000");
        assert_eq!(timestamp_name(1_709_251_199_000), "backup-20240229-235959");
    }

    #[test]
    fn other_arguments_are_not_backup_commands() {
        assert!(command(&[], |_| None).is_none());
        assert!(command(&["--help".to_string()], |_| None).is_none());
    }
}
