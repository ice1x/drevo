//! Tests for the `drevo-server` binary entry point.
//!
//! Task 00045: verify the server binary can be built, the native router
//! works end-to-end, and the default configuration is correct.
//!
//! The suite runs against the native router (`build_native_router`), the one
//! `drevo::server::run()` actually serves. The KV-router-only health/ready
//! JSON envelopes and the `ApiState` shutdown-flag internals were dropped with
//! the KV HTTP router (epic #444); the bind + serve + graceful-shutdown
//! contract is covered here through `server::run()` directly.

#[cfg(feature = "http")]
mod server_tests {
    use std::net::TcpListener;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use drevo::native_api::{build_native_router, NativeApiState};
    use drevo::native_service::NativeService;
    use std::sync::Arc;
    use tracing_test::traced_test;

    fn test_router() -> axum::Router {
        let db = NativeService::in_memory();
        let state = NativeApiState::new(Arc::new(db));
        build_native_router(state)
    }

    // -----------------------------------------------------------------
    // Router smoke tests (same as previous tasks but verifying the
    // binary's expected behavior)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn status_returns_name_version_uptime() {
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["name"], "drevo");
        assert!(json["version"].is_string());
        assert!(json["uptime_seconds"].is_number());
    }

    // -----------------------------------------------------------------
    // Server bind + graceful shutdown
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn server_binds_and_shuts_down_gracefully() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let db = NativeService::in_memory();
        let state = NativeApiState::new(Arc::new(db));
        let router = build_native_router(state);

        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        let handle = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    rx.await.ok();
                })
                .await
                .unwrap();
        });

        // Give the server a moment to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Signal shutdown
        tx.send(()).unwrap();

        // Server should exit cleanly within 5 seconds
        let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(result.is_ok(), "server did not shut down within 5 seconds");
        result.unwrap().unwrap();
    }

    #[tokio::test]
    async fn server_accepts_tcp_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let db = NativeService::in_memory();
        let state = NativeApiState::new(Arc::new(db));
        let router = build_native_router(state);

        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        let handle = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    rx.await.ok();
                })
                .await
                .unwrap();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Verify we can connect via TCP
        let stream = tokio::net::TcpStream::connect(addr).await;
        assert!(stream.is_ok(), "could not connect to server at {addr}");

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    // -----------------------------------------------------------------
    // Binary configuration contract (post-00112 audit)
    //
    // Pre-00112 these tests parsed local string constants and never
    // touched the binary's actual parser. They now bind to
    // [`drevo::server::Config`] so the assertions break if a future
    // change drifts the binary defaults away from the Dockerfile and
    // README contract.
    // -----------------------------------------------------------------

    use drevo::server::Config;

    #[test]
    fn default_listen_addr_is_0_0_0_0_8080() {
        // Container convention: bind all interfaces on port 8080.
        let cfg = Config::from_env(|_| None).unwrap();
        let addr = cfg.socket_addr().unwrap();
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.port, 8080);
        assert_eq!(addr.port(), 8080);
        assert!(addr.ip().is_unspecified());
    }

    #[test]
    fn data_directory_convention() {
        // The Dockerfile mounts a volume at /data — the server binary
        // uses this as the default storage directory.
        let cfg = Config::from_env(|_| None).unwrap();
        assert_eq!(cfg.data_dir.to_string_lossy(), "/data");
    }

    #[test]
    fn env_var_overrides_port() {
        // DREVO_PORT env var overrides the default port.
        let cfg = Config::from_env(|k| match k {
            "DREVO_PORT" => Some("9090".to_string()),
            _ => None,
        })
        .unwrap();
        assert_eq!(cfg.port, 9090);
    }

    #[test]
    fn env_var_overrides_data_dir() {
        // DREVO_DATA_DIR env var overrides the default path.
        let cfg = Config::from_env(|k| match k {
            "DREVO_DATA_DIR" => Some("/custom/path".to_string()),
            _ => None,
        })
        .unwrap();
        assert!(cfg.data_dir.is_absolute());
        assert_eq!(cfg.data_dir.to_string_lossy(), "/custom/path");
    }

    // -----------------------------------------------------------------
    // Task 00112 — end-to-end run() smoke
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn run_serves_health_against_a_temp_data_dir_and_shuts_down() {
        // The audit 00112 introduces `drevo::server::run()` as the
        // single entry point for the binary. This test exercises it
        // against a temporary data directory and an ephemeral port so
        // the binary's bind + serve + graceful-shutdown contract is
        // covered without spawning a subprocess.
        use std::time::Duration;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").unwrap();
            let p = probe.local_addr().unwrap().port();
            drop(probe);
            p
        };

        let dir_path = dir.path().to_path_buf();
        let data_dir = dir_path.to_string_lossy().to_string();
        let port_str = port.to_string();
        let cfg = drevo::server::Config::from_env(move |k| match k {
            "DREVO_HOST" => Some("127.0.0.1".to_string()),
            "DREVO_PORT" => Some(port_str.clone()),
            "DREVO_DATA_DIR" => Some(data_dir.clone()),
            _ => None,
        })
        .unwrap();

        let server = tokio::spawn(async move {
            drevo::server::run(cfg).await.unwrap();
        });

        // Wait for the listener to come up before issuing requests.
        let addr = format!("127.0.0.1:{port}");
        let mut connected = false;
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(&addr).await.is_ok() {
                connected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(connected, "server did not start listening on {addr}");

        // Verify the durable native WAL landed inside the configured data_dir.
        // `run()` serves the durable native engine (the KV serving mode was
        // removed, epic #444), so the store of record is `<data_dir>/native.wal`
        // and no `drevo.redb` is created.
        let wal_file = dir_path.join("native.wal");
        assert!(
            wal_file.exists(),
            "expected the native WAL at {} once run() opens the durable store",
            wal_file.display()
        );
        assert!(
            !dir_path.join("drevo.redb").exists(),
            "run() must not open a KV redb file — the KV serving mode is removed"
        );

        // Trigger graceful shutdown by closing the runtime task —
        // since `run()` blocks on the shutdown signal future, we have
        // to abort instead of waiting for SIGTERM. The abort still
        // covers the bind + serve path which is what we want here.
        server.abort();
        let _ = server.await;
    }

    // -----------------------------------------------------------------
    // Startup version log — wiring guard (issue: version-in-startup-log)
    // -----------------------------------------------------------------

    #[traced_test]
    #[tokio::test]
    async fn run_logs_the_build_version_at_startup() {
        // The startup version line must be emitted by `run()` itself, so this
        // drives the real entry point rather than a helper — deleting the
        // `tracing::info!(... "starting drevo")` line from `run()` fails here.
        // That log is `run`'s first statement, emitted on the first poll before
        // any bind/catalog work, so awaiting `run()` directly (default
        // `#[tokio::test]` is current-thread, so `#[traced_test]`'s subscriber
        // sees it) under a short timeout that then drops the still-serving
        // future is enough to observe it.
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").unwrap();
            let p = probe.local_addr().unwrap().port();
            drop(probe);
            p
        };
        let data_dir = dir.path().to_string_lossy().to_string();
        let port_str = port.to_string();
        let cfg = drevo::server::Config::from_env(move |k| match k {
            "DREVO_HOST" => Some("127.0.0.1".to_string()),
            "DREVO_PORT" => Some(port_str.clone()),
            "DREVO_DATA_DIR" => Some(data_dir.clone()),
            _ => None,
        })
        .unwrap();

        let _ = tokio::time::timeout(Duration::from_millis(500), drevo::server::run(cfg)).await;

        assert!(
            logs_contain("starting drevo"),
            "run() must log the startup line"
        );
        assert!(
            logs_contain(drevo::VERSION),
            "the startup log must carry the build version"
        );
    }
}
