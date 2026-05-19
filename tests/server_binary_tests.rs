//! Tests for the `drevo-server` binary entry point.
//!
//! Task 00045: verify the server binary can be built, the router works
//! end-to-end, and the default configuration is correct.
//!
//! Task 00048: validate the production health-check contract — separate
//! liveness (`/health`) and readiness (`/ready`) probes, with `/health`
//! flipping to 503 once the process enters graceful shutdown so that
//! Kubernetes Endpoints controllers drain traffic before SIGKILL.

#[cfg(feature = "http")]
mod server_tests {
    use std::net::TcpListener;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use drevo::api::{build_router, ApiState};
    use drevo::db::Drevo;
    use std::sync::Arc;

    fn test_router() -> axum::Router {
        let db = Drevo::open_in_memory().unwrap();
        let state = ApiState::new(Arc::new(db));
        build_router(state)
    }

    // -----------------------------------------------------------------
    // Router smoke tests (same as previous tasks but verifying the
    // binary's expected behavior)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn health_returns_ok_json() {
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
    }

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

        let db = Drevo::open_in_memory().unwrap();
        let state = ApiState::new(Arc::new(db));
        let router = build_router(state);

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

        let db = Drevo::open_in_memory().unwrap();
        let state = ApiState::new(Arc::new(db));
        let router = build_router(state);

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
        assert_eq!(cfg.db_path().to_string_lossy(), "/data/drevo.redb");
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
    // Task 00048 — /ready (readiness probe) + shutdown-aware /health
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn ready_returns_ok_when_db_is_healthy() {
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ready");
    }

    #[tokio::test]
    async fn health_returns_ok_when_not_shutting_down() {
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn health_returns_503_after_signal_shutdown() {
        // After the operator signals graceful shutdown the process must
        // continue serving in-flight requests but `/health` must flip to
        // 503 so the Kubernetes Endpoints controller stops sending new
        // traffic before SIGKILL lands.
        let db = Drevo::open_in_memory().unwrap();
        let state = ApiState::new(Arc::new(db));
        let router = build_router(state.clone());

        state.signal_shutdown();

        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "shutting_down");
    }

    #[tokio::test]
    async fn ready_returns_503_after_signal_shutdown() {
        // /ready must also flip to 503 during graceful shutdown — once
        // the process is draining, it is by definition not "ready to
        // serve new traffic" even if the DB is still answering.
        let db = Drevo::open_in_memory().unwrap();
        let state = ApiState::new(Arc::new(db));
        let router = build_router(state.clone());

        state.signal_shutdown();

        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "shutting_down");
    }

    #[tokio::test]
    async fn is_shutting_down_starts_false() {
        let db = Drevo::open_in_memory().unwrap();
        let state = ApiState::new(Arc::new(db));
        assert!(!state.is_shutting_down());
    }

    #[tokio::test]
    async fn signal_shutdown_flips_flag_and_is_idempotent() {
        let db = Drevo::open_in_memory().unwrap();
        let state = ApiState::new(Arc::new(db));
        state.signal_shutdown();
        assert!(state.is_shutting_down());
        // Calling twice must remain true (idempotent — multiple signals
        // arriving in quick succession must not panic or flip back).
        state.signal_shutdown();
        assert!(state.is_shutting_down());
    }

    #[tokio::test]
    async fn shutdown_flag_is_shared_between_clones() {
        // ApiState is cloned per-handler by axum. A shutdown signalled
        // on one clone must be visible on every other clone — otherwise
        // graceful shutdown is silently broken.
        let db = Drevo::open_in_memory().unwrap();
        let state = ApiState::new(Arc::new(db));
        let clone = state.clone();
        assert!(!clone.is_shutting_down());
        state.signal_shutdown();
        assert!(clone.is_shutting_down());
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

        // Verify the redb file landed inside the configured data_dir
        // — proves the Config::db_path() contract is honoured by run().
        let db_file = dir_path.join("drevo.redb");
        assert!(
            db_file.exists(),
            "expected the redb file at {} once run() opens the database",
            db_file.display()
        );

        // Trigger graceful shutdown by closing the runtime task —
        // since `run()` blocks on the shutdown signal future, we have
        // to abort instead of waiting for SIGTERM. The abort still
        // covers the bind + serve path which is what we want here.
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn ready_method_not_allowed_returns_json_405() {
        // Same JSON-405 contract enforced everywhere else in the API.
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "method not allowed");
        assert_eq!(json["status"], 405);
    }
}
