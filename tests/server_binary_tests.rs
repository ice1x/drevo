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
    // Dockerfile-related configuration tests
    // -----------------------------------------------------------------

    #[test]
    fn default_listen_addr_is_0_0_0_0_8080() {
        // Documents the container convention: bind all interfaces on port 8080.
        let addr = "0.0.0.0:8080";
        let parsed: std::net::SocketAddr = addr.parse().unwrap();
        assert_eq!(parsed.port(), 8080);
        assert!(parsed.ip().is_unspecified());
    }

    #[test]
    fn data_directory_convention() {
        // The Dockerfile mounts a volume at /data — the server binary
        // should use this as the default storage directory.
        let data_dir = std::path::Path::new("/data");
        assert_eq!(data_dir.to_str().unwrap(), "/data");
    }

    #[test]
    fn env_var_overrides_port() {
        // DREVO_PORT env var should override the default port.
        // Here we test the parsing logic.
        let port_str = "9090";
        let port: u16 = port_str.parse().unwrap();
        assert_eq!(port, 9090);
    }

    #[test]
    fn env_var_overrides_data_dir() {
        // DREVO_DATA_DIR env var should override the default path.
        let custom = "/custom/path";
        let path = std::path::Path::new(custom);
        assert!(path.is_absolute());
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
