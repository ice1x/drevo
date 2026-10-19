//! `drevo-server encrypt`, `decrypt` and `rotate-key`: rewrite a stopped
//! server's data directory under another key.
//!
//! ```text
//! DREVO_ENCRYPTION_KEY_FILE=/run/secrets/drevo.key drevo-server encrypt
//! DREVO_ENCRYPTION_KEY_FILE=/run/secrets/drevo.key drevo-server decrypt
//! DREVO_ENCRYPTION_KEY_FILE=/run/secrets/old.key drevo-server rotate-key --new-key-file /run/secrets/new.key
//! ```
//!
//! Every database in `DREVO_DATA_DIR` — its log, the engine's sidecars and
//! `semantic.json` — and the server's `database_access.json` is rewritten.
//! The server must be stopped (a running one holds the stores' locks). Each
//! file is replaced atomically, but not all of them at once: back the data
//! directory up first (`drevo-server backup`).

use std::path::{Path, PathBuf};

use drevo_core::encryption::{read_file, write_file, Key};

use crate::database_registry::{ACCESS_CONTEXT, ACCESS_FILE, DATABASES_SUBDIR, DB_WAL_FILE};
use crate::native::NativeGraph;
use crate::native_service::SEMANTIC_CONTEXT;

const USAGE: &str = "usage: drevo-server encrypt | decrypt | rotate-key --new-key-file FILE\n\
                     (the current key comes from DREVO_ENCRYPTION_KEY_FILE or DREVO_ENCRYPTION_KEY)";

/// Run an encryption command when `args` start with one; `None` otherwise.
/// `env` reads the environment (`DREVO_DATA_DIR`, `DREVO_WAL_FORMAT`, the key
/// variables).
pub fn command(
    args: &[String],
    env: impl Fn(&str) -> Option<String>,
) -> Option<Result<String, String>> {
    let (verb, rest) = args.split_first()?;
    if !matches!(verb.as_str(), "encrypt" | "decrypt" | "rotate-key") {
        return None;
    }
    Some(run(verb, rest, &env))
}

fn run(
    verb: &str,
    rest: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let usage = |why: &str| format!("{why}\n{USAGE}");
    let mut new_key_file = None;
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match (verb, flag.as_str()) {
            ("rotate-key", "--new-key-file") => {
                new_key_file = Some(
                    it.next()
                        .ok_or_else(|| usage("`--new-key-file` needs a value"))?,
                )
            }
            _ => return Err(usage(&format!("unknown option `{flag}`"))),
        }
    }
    let key = crate::server::encryption_key(&|k: &str| env(k)).map_err(|e| e.to_string())?;
    let (from, to) = match verb {
        "encrypt" => (
            None,
            Some(key.ok_or_else(|| usage("`encrypt` needs the key to encrypt with"))?),
        ),
        "decrypt" => (
            Some(key.ok_or_else(|| usage("`decrypt` needs the current key"))?),
            None,
        ),
        _ => {
            let current = key.ok_or_else(|| usage("`rotate-key` needs the current key"))?;
            let file =
                new_key_file.ok_or_else(|| usage("`rotate-key` needs `--new-key-file FILE`"))?;
            let text =
                std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
            let new = Key::from_hex(&text).map_err(|e| format!("{file}: {e}"))?;
            (Some(current), Some(new))
        }
    };
    let format = match env("DREVO_WAL_FORMAT") {
        Some(name) => name.parse().map_err(|e| format!("DREVO_WAL_FORMAT: {e}"))?,
        None => crate::wal_format::WalFormat::default(),
    };
    let data_dir = PathBuf::from(env("DREVO_DATA_DIR").unwrap_or_else(|| "/data".into()));
    let stores = stores(&data_dir)?;
    for wal in &stores {
        NativeGraph::reencrypt(wal, format, from.clone(), to.clone()).map_err(|e| match e {
            drevo_core::error::CoreError::Locked => format!(
                "{} is open: stop the server before changing its encryption",
                wal.display()
            ),
            other => format!("{}: {other}", wal.display()),
        })?;
        rekey_file(
            &wal.with_file_name("semantic.json"),
            SEMANTIC_CONTEXT,
            from.as_ref(),
            to.as_ref(),
        )?;
    }
    rekey_file(
        &data_dir.join(ACCESS_FILE),
        ACCESS_CONTEXT,
        from.as_ref(),
        to.as_ref(),
    )?;
    let done = match verb {
        "encrypt" => "encrypted",
        "decrypt" => "decrypted",
        _ => "re-encrypted with the new key",
    };
    Ok(format!(
        "{done} {} database(s) in {}",
        stores.len(),
        data_dir.display()
    ))
}

/// The log of the default database and of every database under
/// `databases/`.
fn stores(data_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut stores = Vec::new();
    let default = data_dir.join(DB_WAL_FILE);
    if default.exists() {
        stores.push(default);
    }
    let dbs = data_dir.join(DATABASES_SUBDIR);
    if dbs.is_dir() {
        let entries = std::fs::read_dir(&dbs).map_err(|e| format!("{}: {e}", dbs.display()))?;
        let mut more: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path().join(DB_WAL_FILE))
            .filter(|wal| wal.exists())
            .collect();
        more.sort();
        stores.extend(more);
    }
    if stores.is_empty() {
        return Err(format!("no databases in {}", data_dir.display()));
    }
    Ok(stores)
}

/// Rewrite the sidecar at `path` (if there is one) from key `from` to key
/// `to`.
fn rekey_file(
    path: &Path,
    context: &[u8],
    from: Option<&Key>,
    to: Option<&Key>,
) -> Result<(), String> {
    let bytes = read_file(path, from, context).map_err(|e| e.to_string())?;
    if let Some(bytes) = bytes {
        write_file(path, to, context, &bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}
