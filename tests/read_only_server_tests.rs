//! Read-only databases on a server.
//!
//! - `DREVO_READ_ONLY=1` opens every database read-only: reads, search and
//!   export work, every write and schema change fails, and nothing in the
//!   data directory is written, so it can be one the server cannot write to.
//! - `ALTER DATABASE <name> SET ACCESS READ ONLY | READ WRITE` switches one
//!   database and is remembered across restarts.
//! - `SHOW DATABASES` and `GET /databases` report each database's access.
#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::cypher::executor::{ExecError, Value};
use drevo::cypher::parser::parse;
use drevo::database_registry::{DatabaseRegistry, RegistryError};
use drevo::error::DrevoError;
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::server::Config;
use http_body_util::BodyExt;
use tower::ServiceExt;

const READ_ONLY: &str = "Neo.ClientError.General.ForbiddenOnReadOnlyDatabase";

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A scratch directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_read_only_server_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        #[cfg(unix)]
        set_modes(&self.0, 0o755, 0o644);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The databases of `data_dir`, opened the way the server does.
fn open(data_dir: &Path) -> Arc<DatabaseRegistry> {
    let default = Arc::new(NativeService::open(data_dir.join("native.wal")).unwrap());
    Arc::new(DatabaseRegistry::with_durable_dir(default, data_dir).unwrap())
}

/// The databases of `data_dir`, opened as `DREVO_READ_ONLY=1` does.
fn open_read_only(data_dir: &Path) -> Arc<DatabaseRegistry> {
    let default = Arc::new(NativeService::open_read_only(data_dir.join("native.wal")).unwrap());
    Arc::new(DatabaseRegistry::with_durable_dir_read_only(default, data_dir).unwrap())
}

fn run(svc: &NativeService, q: &str) -> Result<Vec<Vec<Value>>, ExecError> {
    svc.execute(&parse(q).unwrap(), HashMap::new())
        .map(|r| r.rows)
}

fn titles(svc: &NativeService) -> Vec<String> {
    run(svc, "MATCH (n) RETURN n.title ORDER BY n.title")
        .unwrap()
        .into_iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

fn assert_read_only<T: std::fmt::Debug>(result: Result<T, ExecError>) {
    match result {
        Err(ExecError::Storage(DrevoError::ReadOnly)) => {}
        other => panic!("expected a read-only error, got {other:?}"),
    }
}

/// A bug tracker in the default database and an ERP in `erp`, closed.
fn populated(data_dir: &Path) {
    let registry = open(data_dir);
    run(
        &registry.default_service(),
        "CREATE (:Bug {title: 'BUG-1 login fails'})",
    )
    .unwrap();
    run(
        &registry.default_service(),
        "CREATE VECTOR INDEX bug_vec FOR (b:Bug) ON (b.embedding)",
    )
    .unwrap();
    let erp = registry.create("erp").unwrap();
    run(&erp, "CREATE (:Invoice {title: 'INV-7'})").unwrap();
}

/// Every file under `dir` with its bytes.
fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.insert(e.path(), std::fs::read(e.path()).unwrap());
            }
        }
    }
    out
}

#[cfg(unix)]
fn set_modes(dir: &Path, dir_mode: u32, file_mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
    for e in entries.flatten() {
        if e.path().is_dir() {
            set_modes(&e.path(), dir_mode, file_mode);
        } else {
            let _ = std::fs::set_permissions(e.path(), std::fs::Permissions::from_mode(file_mode));
        }
    }
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(dir_mode));
}

// ── DREVO_READ_ONLY ─────────────────────────────────────────────────────

#[test]
fn drevo_read_only_is_a_boolean_setting() {
    let cfg = |v: &'static str| {
        Config::from_env(move |k: &str| (k == "DREVO_READ_ONLY").then(|| v.to_string()))
    };
    assert!(!Config::from_env(|_: &str| None).unwrap().read_only);
    for on in ["1", "true", "yes", "on", "TRUE"] {
        assert!(cfg(on).unwrap().read_only, "{on}");
    }
    for off in ["0", "false", "no", "off", ""] {
        assert!(!cfg(off).unwrap().read_only, "{off}");
    }
    let err = cfg("maybe").unwrap_err().to_string();
    assert!(err.contains("DREVO_READ_ONLY"), "{err}");
}

#[test]
fn a_read_only_server_reads_every_database() {
    let tmp = TmpDir::new();
    populated(&tmp.0);
    let registry = open_read_only(&tmp.0);
    assert_eq!(titles(&registry.default_service()), ["BUG-1 login fails"]);
    assert_eq!(titles(&registry.get("erp").unwrap()), ["INV-7"]);
    assert!(registry.is_read_only("drevo") && registry.is_read_only("erp"));
}

#[test]
fn a_read_only_server_refuses_writes_and_schema_changes() {
    let tmp = TmpDir::new();
    populated(&tmp.0);
    let registry = open_read_only(&tmp.0);
    let tracker = registry.default_service();
    assert_read_only(run(&tracker, "CREATE (:Bug {title: 'BUG-2'})"));
    assert_read_only(run(&tracker, "MATCH (b:Bug) SET b.status = 'done'"));
    assert_read_only(run(&tracker, "MATCH (b:Bug) DETACH DELETE b"));
    assert_read_only(run(
        &tracker,
        "CREATE CONSTRAINT bug_title FOR (b:Bug) REQUIRE b.title IS UNIQUE",
    ));
    assert_read_only(run(&tracker, "DROP INDEX bug_vec"));
    assert_read_only(run(
        &tracker,
        "CALL drevo.semantic.register('Bug', 'title', 'embedding', 'manual')",
    ));
    assert_read_only(run(&registry.get("erp").unwrap(), "CREATE (:Invoice)"));
    assert_eq!(titles(&tracker), ["BUG-1 login fails"]);
}

#[test]
fn a_read_only_server_cannot_create_or_drop_databases() {
    let tmp = TmpDir::new();
    populated(&tmp.0);
    let registry = open_read_only(&tmp.0);
    assert!(matches!(
        registry.create("crm"),
        Err(RegistryError::ReadOnly)
    ));
    assert!(matches!(
        registry.remove("erp"),
        Err(RegistryError::ReadOnly)
    ));
    assert!(matches!(
        registry.set_access("erp", false),
        Err(RegistryError::ReadOnly)
    ));
    assert_eq!(registry.list(), ["drevo", "erp"]);
}

#[test]
fn a_read_only_server_writes_nothing_in_its_data_directory() {
    let tmp = TmpDir::new();
    populated(&tmp.0);
    let before = files(&tmp.0);
    {
        let registry = open_read_only(&tmp.0);
        let _ = run(&registry.default_service(), "CREATE (:Bug)");
        let _ = run(&registry.default_service(), "MATCH (n) RETURN count(n)");
    }
    assert_eq!(files(&tmp.0), before);
}

#[cfg(unix)]
#[test]
fn a_read_only_server_opens_a_data_directory_it_cannot_write() {
    let tmp = TmpDir::new();
    populated(&tmp.0);
    set_modes(&tmp.0, 0o555, 0o444);
    if std::fs::write(tmp.0.join("probe"), b"x").is_ok() {
        return; // running as root: file modes are not enforced
    }
    let registry = open_read_only(&tmp.0);
    assert_eq!(titles(&registry.get("erp").unwrap()), ["INV-7"]);
}

// ── ALTER DATABASE … SET ACCESS ─────────────────────────────────────────

#[test]
fn one_database_can_be_made_read_only_and_back() {
    let tmp = TmpDir::new();
    populated(&tmp.0);
    let registry = open(&tmp.0);
    registry.set_access("erp", true).unwrap();
    assert!(registry.is_read_only("erp"));
    assert!(!registry.is_read_only("drevo"));
    let erp = registry.get("erp").unwrap();
    assert_read_only(run(&erp, "CREATE (:Invoice {title: 'INV-8'})"));
    run(
        &registry.default_service(),
        "CREATE (:Bug {title: 'BUG-2'})",
    )
    .unwrap();

    registry.set_access("erp", false).unwrap();
    run(&erp, "CREATE (:Invoice {title: 'INV-8'})").unwrap();
    assert_eq!(titles(&erp), ["INV-7", "INV-8"]);
}

#[test]
fn database_access_survives_a_restart() {
    let tmp = TmpDir::new();
    populated(&tmp.0);
    {
        let registry = open(&tmp.0);
        registry.set_access("erp", true).unwrap();
        registry.set_access("drevo", true).unwrap();
        registry.set_access("drevo", false).unwrap();
    }
    let registry = open(&tmp.0);
    assert!(registry.is_read_only("erp"));
    assert!(!registry.is_read_only("drevo"));
    assert_read_only(run(&registry.get("erp").unwrap(), "CREATE (:Invoice)"));
}

#[test]
fn a_dropped_database_forgets_its_access() {
    let tmp = TmpDir::new();
    populated(&tmp.0);
    let registry = open(&tmp.0);
    registry.set_access("erp", true).unwrap();
    registry.remove("erp").unwrap();
    registry.create("erp").unwrap();
    assert!(!registry.is_read_only("erp"));
}

#[test]
fn access_of_an_unknown_database_is_not_found() {
    let tmp = TmpDir::new();
    populated(&tmp.0);
    let registry = open(&tmp.0);
    assert!(matches!(
        registry.set_access("nope", true),
        Err(RegistryError::NotFound(_))
    ));
}

// ── HTTP: ALTER, SHOW DATABASES, GET /databases ─────────────────────────

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .expect("request");
    let response = app.clone().oneshot(req).await.expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    // An export answers XML: kept as a string.
    let json = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
    });
    (status, json)
}

fn app(registry: Arc<DatabaseRegistry>) -> axum::Router {
    build_native_router(NativeApiState::with_registry(
        registry.default_service(),
        registry,
    ))
}

fn cypher(q: &str) -> String {
    serde_json::json!({ "query": q }).to_string()
}

#[tokio::test]
async fn alter_database_over_http_switches_access_and_show_databases_reports_it() {
    let tmp = TmpDir::new();
    populated(&tmp.0);
    let app = app(open(&tmp.0));

    let (status, body) = call(
        &app,
        "POST",
        "/cypher",
        Some(&cypher("ALTER DATABASE erp SET ACCESS READ ONLY")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, body) = call(&app, "POST", "/cypher", Some(&cypher("SHOW DATABASES"))).await;
    assert_eq!(body["columns"], serde_json::json!(["name", "access"]));
    assert_eq!(
        body["rows"],
        serde_json::json!([["drevo", "read-write"], ["erp", "read-only"]])
    );

    let (_, body) = call(&app, "GET", "/databases", None).await;
    assert_eq!(body["databases"], serde_json::json!(["drevo", "erp"]));
    assert_eq!(
        body["access"],
        serde_json::json!({"drevo": "read-write", "erp": "read-only"})
    );

    let (status, body) = call(
        &app,
        "POST",
        "/cypher",
        Some(&cypher("USE erp CREATE (:Invoice {title: 'INV-9'})")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], READ_ONLY);

    let (status, _) = call(
        &app,
        "POST",
        "/cypher",
        Some(&cypher("ALTER DATABASE erp SET ACCESS READ WRITE")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        &app,
        "POST",
        "/cypher",
        Some(&cypher("USE erp CREATE (:Invoice {title: 'INV-9'})")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn alter_database_of_an_unknown_database_is_404() {
    let tmp = TmpDir::new();
    populated(&tmp.0);
    let app = app(open(&tmp.0));
    let (status, _) = call(
        &app,
        "POST",
        "/cypher",
        Some(&cypher("ALTER DATABASE nope SET ACCESS READ ONLY")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_read_only_server_answers_catalog_changes_with_403() {
    let tmp = TmpDir::new();
    populated(&tmp.0);
    let app = app(open_read_only(&tmp.0));
    for q in [
        "CREATE DATABASE crm",
        "ALTER DATABASE erp SET ACCESS READ WRITE",
    ] {
        let (status, body) = call(&app, "POST", "/cypher", Some(&cypher(q))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{q}: {body}");
        assert_eq!(body["code"], READ_ONLY, "{q}");
    }
    let (status, body) = call(&app, "POST", "/databases", Some(r#"{"name": "crm"}"#)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = call(&app, "DELETE", "/databases/erp", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = call(&app, "POST", "/storage/shrink", Some("{}")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = call(&app, "GET", "/export/graphml", None).await;
    assert_eq!(status, StatusCode::OK, "export works: {body}");
}
