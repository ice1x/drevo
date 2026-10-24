//! Physical backups of a whole server: every database, its schema sidecar,
//! the users and the embeddings settings.
//!
//! - [`backup::backup_server`] writes `server.json` plus one engine backup
//!   per database; with a previous backup, each database that continues it
//!   is backed up incrementally.
//! - [`backup::restore_server`] rebuilds a data directory that a server opens
//!   to the backed-up state.
//! - `POST /admin/backup` takes one into the server's backup directory, and
//!   `drevo-server backup` / `restore` do it from the command line.
#![cfg(feature = "http")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::backup::{self, BackupKind, ServerBackup, SERVER_MANIFEST};
use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::database_registry::DatabaseRegistry;
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use http_body_util::BodyExt;
use tower::ServiceExt;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A scratch directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_server_backup_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A server's databases, opened from `data_dir` the way the server does.
fn open(data_dir: &Path) -> Arc<DatabaseRegistry> {
    std::fs::create_dir_all(data_dir).unwrap();
    let default = Arc::new(NativeService::open(data_dir.join("native.wal")).unwrap());
    Arc::new(DatabaseRegistry::with_durable_dir(default, data_dir).unwrap())
}

fn run(svc: &NativeService, q: &str) -> Vec<Vec<Value>> {
    svc.execute(&parse(q).unwrap(), HashMap::new())
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .rows
}

fn strings(rows: Vec<Vec<Value>>) -> Vec<String> {
    rows.into_iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

/// A bug tracker in the default database, an ERP in `erp`, users and
/// embeddings settings next to them.
fn populated(data_dir: &Path) -> Arc<DatabaseRegistry> {
    let registry = open(data_dir);
    let tracker = registry.default_service();
    run(
        &tracker,
        "CREATE (:Bug {key: 'BUG-1', title: 'Crash on save'})-[:FOUND_IN]->(:Release {name: '1.2'})",
    );
    run(
        &tracker,
        "CREATE CONSTRAINT bug_key FOR (b:Bug) REQUIRE b.key IS UNIQUE",
    );
    let erp = registry.create("erp").unwrap();
    run(&erp, "CREATE (:Invoice {no: 'INV-7', total: 120.5})");
    std::fs::write(
        data_dir.join("security.json"),
        br#"{"format":2,"users":[]}"#,
    )
    .unwrap();
    std::fs::write(
        data_dir.join("embeddings_config.json"),
        br#"{"model":"nomic-embed-text"}"#,
    )
    .unwrap();
    std::fs::write(
        data_dir.join("text2cypher_config.json"),
        br#"{"upstream":"http://ollama:11434/v1/chat/completions","model":"qwen"}"#,
    )
    .unwrap();
    registry
}

fn bug_keys(svc: &NativeService) -> Vec<String> {
    strings(run(svc, "MATCH (b:Bug) RETURN b.key ORDER BY b.key"))
}

fn constraint_names(svc: &NativeService) -> Vec<String> {
    strings(run(svc, "SHOW CONSTRAINTS YIELD name RETURN name"))
}

#[test]
fn a_server_backup_restores_every_database_and_the_server_files() {
    let tmp = TmpDir::new();
    let registry = populated(&tmp.path("data"));
    let summary =
        backup::backup_server(&registry, &tmp.path("data"), &tmp.path("b1"), None).unwrap();
    assert_eq!(
        summary
            .databases
            .iter()
            .map(|d| (d.name.as_str(), d.kind))
            .collect::<Vec<_>>(),
        [("drevo", BackupKind::Full), ("erp", BackupKind::Full)]
    );
    assert_eq!(
        summary
            .files
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        [
            "security.json",
            "embeddings_config.json",
            "text2cypher_config.json"
        ]
    );
    assert_eq!(ServerBackup::read(&tmp.path("b1")).unwrap(), summary);
    assert!(tmp.path("b1").join(SERVER_MANIFEST).exists());

    let report = backup::restore_server(&[tmp.path("b1")], &tmp.path("restored")).unwrap();
    assert_eq!(report.databases, ["drevo", "erp"]);
    drop(registry);
    let restored = open(&tmp.path("restored"));
    let tracker = restored.default_service();
    assert_eq!(bug_keys(&tracker), ["BUG-1"]);
    assert_eq!(
        strings(run(
            &tracker,
            "MATCH (:Bug)-[:FOUND_IN]->(r:Release) RETURN r.name"
        )),
        ["1.2"]
    );
    // The schema sidecar came back: the constraint holds.
    assert_eq!(constraint_names(&tracker), ["bug_key"]);
    assert!(tracker
        .execute(
            &parse("CREATE (:Bug {key: 'BUG-1'})").unwrap(),
            HashMap::new()
        )
        .is_err());
    let erp = restored.get("erp").expect("erp restored");
    assert_eq!(
        strings(run(&erp, "MATCH (i:Invoice) RETURN i.no")),
        ["INV-7"]
    );
    assert_eq!(
        std::fs::read(tmp.path("restored/security.json")).unwrap(),
        br#"{"format":2,"users":[]}"#
    );
    assert_eq!(
        std::fs::read(tmp.path("restored/embeddings_config.json")).unwrap(),
        br#"{"model":"nomic-embed-text"}"#
    );
    assert_eq!(
        std::fs::read(tmp.path("restored/text2cypher_config.json")).unwrap(),
        br#"{"upstream":"http://ollama:11434/v1/chat/completions","model":"qwen"}"#
    );
}

#[test]
fn a_later_backup_holds_only_what_changed_and_the_chain_restores() {
    let tmp = TmpDir::new();
    let registry = populated(&tmp.path("data"));
    backup::backup_server(&registry, &tmp.path("data"), &tmp.path("b1"), None).unwrap();

    run(&registry.default_service(), "CREATE (:Bug {key: 'BUG-2'})");
    // A database created after the first backup has nothing to continue.
    let crm = registry.create("crm").unwrap();
    run(&crm, "CREATE (:Lead {name: 'Acme'})");
    let b2 = backup::backup_server(
        &registry,
        &tmp.path("data"),
        &tmp.path("b2"),
        Some(&tmp.path("b1")),
    )
    .unwrap();
    let by_name: HashMap<&str, &backup::DatabaseBackup> =
        b2.databases.iter().map(|d| (d.name.as_str(), d)).collect();
    assert_eq!(by_name["drevo"].kind, BackupKind::Incremental);
    assert_eq!(by_name["erp"].kind, BackupKind::Incremental);
    assert_eq!(by_name["erp"].wal_bytes, 0, "nothing changed in erp");
    assert_eq!(by_name["crm"].kind, BackupKind::Full);
    assert!(by_name["crm"]
        .full_because
        .as_deref()
        .unwrap()
        .contains("no earlier backup"));

    backup::restore_server(&[tmp.path("b1"), tmp.path("b2")], &tmp.path("restored")).unwrap();
    drop(registry);
    let restored = open(&tmp.path("restored"));
    assert_eq!(bug_keys(&restored.default_service()), ["BUG-1", "BUG-2"]);
    let crm = restored.get("crm").expect("crm restored");
    assert_eq!(strings(run(&crm, "MATCH (l:Lead) RETURN l.name")), ["Acme"]);
}

#[test]
fn restoring_never_touches_an_existing_data_directory() {
    let tmp = TmpDir::new();
    let registry = populated(&tmp.path("data"));
    backup::backup_server(&registry, &tmp.path("data"), &tmp.path("b1"), None).unwrap();
    std::fs::create_dir_all(tmp.path("busy")).unwrap();
    std::fs::write(tmp.path("busy/notes.txt"), b"keep me").unwrap();
    let e = backup::restore_server(&[tmp.path("b1")], &tmp.path("busy")).unwrap_err();
    assert!(e.to_string().contains("not empty"), "{e}");
    assert_eq!(
        std::fs::read(tmp.path("busy/notes.txt")).unwrap(),
        b"keep me"
    );
}

#[test]
fn a_failed_restore_leaves_no_half_restored_server() {
    let tmp = TmpDir::new();
    let registry = populated(&tmp.path("data"));
    backup::backup_server(&registry, &tmp.path("data"), &tmp.path("b1"), None).unwrap();
    // Damage the second database's log.
    let wal = tmp.path("b1/databases/erp/native.wal");
    let mut bytes = std::fs::read(&wal).unwrap();
    let last = bytes.len() - 2;
    bytes[last] ^= 0x01;
    std::fs::write(&wal, &bytes).unwrap();
    let e = backup::restore_server(&[tmp.path("b1")], &tmp.path("restored")).unwrap_err();
    assert!(e.to_string().contains("damaged"), "{e}");
    assert!(!tmp.path("restored").exists(), "nothing is left behind");

    // A damaged server file is caught too.
    bytes[last] ^= 0x01;
    std::fs::write(&wal, &bytes).unwrap();
    std::fs::write(tmp.path("b1/databases/drevo/security.json"), b"{}").unwrap();
    let e = backup::restore_server(&[tmp.path("b1")], &tmp.path("restored")).unwrap_err();
    assert!(e.to_string().contains("security.json"), "{e}");
    assert!(!tmp.path("restored").exists());
}

#[test]
fn a_server_backup_never_lands_on_another() {
    let tmp = TmpDir::new();
    let registry = populated(&tmp.path("data"));
    backup::backup_server(&registry, &tmp.path("data"), &tmp.path("b1"), None).unwrap();
    let e = backup::backup_server(&registry, &tmp.path("data"), &tmp.path("b1"), None).unwrap_err();
    assert!(e.to_string().contains("already exists"), "{e}");
}

// ── POST /admin/backup ──────────────────────────────────────────────

async fn post(
    app: &axum::Router,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn app(registry: &Arc<DatabaseRegistry>, backups: &Path) -> axum::Router {
    build_native_router(
        NativeApiState::with_registry(registry.default_service(), Arc::clone(registry))
            .with_backup_dir(backups.to_path_buf()),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn post_admin_backup_takes_named_and_incremental_backups() {
    let tmp = TmpDir::new();
    let registry = populated(&tmp.path("data"));
    let app = app(&registry, &tmp.path("backups"));

    let (status, body) = post(&app, "/admin/backup", serde_json::json!({"name": "monday"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "monday");
    assert_eq!(body["databases"][0]["kind"], "full");
    assert!(tmp.path("backups/monday").join(SERVER_MANIFEST).exists());

    run(&registry.default_service(), "CREATE (:Bug {key: 'BUG-3'})");
    let (status, body) = post(
        &app,
        "/admin/backup",
        serde_json::json!({"name": "tuesday", "since": "monday"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["databases"][0]["kind"], "incremental");

    // Without a name, one is made from the time.
    let (status, body) = post(&app, "/admin/backup", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let made = body["name"].as_str().unwrap().to_string();
    assert!(made.starts_with("backup-"), "{made}");

    let (status, list) = get(&app, "/admin/backups").await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = list["backups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 3);
    assert!(
        names.contains(&"monday") && names.contains(&"tuesday") && names.contains(&made.as_str())
    );

    // The chain restores.
    backup::restore_server(
        &[tmp.path("backups/monday"), tmp.path("backups/tuesday")],
        &tmp.path("restored"),
    )
    .unwrap();
    drop(app);
    drop(registry);
    let restored = open(&tmp.path("restored"));
    assert_eq!(bug_keys(&restored.default_service()), ["BUG-1", "BUG-3"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn post_admin_backup_refuses_bad_requests() {
    let tmp = TmpDir::new();
    let registry = populated(&tmp.path("data"));
    let app = app(&registry, &tmp.path("backups"));
    for bad in ["../escape", "a/b", ".hidden", "", "x y"] {
        let (status, body) = post(&app, "/admin/backup", serde_json::json!({"name": bad})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }
    let (status, _) = post(&app, "/admin/backup", serde_json::json!({"name": "once"})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post(&app, "/admin/backup", serde_json::json!({"name": "once"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = post(
        &app,
        "/admin/backup",
        serde_json::json!({"name": "later", "since": "never-taken"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(!tmp.path("backups/later").exists());
}

#[tokio::test]
async fn an_in_memory_server_has_nothing_to_back_up() {
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));
    let (status, body) = post(&app, "/admin/backup", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap().contains("memory"), "{body}");
}

// ── drevo-server backup / restore ───────────────────────────────────

fn env(data_dir: &Path) -> impl Fn(&str) -> Option<String> {
    let data_dir = data_dir.display().to_string();
    move |key| (key == "DREVO_DATA_DIR").then(|| data_dir.clone())
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn the_command_line_backs_up_a_stopped_server_and_restores_it() {
    let tmp = TmpDir::new();
    drop(populated(&tmp.path("data")));
    let b1 = tmp.path("b1").display().to_string();
    let out = backup::command(&args(&["backup", "--to", &b1]), env(&tmp.path("data")))
        .expect("a backup command")
        .unwrap();
    assert!(out.contains("drevo") && out.contains("erp"), "{out}");

    let b2 = tmp.path("b2").display().to_string();
    let out = backup::command(
        &args(&["backup", "--to", &b2, "--since", &b1]),
        env(&tmp.path("data")),
    )
    .expect("a backup command")
    .unwrap();
    assert!(out.contains("incremental"), "{out}");

    let restored = tmp.path("restored").display().to_string();
    let out = backup::command(
        &args(&["restore", "--from", &b1, "--from", &b2, "--to", &restored]),
        |_| None,
    )
    .expect("a restore command")
    .unwrap();
    assert!(out.contains("restored"), "{out}");
    let restored = open(&tmp.path("restored"));
    assert_eq!(bug_keys(&restored.default_service()), ["BUG-1"]);
}

#[test]
fn the_command_line_explains_a_running_server_and_bad_arguments() {
    let tmp = TmpDir::new();
    let running = populated(&tmp.path("data"));
    let b1 = tmp.path("b1").display().to_string();
    let e = backup::command(&args(&["backup", "--to", &b1]), env(&tmp.path("data")))
        .expect("a backup command")
        .unwrap_err();
    assert!(e.contains("POST /admin/backup"), "{e}");
    drop(running);

    for bad in [
        args(&["backup"]),
        args(&["backup", "--to"]),
        args(&["backup", "--to", "x", "--frobnicate"]),
        args(&["restore", "--to", "x"]),
        args(&["restore", "--from", "x"]),
    ] {
        let e = backup::command(&bad, env(&tmp.path("data")))
            .expect("a backup command")
            .unwrap_err();
        assert!(e.contains("usage"), "{bad:?}: {e}");
    }
    // Anything else is not a backup command: the server starts as usual.
    assert!(backup::command(&args(&[]), |_| None).is_none());
    assert!(backup::command(&args(&["serve"]), |_| None).is_none());
}
