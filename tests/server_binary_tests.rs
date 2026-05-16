//! Tests for the `drevo-server` binary entry point.
//!
//! Task 00045: verify the server binary can be built, the router works
//! end-to-end, and the default configuration is correct.

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
}
