//! Encryption at rest on the server: the key from `DREVO_ENCRYPTION_KEY_FILE`
//! encrypts every database's log and sidecars, and `drevo-server encrypt` /
//! `rotate-key` / `decrypt` move a stopped server's data between keys.
#![cfg(all(feature = "encryption", feature = "http"))]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use drevo::cypher::parser::parse;
use drevo::database_registry::DatabaseRegistry;
use drevo::native_service::NativeService;
use drevo::server::Config;
use drevo_core::encryption::Key;

/// The key is process-wide: the tests here take turns.
static TURN: Mutex<()> = Mutex::new(());

const HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const OTHER_HEX: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";
const SECRET: &str = "acct-4417-confidential";

struct Turn {
    _guard: std::sync::MutexGuard<'static, ()>,
}
impl Drop for Turn {
    fn drop(&mut self) {
        NativeService::set_encryption_key(None);
    }
}
fn turn() -> Turn {
    Turn {
        _guard: TURN.lock().unwrap_or_else(|e| e.into_inner()),
    }
}

fn key() -> Key {
    Key::from_hex(HEX).unwrap()
}

fn run(svc: &NativeService, q: &str) {
    svc.execute(&parse(q).unwrap(), HashMap::new())
        .unwrap_or_else(|e| panic!("`{q}`: {e}"));
}

fn contains(bytes: &[u8], needle: &str) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Every non-empty file under `dir`, except lock files, is encrypted and
/// holds none of `needles`.
fn assert_all_encrypted(dir: &Path, needles: &[&str]) {
    let mut checked = 0;
    for entry in walk(dir) {
        let name = entry.to_string_lossy().to_string();
        if name.ends_with(".lock") {
            continue;
        }
        let bytes = std::fs::read(&entry).unwrap();
        if bytes.is_empty() {
            continue;
        }
        assert!(
            bytes.starts_with(b"DRVWAL") || bytes.starts_with(b"DRVSEAL1"),
            "{name} is not encrypted"
        );
        for needle in needles {
            assert!(
                !contains(&bytes, needle),
                "`{needle}` is readable in {name}"
            );
        }
        checked += 1;
    }
    assert!(checked > 0, "no files under {}", dir.display());
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn the_key_comes_from_a_file_and_bad_keys_are_refused() {
    let _turn = turn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("drevo.key");
    std::fs::write(&file, format!("{HEX}\n")).unwrap();
    let path = file.to_string_lossy().to_string();
    let env = |pairs: Vec<(&'static str, String)>| {
        let map: HashMap<&str, String> = pairs.into_iter().collect();
        move |k: &str| map.get(k).cloned()
    };

    let cfg = Config::from_env(env(vec![("DREVO_ENCRYPTION_KEY_FILE", path.clone())])).unwrap();
    assert!(cfg.encryption_key.is_some());
    assert!(
        !format!("{cfg:?}").contains(HEX),
        "the key must not be printed"
    );
    let inline = Config::from_env(env(vec![("DREVO_ENCRYPTION_KEY", HEX.to_string())])).unwrap();
    assert!(inline.encryption_key.is_some());
    assert!(Config::from_env(env(vec![]))
        .unwrap()
        .encryption_key
        .is_none());

    for bad in [
        vec![
            ("DREVO_ENCRYPTION_KEY_FILE", path.clone()),
            ("DREVO_ENCRYPTION_KEY", HEX.to_string()),
        ],
        vec![("DREVO_ENCRYPTION_KEY", "not-hex".to_string())],
        vec![("DREVO_ENCRYPTION_KEY", HEX[..60].to_string())],
        vec![(
            "DREVO_ENCRYPTION_KEY_FILE",
            "/nonexistent/drevo.key".to_string(),
        )],
    ] {
        let err = Config::from_env(env(bad)).unwrap_err();
        assert!(err.to_string().contains("encryption key"), "{err}");
    }
}

#[test]
fn an_encrypted_server_seals_every_database_and_file() {
    let _turn = turn();
    let dir = tempfile::tempdir().unwrap();
    NativeService::set_encryption_key(Some(key()));
    {
        let default = Arc::new(NativeService::open(dir.path().join("native.wal")).unwrap());
        run(
            &default,
            &format!("CREATE (:Account {{title: '{SECRET}', owner: 'ann'}})"),
        );
        run(
            &default,
            "CREATE CONSTRAINT account_title FOR (a:Account) REQUIRE a.title IS UNIQUE",
        );
        let registry = DatabaseRegistry::with_durable_dir(default, dir.path()).unwrap();
        let crm = registry.create("crm").unwrap();
        run(&crm, &format!("CREATE (:Lead {{title: '{SECRET}'}})"));
        registry.set_access("crm", true).unwrap();
    }
    assert_all_encrypted(dir.path(), &[SECRET, "account_title", "crm\"", "Account"]);

    // Reopened with the key, everything is there.
    let default = Arc::new(NativeService::open(dir.path().join("native.wal")).unwrap());
    let shown = default
        .execute(&parse("SHOW CONSTRAINTS").unwrap(), HashMap::new())
        .unwrap();
    assert_eq!(shown.rows.len(), 1);
    let registry = DatabaseRegistry::with_durable_dir(default, dir.path()).unwrap();
    assert!(registry.get("crm").unwrap().is_read_only());
    drop(registry);

    // Without the key, or with another, nothing opens.
    NativeService::set_encryption_key(None);
    assert!(NativeService::open(dir.path().join("native.wal")).is_err());
    NativeService::set_encryption_key(Some(Key::from_hex(OTHER_HEX).unwrap()));
    assert!(NativeService::open(dir.path().join("native.wal")).is_err());
}

#[test]
fn encrypt_rotate_and_decrypt_a_stopped_server() {
    let _turn = turn();
    let dir = tempfile::tempdir().unwrap();
    {
        let default = Arc::new(NativeService::open(dir.path().join("native.wal")).unwrap());
        run(
            &default,
            &format!("CREATE (:Account {{title: '{SECRET}'}})"),
        );
        run(
            &default,
            "CREATE CONSTRAINT account_title FOR (a:Account) REQUIRE a.title IS UNIQUE",
        );
        let registry = DatabaseRegistry::with_durable_dir(default, dir.path()).unwrap();
        let crm = registry.create("crm").unwrap();
        run(&crm, &format!("CREATE (:Lead {{title: '{SECRET}'}})"));
        registry.set_access("crm", true).unwrap();
    }
    let keys = tempfile::tempdir().unwrap();
    let old_key = keys.path().join("old.key");
    let new_key = keys.path().join("new.key");
    std::fs::write(&old_key, HEX).unwrap();
    std::fs::write(&new_key, OTHER_HEX).unwrap();
    let data = dir.path().to_string_lossy().to_string();
    let env = |key_file: &Path| {
        let data = data.clone();
        let key_file = key_file.to_string_lossy().to_string();
        move |k: &str| match k {
            "DREVO_DATA_DIR" => Some(data.clone()),
            "DREVO_ENCRYPTION_KEY_FILE" => Some(key_file.clone()),
            _ => None,
        }
    };
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    let out = drevo::encryption_cmd::command(&args(&["encrypt"]), env(&old_key))
        .unwrap()
        .unwrap();
    assert!(out.contains("encrypted 2 database(s)"), "{out}");
    assert_all_encrypted(dir.path(), &[SECRET, "account_title"]);
    NativeService::set_encryption_key(Some(key()));
    {
        let default = Arc::new(NativeService::open(dir.path().join("native.wal")).unwrap());
        let registry = DatabaseRegistry::with_durable_dir(default, dir.path()).unwrap();
        assert!(registry.get("crm").unwrap().is_read_only());
    }

    let out = drevo::encryption_cmd::command(
        &args(&["rotate-key", "--new-key-file", &new_key.to_string_lossy()]),
        env(&old_key),
    )
    .unwrap()
    .unwrap();
    assert!(out.contains("new key"), "{out}");
    assert!(
        NativeService::open(dir.path().join("native.wal")).is_err(),
        "the old key is gone"
    );
    NativeService::set_encryption_key(Some(Key::from_hex(OTHER_HEX).unwrap()));
    drop(NativeService::open(dir.path().join("native.wal")).unwrap());

    drevo::encryption_cmd::command(&args(&["decrypt"]), env(&new_key))
        .unwrap()
        .unwrap();
    NativeService::set_encryption_key(None);
    let default = Arc::new(NativeService::open(dir.path().join("native.wal")).unwrap());
    let registry = DatabaseRegistry::with_durable_dir(default, dir.path()).unwrap();
    assert!(registry.get("crm").unwrap().is_read_only());
    assert!(contains(
        &std::fs::read(dir.path().join("native.wal")).unwrap(),
        SECRET
    ));

    // Not an encryption command: someone else's to handle.
    assert!(drevo::encryption_cmd::command(&args(&["backup"]), env(&new_key)).is_none());
    let err = drevo::encryption_cmd::command(&args(&["encrypt", "--bogus", "x"]), env(&old_key))
        .unwrap()
        .unwrap_err();
    assert!(err.contains("unknown option"), "{err}");
}

#[test]
fn a_backup_of_an_encrypted_server_stays_encrypted() {
    let _turn = turn();
    let dir = tempfile::tempdir().unwrap();
    NativeService::set_encryption_key(Some(key()));
    {
        let svc = NativeService::open(dir.path().join("native.wal")).unwrap();
        run(&svc, &format!("CREATE (:Account {{title: '{SECRET}'}})"));
        run(
            &svc,
            "CREATE CONSTRAINT account_title FOR (a:Account) REQUIRE a.title IS UNIQUE",
        );
    }
    NativeService::set_encryption_key(None);
    let backup = tempfile::tempdir().unwrap();
    let to = backup.path().join("b1");
    let data = dir.path().to_string_lossy().to_string();
    let out = drevo::backup::command(
        &[
            "backup".to_string(),
            "--to".to_string(),
            to.to_string_lossy().to_string(),
        ],
        |k| match k {
            "DREVO_DATA_DIR" => Some(data.clone()),
            "DREVO_ENCRYPTION_KEY" => Some(HEX.to_string()),
            _ => None,
        },
    )
    .unwrap()
    .unwrap();
    assert!(out.contains("backed up"), "{out}");
    let files: Vec<_> = walk(&to)
        .into_iter()
        .filter(|p| {
            !p.ends_with("manifest.json") && !p.to_string_lossy().ends_with("manifest.json")
        })
        .collect();
    assert!(!files.is_empty());
    for f in files {
        let bytes = std::fs::read(&f).unwrap();
        assert!(!contains(&bytes, SECRET), "{} holds plaintext", f.display());
        assert!(
            !contains(&bytes, "account_title"),
            "{} holds plaintext",
            f.display()
        );
    }
}

#[cfg(feature = "auth")]
mod server_files {
    use super::*;

    use drevo::embeddings::{EmbeddingsConfigStore, EmbeddingsConfigUpdate};
    use drevo::security::open_server_security;
    use drevo_auth::authn::Credentials;
    use drevo_auth::users::Bootstrap;

    const API_KEY: &str = "sk-embeddings-do-not-leak";
    const PASSWORD: &str = "hunter2-but-longer";

    fn admin() -> Bootstrap {
        Bootstrap::Admin {
            user: "ann".to_string(),
            password: PASSWORD.to_string(),
        }
    }

    /// Write the server's users and embeddings settings into `dir`.
    fn write_server_files(dir: &Path) {
        open_server_security(Some(dir), &admin()).unwrap();
        let store = EmbeddingsConfigStore::load(dir.join("embeddings_config.json"), None);
        store
            .apply(EmbeddingsConfigUpdate {
                upstream: "https://embeddings.example.com/v1/embeddings".to_string(),
                api_key: Some(API_KEY.to_string()),
                model: Some("text-embedding-3-small".to_string()),
            })
            .unwrap();
    }

    fn api_key_in(dir: &Path) -> Option<String> {
        EmbeddingsConfigStore::load(dir.join("embeddings_config.json"), None)
            .snapshot()
            .and_then(|c| c.api_key)
    }

    fn ann_logs_in(dir: &Path) -> bool {
        let (security, _) = open_server_security(Some(dir), &admin()).unwrap();
        security.users().is_some_and(|u| {
            u.authenticate(&Credentials::Basic {
                username: "ann".into(),
                password: PASSWORD.into(),
            })
            .is_ok()
        })
    }

    #[test]
    fn users_and_embeddings_settings_are_sealed_with_everything_else() {
        let _turn = turn();
        let dir = tempfile::tempdir().unwrap();
        NativeService::set_encryption_key(Some(key()));
        {
            let svc = NativeService::open(dir.path().join("native.wal")).unwrap();
            run(&svc, &format!("CREATE (:Account {{title: '{SECRET}'}})"));
        }
        write_server_files(dir.path());
        // No exceptions: nothing in the data directory is readable.
        assert_all_encrypted(
            dir.path(),
            &[SECRET, "ann", "argon2", API_KEY, "embeddings.example.com"],
        );

        // With the key they read back.
        assert!(ann_logs_in(dir.path()));
        assert_eq!(api_key_in(dir.path()).as_deref(), Some(API_KEY));

        // Without it the users do not open, and the settings are not used.
        NativeService::set_encryption_key(None);
        assert!(open_server_security(Some(dir.path()), &admin()).is_err());
        assert_eq!(api_key_in(dir.path()), None);
    }

    #[test]
    fn encrypt_rotate_and_decrypt_cover_the_server_files() {
        let _turn = turn();
        let dir = tempfile::tempdir().unwrap();
        {
            let svc = NativeService::open(dir.path().join("native.wal")).unwrap();
            run(&svc, "CREATE (:Account {title: 'plain'})");
        }
        write_server_files(dir.path());
        let security = dir.path().join("security.json");
        #[cfg(unix)]
        let mode = |p: &Path| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p).unwrap().permissions().mode() & 0o777
        };
        #[cfg(unix)]
        assert_eq!(mode(&security), 0o600);

        let keys = tempfile::tempdir().unwrap();
        let (old_key, new_key) = (keys.path().join("old.key"), keys.path().join("new.key"));
        std::fs::write(&old_key, HEX).unwrap();
        std::fs::write(&new_key, OTHER_HEX).unwrap();
        let data = dir.path().to_string_lossy().to_string();
        let env = |key_file: &Path| {
            let data = data.clone();
            let key_file = key_file.to_string_lossy().to_string();
            move |k: &str| match k {
                "DREVO_DATA_DIR" => Some(data.clone()),
                "DREVO_ENCRYPTION_KEY_FILE" => Some(key_file.clone()),
                _ => None,
            }
        };
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        drevo::encryption_cmd::command(&args(&["encrypt"]), env(&old_key))
            .unwrap()
            .unwrap();
        assert_all_encrypted(dir.path(), &["ann", API_KEY]);
        #[cfg(unix)]
        assert_eq!(mode(&security), 0o600, "still owner-only");
        NativeService::set_encryption_key(Some(key()));
        assert!(ann_logs_in(dir.path()));
        assert_eq!(api_key_in(dir.path()).as_deref(), Some(API_KEY));

        drevo::encryption_cmd::command(
            &args(&["rotate-key", "--new-key-file", &new_key.to_string_lossy()]),
            env(&old_key),
        )
        .unwrap()
        .unwrap();
        assert!(
            open_server_security(Some(dir.path()), &admin()).is_err(),
            "the old key is gone"
        );
        NativeService::set_encryption_key(Some(Key::from_hex(OTHER_HEX).unwrap()));
        assert!(ann_logs_in(dir.path()));

        drevo::encryption_cmd::command(&args(&["decrypt"]), env(&new_key))
            .unwrap()
            .unwrap();
        NativeService::set_encryption_key(None);
        assert!(ann_logs_in(dir.path()));
        assert_eq!(api_key_in(dir.path()).as_deref(), Some(API_KEY));
        assert!(contains(&std::fs::read(&security).unwrap(), "ann"));
    }
}
