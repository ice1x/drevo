//! User management: the security catalog of users and their passwords, kept
//! in a [`CatalogStore`] and changed through an [`AuthManager`].
#![cfg(feature = "password")]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use drevo_auth::catalog::{CatalogStore, FileStore, MemoryStore, StoreError};
use drevo_auth::users::{
    AlterUser, AuthManager, Bootstrap, BootstrapOutcome, ManageError, NewUser, PasswordPolicy,
    UserStatus,
};

// std-only temp dir, removed on drop.
static NEXT: AtomicU64 = AtomicU64::new(0);
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_auth_users_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn file(&self) -> PathBuf {
        self.0.join("security.json")
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn manager() -> AuthManager {
    AuthManager::open(Arc::new(MemoryStore::new())).unwrap()
}

fn user(name: &str, password: &str) -> NewUser {
    NewUser::new(name, password)
}

// ── Names and passwords ──────────────────────────────────────────────

#[test]
fn user_names_follow_neo4j_rules() {
    let m = manager();
    for ok in [
        "neo4j",
        "ann.lee",
        "ann_lee",
        "ann-lee",
        "ann@example.com",
        "A1",
    ] {
        m.create_user(user(ok, "password1"))
            .unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    let too_long = "a".repeat(66);
    for bad in [
        "",
        "ann lee",
        "ann'",
        "ann\"",
        "ann/lee",
        "ann;",
        too_long.as_str(),
    ] {
        assert!(
            matches!(
                m.create_user(user(bad, "password1")),
                Err(ManageError::InvalidName { .. })
            ),
            "{bad:?} must be refused"
        );
    }
    m.create_user(user(&"a".repeat(65), "password1")).unwrap();
}

#[test]
fn passwords_shorter_than_the_policy_are_refused() {
    let m = manager();
    assert!(matches!(
        m.create_user(user("ann", "1234567")),
        Err(ManageError::WeakPassword { min_length: 8 })
    ));
    m.create_user(user("ann", "12345678")).unwrap();
    // Length is counted in characters, not bytes.
    assert!(matches!(
        m.create_user(user("bob", "ключ123")),
        Err(ManageError::WeakPassword { .. })
    ));
    m.create_user(user("bob", "ключ1234")).unwrap();
}

#[test]
fn the_password_policy_is_configurable() {
    let m = AuthManager::open(Arc::new(MemoryStore::new()))
        .unwrap()
        .with_policy(PasswordPolicy { min_length: 12 });
    assert!(matches!(
        m.create_user(user("ann", "only11chars")),
        Err(ManageError::WeakPassword { min_length: 12 })
    ));
    m.create_user(user("ann", "twelve chars")).unwrap();
}

// ── Create, list, alter, drop ────────────────────────────────────────

#[test]
fn a_new_user_is_active_and_must_change_the_password_by_default() {
    let m = manager();
    m.create_user(user("ann", "password1")).unwrap();
    let ann = m.user("ann").unwrap();
    assert_eq!(ann.name, "ann");
    assert_eq!(ann.status, UserStatus::Active);
    assert!(ann.password_change_required);
    assert!(m.check_password("ann", "password1"));
    assert!(!m.check_password("ann", "password2"));
}

#[test]
fn create_options_set_status_and_change_requirement() {
    let m = manager();
    m.create_user(
        user("ann", "password1")
            .change_required(false)
            .status(UserStatus::Suspended),
    )
    .unwrap();
    let ann = m.user("ann").unwrap();
    assert!(!ann.password_change_required);
    assert_eq!(ann.status, UserStatus::Suspended);
}

#[test]
fn creating_an_existing_user_fails_unless_if_not_exists() {
    let m = manager();
    m.create_user(user("ann", "password1")).unwrap();
    assert!(matches!(
        m.create_user(user("ann", "password2")),
        Err(ManageError::UserExists(name)) if name == "ann"
    ));
    // IF NOT EXISTS is a no-op: the first password still holds.
    m.create_user(user("ann", "password2").if_not_exists())
        .unwrap();
    assert!(m.check_password("ann", "password1"));
}

#[test]
fn users_are_listed_by_name_without_their_hashes() {
    let m = manager();
    for name in ["carol", "ann", "bob"] {
        m.create_user(user(name, "password1")).unwrap();
    }
    let names: Vec<String> = m.users().into_iter().map(|u| u.name).collect();
    assert_eq!(names, ["ann", "bob", "carol"]);
    let shown = format!("{:?}", m.users());
    assert!(!shown.contains("$argon2"), "listing leaks a hash: {shown}");
    assert!(!shown.contains("password1"));
}

#[test]
fn alter_user_changes_password_status_and_requirement() {
    let m = manager();
    m.create_user(user("ann", "password1")).unwrap();
    m.alter_user(
        "ann",
        AlterUser::default()
            .password("password2")
            .change_required(false)
            .status(UserStatus::Suspended),
    )
    .unwrap();
    let ann = m.user("ann").unwrap();
    assert!(m.check_password("ann", "password2"));
    assert!(!m.check_password("ann", "password1"));
    assert!(!ann.password_change_required);
    assert_eq!(ann.status, UserStatus::Suspended);
}

#[test]
fn alter_user_validates_the_new_password() {
    let m = manager();
    m.create_user(user("ann", "password1")).unwrap();
    assert!(matches!(
        m.alter_user("ann", AlterUser::default().password("short")),
        Err(ManageError::WeakPassword { .. })
    ));
    assert!(matches!(
        m.alter_user("ann", AlterUser::default().password("password1")),
        Err(ManageError::SamePassword)
    ));
    assert!(
        m.check_password("ann", "password1"),
        "a refused change changes nothing"
    );
}

#[test]
fn altering_or_dropping_an_unknown_user_fails() {
    let m = manager();
    assert!(matches!(
        m.alter_user("zed", AlterUser::default().change_required(false)),
        Err(ManageError::UserNotFound(name)) if name == "zed"
    ));
    assert!(matches!(
        m.drop_user("zed", false),
        Err(ManageError::UserNotFound(_))
    ));
    m.drop_user("zed", true).unwrap();
}

#[test]
fn a_dropped_user_cannot_log_in() {
    let m = manager();
    m.create_user(user("ann", "password1")).unwrap();
    m.drop_user("ann", false).unwrap();
    assert!(m.user("ann").is_none());
    assert!(!m.check_password("ann", "password1"));
}

#[test]
fn a_user_changes_their_own_password_with_the_current_one() {
    let m = manager();
    m.create_user(user("ann", "password1")).unwrap();
    assert!(matches!(
        m.change_own_password("ann", "wrong-old", "password2"),
        Err(ManageError::WrongCurrentPassword)
    ));
    assert!(matches!(
        m.change_own_password("ann", "password1", "password1"),
        Err(ManageError::SamePassword)
    ));
    assert!(matches!(
        m.change_own_password("ann", "password1", "short"),
        Err(ManageError::WeakPassword { .. })
    ));
    m.change_own_password("ann", "password1", "password2")
        .unwrap();
    assert!(m.check_password("ann", "password2"));
    assert!(
        !m.user("ann").unwrap().password_change_required,
        "changing the password clears the requirement"
    );
}

#[test]
fn errors_never_contain_a_password() {
    let m = manager();
    m.create_user(user("ann", "password1")).unwrap();
    let errors = [
        m.create_user(user("bob", "secret")).unwrap_err(),
        m.alter_user("ann", AlterUser::default().password("password1"))
            .unwrap_err(),
        m.change_own_password("ann", "guess-old-pw", "password2")
            .unwrap_err(),
    ];
    for e in errors {
        let text = format!("{e} {e:?}");
        for secret in ["secret", "password1", "guess-old-pw", "password2"] {
            assert!(!text.contains(secret), "{text:?} contains {secret:?}");
        }
    }
}

// ── Persistence ──────────────────────────────────────────────────────

#[test]
fn users_survive_reopening_a_file_store() {
    let dir = TmpDir::new();
    {
        let m = AuthManager::open(Arc::new(FileStore::new(dir.file()))).unwrap();
        m.create_user(user("ann", "password1").change_required(false))
            .unwrap();
        m.create_user(user("bob", "password2").status(UserStatus::Suspended))
            .unwrap();
        m.drop_user("bob", false).unwrap();
        m.create_user(user("carol", "password3")).unwrap();
    }
    let m = AuthManager::open(Arc::new(FileStore::new(dir.file()))).unwrap();
    let names: Vec<String> = m.users().into_iter().map(|u| u.name).collect();
    assert_eq!(names, ["ann", "carol"]);
    assert!(m.check_password("ann", "password1"));
    assert!(!m.user("ann").unwrap().password_change_required);
    assert!(m.user("carol").unwrap().password_change_required);
}

#[test]
fn every_change_is_saved_before_the_call_returns() {
    let dir = TmpDir::new();
    let store = Arc::new(FileStore::new(dir.file()));
    let m = AuthManager::open(store.clone()).unwrap();
    m.create_user(user("ann", "password1")).unwrap();
    let saved = store.load().unwrap().expect("catalog written");
    assert!(saved.users.contains_key("ann"));
    m.alter_user("ann", AlterUser::default().status(UserStatus::Suspended))
        .unwrap();
    assert_eq!(
        store.load().unwrap().unwrap().users["ann"].status,
        UserStatus::Suspended
    );
}

#[test]
fn the_file_holds_hashes_never_passwords() {
    let dir = TmpDir::new();
    let m = AuthManager::open(Arc::new(FileStore::new(dir.file()))).unwrap();
    m.create_user(user("ann", "correct horse battery")).unwrap();
    let text = fs::read_to_string(dir.file()).unwrap();
    assert!(text.contains("$argon2id$"));
    assert!(!text.contains("correct horse battery"));
}

#[test]
fn a_save_leaves_no_temporary_file_behind() {
    let dir = TmpDir::new();
    let m = AuthManager::open(Arc::new(FileStore::new(dir.file()))).unwrap();
    m.create_user(user("ann", "password1")).unwrap();
    let names: Vec<String> = fs::read_dir(&dir.0)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["security.json"]);
}

#[cfg(unix)]
#[test]
fn the_file_is_readable_by_its_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TmpDir::new();
    let m = AuthManager::open(Arc::new(FileStore::new(dir.file()))).unwrap();
    m.create_user(user("ann", "password1")).unwrap();
    let mode = fs::metadata(dir.file()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn a_missing_file_is_an_empty_catalog() {
    let dir = TmpDir::new();
    let m = AuthManager::open(Arc::new(FileStore::new(dir.file()))).unwrap();
    assert!(m.users().is_empty());
    assert!(!dir.file().exists(), "opening alone writes nothing");
}

#[test]
fn a_corrupt_file_refuses_to_open_instead_of_starting_empty() {
    // Starting empty would let the server create a fresh admin with a known
    // password over a damaged catalog.
    let dir = TmpDir::new();
    fs::write(dir.file(), "{ not json").unwrap();
    assert!(matches!(
        AuthManager::open(Arc::new(FileStore::new(dir.file()))),
        Err(ManageError::Store(StoreError::Corrupt { .. }))
    ));
}

#[test]
fn a_catalog_from_a_newer_format_refuses_to_open() {
    let dir = TmpDir::new();
    fs::write(dir.file(), r#"{"format_version": 999, "users": {}}"#).unwrap();
    assert!(matches!(
        AuthManager::open(Arc::new(FileStore::new(dir.file()))),
        Err(ManageError::Store(StoreError::UnsupportedVersion {
            found: 999,
            ..
        }))
    ));
}

#[test]
fn concurrent_changes_are_all_kept() {
    let dir = TmpDir::new();
    let m = Arc::new(AuthManager::open(Arc::new(FileStore::new(dir.file()))).unwrap());
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let m = Arc::clone(&m);
            std::thread::spawn(move || {
                m.create_user(user(&format!("user{i}"), "password1"))
                    .unwrap()
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let reopened = AuthManager::open(Arc::new(FileStore::new(dir.file()))).unwrap();
    assert_eq!(reopened.users().len(), 8);
}

// ── Bootstrap ────────────────────────────────────────────────────────

#[test]
fn the_bootstrap_spec_is_parsed_like_neo4j_auth() {
    assert_eq!(Bootstrap::parse(None), Ok(Bootstrap::Generated));
    assert_eq!(Bootstrap::parse(Some("none")), Ok(Bootstrap::Disabled));
    assert_eq!(
        Bootstrap::parse(Some("admin/s3cret-pass")),
        Ok(Bootstrap::Admin {
            user: "admin".into(),
            password: "s3cret-pass".into()
        })
    );
    // Only the first slash separates: a password may contain slashes.
    assert_eq!(
        Bootstrap::parse(Some("neo4j/a/b/c/d/e")),
        Ok(Bootstrap::Admin {
            user: "neo4j".into(),
            password: "a/b/c/d/e".into()
        })
    );
    for bad in ["", "neo4j", "/password", "neo4j/"] {
        assert!(
            Bootstrap::parse(Some(bad)).is_err(),
            "{bad:?} must be refused"
        );
    }
}

#[test]
fn bootstrap_parse_errors_do_not_echo_the_password() {
    let e = Bootstrap::parse(Some("bad name/s3cret-pass")).unwrap_err();
    assert!(!format!("{e} {e:?}").contains("s3cret-pass"));
}

#[test]
fn an_admin_bootstrap_creates_the_first_user_once() {
    let m = manager();
    let spec = Bootstrap::Admin {
        user: "admin".into(),
        password: "first-password".into(),
    };
    assert_eq!(m.bootstrap(&spec).unwrap(), BootstrapOutcome::Created);
    assert!(m.check_password("admin", "first-password"));
    assert!(
        !m.user("admin").unwrap().password_change_required,
        "an operator-chosen password needs no change"
    );

    // A second start with another password changes nothing.
    let again = Bootstrap::Admin {
        user: "admin".into(),
        password: "other-password".into(),
    };
    assert_eq!(
        m.bootstrap(&again).unwrap(),
        BootstrapOutcome::AlreadyInitialised
    );
    assert!(m.check_password("admin", "first-password"));
}

#[test]
fn a_weak_bootstrap_password_is_refused() {
    let m = manager();
    let spec = Bootstrap::Admin {
        user: "admin".into(),
        password: "short".into(),
    };
    assert!(matches!(
        m.bootstrap(&spec),
        Err(ManageError::WeakPassword { .. })
    ));
    assert!(m.users().is_empty());
}

#[test]
fn a_generated_bootstrap_makes_neo4j_with_a_random_password_to_change() {
    let m = manager();
    let BootstrapOutcome::Generated { user, password } =
        m.bootstrap(&Bootstrap::Generated).unwrap()
    else {
        panic!("expected a generated password");
    };
    assert_eq!(user, "neo4j");
    assert!(
        password.len() >= 20,
        "generated password too short: {}",
        password.len()
    );
    assert!(m.check_password("neo4j", &password));
    assert!(m.user("neo4j").unwrap().password_change_required);

    let other = manager();
    let BootstrapOutcome::Generated {
        password: second, ..
    } = other.bootstrap(&Bootstrap::Generated).unwrap()
    else {
        panic!("expected a generated password");
    };
    assert_ne!(password, second, "each server gets its own password");
    assert_eq!(
        m.bootstrap(&Bootstrap::Generated).unwrap(),
        BootstrapOutcome::AlreadyInitialised
    );
}

#[test]
fn a_disabled_bootstrap_creates_nobody() {
    let m = manager();
    assert_eq!(
        m.bootstrap(&Bootstrap::Disabled).unwrap(),
        BootstrapOutcome::Disabled
    );
    assert!(m.users().is_empty());
}

#[test]
fn bootstrap_on_a_reopened_store_sees_the_existing_users() {
    let dir = TmpDir::new();
    {
        let m = AuthManager::open(Arc::new(FileStore::new(dir.file()))).unwrap();
        m.bootstrap(&Bootstrap::Generated).unwrap();
    }
    let m = AuthManager::open(Arc::new(FileStore::new(dir.file()))).unwrap();
    assert_eq!(
        m.bootstrap(&Bootstrap::Generated).unwrap(),
        BootstrapOutcome::AlreadyInitialised
    );
}

#[test]
fn the_manager_is_shared_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AuthManager>();
    assert_send_sync::<FileStore>();
    assert_send_sync::<MemoryStore>();
}

#[test]
fn a_store_can_be_used_through_the_trait() {
    let store: Arc<dyn CatalogStore> = Arc::new(MemoryStore::new());
    assert!(store.load().unwrap().is_none());
    let m = AuthManager::open(store.clone()).unwrap();
    m.create_user(user("ann", "password1")).unwrap();
    assert_eq!(store.load().unwrap().unwrap().users.len(), 1);
}
