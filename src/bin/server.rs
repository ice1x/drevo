//! drevo standalone HTTP server binary.
//!
//! Task 00045 introduced this entry point for the containerised
//! deployment. Task 00048 made graceful shutdown cooperate with
//! Kubernetes-style rolling restarts. Task 00112 (Phase 8.5 audit)
//! moved every line of behaviour into [`drevo::server`] so the
//! env-var parser and the bind/serve/shutdown loop are unit-tested;
//! this file is now a thin shim that initialises `tracing` and
//! translates failures into a process exit code.
//!
//! ## Environment variables
//!
//! | Variable          | Default     | Description                       |
//! |-------------------|-------------|-----------------------------------|
//! | `DREVO_HOST`      | `0.0.0.0`   | Bind address                      |
//! | `DREVO_PORT`      | `8080`      | TCP port to listen on             |
//! | `DREVO_DATA_DIR`  | `/data`     | Path to the redb database file    |
//! | `RUST_LOG`        | `info`      | `tracing` filter (env-filter)     |
//!
//! See [`drevo::server`] for the documented signal-handling caveats
//! (Unix vs Windows).

use std::process::ExitCode;

use drevo::server::{run, Config};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    let cfg = match Config::from_env(|key| std::env::var(key).ok()) {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::error!(error = %err, "invalid server configuration");
            return ExitCode::from(2);
        }
    };

    match run(cfg).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "server exited with error");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
