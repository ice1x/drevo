//! drevo standalone HTTP server binary.
//!
//! Task 00045 introduced this entry point for the containerised
//! deployment. Task 00048 made graceful shutdown cooperate with
//! Kubernetes-style rolling restarts: as soon as SIGTERM or Ctrl+C
//! is observed the shared [`ApiState`] is flipped to "shutting down"
//! so that `/health` and `/ready` return 503 and the orchestrator
//! drains traffic before SIGKILL lands. axum's
//! [`with_graceful_shutdown`] then completes in-flight requests
//! before the process exits.
//!
//! ## Environment variables
//!
//! | Variable             | Default    | Description                    |
//! |----------------------|------------|--------------------------------|
//! | `DREVO_PORT`     | `8080`     | TCP port to listen on          |
//! | `DREVO_DATA_DIR` | `/data`    | Path to the redb database file |
//! | `DREVO_HOST`     | `0.0.0.0`  | Bind address                   |

use std::net::SocketAddr;
use std::sync::Arc;

use drevo::api::{build_router, ApiState};
use drevo::db::Drevo;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() {
    let host = env_or("DREVO_HOST", "0.0.0.0");
    let port: u16 = env_or("DREVO_PORT", "8080")
        .parse()
        .expect("DREVO_PORT must be a valid port number");
    let data_dir = env_or("DREVO_DATA_DIR", "/data");

    let db_path = std::path::Path::new(&data_dir).join("drevo.redb");

    eprintln!("drevo: opening database at {}", db_path.display());
    let db = Drevo::open(&db_path).expect("failed to open database");
    let state = ApiState::new(Arc::new(db));
    let shutdown_state = state.clone();
    let router = build_router(state);

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .expect("invalid DREVO_HOST:DREVO_PORT combination");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind TCP listener");

    eprintln!("drevo: listening on {addr}");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            // Flip /health and /ready to 503 *before* axum begins
            // draining in-flight requests so that load balancers stop
            // forwarding new traffic during the drain window.
            shutdown_state.signal_shutdown();
            eprintln!("drevo: shutdown signal received, draining…");
        })
        .await
        .expect("server error");

    eprintln!("drevo: shut down cleanly");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
