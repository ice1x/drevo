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
//!
//! ## Backups
//!
//! `drevo-server backup --to DIR [--since DIR]` backs up a stopped server's
//! data directory, and `drevo-server restore --from DIR [--from DIR …] --to
//! DATA_DIR` restores one; see [`drevo::backup`].

use std::process::ExitCode;

use drevo::server::{run, Config};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    // `drevo-server backup …` / `drevo-server restore …` run and exit.
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(feature = "encryption")]
    let encryption = drevo::encryption_cmd::command(&args, |key| std::env::var(key).ok());
    #[cfg(not(feature = "encryption"))]
    let encryption: Option<Result<String, String>> = None;
    if let Some(result) =
        encryption.or_else(|| drevo::backup::command(&args, |key| std::env::var(key).ok()))
    {
        return match result {
            Ok(out) => {
                println!("{out}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("{err}");
                ExitCode::FAILURE
            }
        };
    }
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

/// Log to stdout under `RUST_LOG` (default `info`) and, independently of that
/// filter, copy every WARN/ERROR event into the Web UI's problem feed (#552).
fn init_tracing() {
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(filter))
        .with(drevo::problems::ProblemLayer::global().with_filter(LevelFilter::WARN))
        .init();
}
