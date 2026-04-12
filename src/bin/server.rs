//! GraphNote DB standalone HTTP server binary.
//!
//! Task 00045: entry point for the containerised deployment. Reads
//! configuration from environment variables and starts the axum HTTP
//! server with graceful shutdown on SIGTERM / SIGINT.
//!
//! ## Environment variables
//!
//! | Variable             | Default    | Description                    |
//! |----------------------|------------|--------------------------------|
//! | `GRAPHNOTE_PORT`     | `8080`     | TCP port to listen on          |
//! | `GRAPHNOTE_DATA_DIR` | `/data`    | Path to the redb database file |
//! | `GRAPHNOTE_HOST`     | `0.0.0.0`  | Bind address                   |

use std::net::SocketAddr;
use std::sync::Arc;

use graphnote_db::api::{build_router, ApiState};
use graphnote_db::db::GraphNoteDb;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() {
    let host = env_or("GRAPHNOTE_HOST", "0.0.0.0");
    let port: u16 = env_or("GRAPHNOTE_PORT", "8080")
        .parse()
        .expect("GRAPHNOTE_PORT must be a valid port number");
    let data_dir = env_or("GRAPHNOTE_DATA_DIR", "/data");

    let db_path = std::path::Path::new(&data_dir).join("graphnote.redb");

    eprintln!("graphnote-db: opening database at {}", db_path.display());
    let db = GraphNoteDb::open(&db_path).expect("failed to open database");
    let state = ApiState::new(Arc::new(db));
    let router = build_router(state);

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .expect("invalid GRAPHNOTE_HOST:GRAPHNOTE_PORT combination");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind TCP listener");

    eprintln!("graphnote-db: listening on {addr}");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");

    eprintln!("graphnote-db: shut down cleanly");
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
