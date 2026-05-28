//! `drevo-mcp` — MCP (Model Context Protocol) stdio server binary
//! for Cline / Claude Code / Claude Desktop.
//!
//! Phase 15 task `00090`. Thin entry-point that:
//!
//! 1. parses the data directory path from `DREVO_DATA_DIR` (env)
//!    or `--data-dir <path>` (CLI), defaulting to `./drevo-data`;
//! 2. opens an embedded [`drevo::db::Drevo`] handle against that
//!    path (creating it if missing);
//! 3. wires it into a [`drevo::mcp::Server`] with the default
//!    seven-tool registry; and
//! 4. runs the sync stdio dispatch loop until stdin EOF.
//!
//! Logging goes to **stderr** (never stdout — stdout is the MCP
//! wire protocol). The default filter is `info`; override with
//! `RUST_LOG`.
//!
//! ## Embedded mode, no Docker
//!
//! Unlike `drevo-server` (the HTTP binary), this process is meant
//! to be **spawned by the MCP client itself** as a subprocess.
//! There is no listen port, no daemonisation, no signal handling
//! beyond the implicit "stdin closed → exit cleanly" lifecycle.
//! The MCP client's process tree owns the binary's lifetime.

use std::path::PathBuf;
use std::process::ExitCode;

use drevo::db::Drevo;
use drevo::mcp::Server;

fn main() -> ExitCode {
    init_tracing();

    let data_dir = match resolve_data_dir() {
        Ok(p) => p,
        Err(err) => {
            eprintln!("drevo-mcp: invalid data dir: {err}");
            return ExitCode::from(2);
        }
    };

    let drevo = match Drevo::open(&data_dir) {
        Ok(d) => d,
        Err(err) => {
            eprintln!(
                "drevo-mcp: failed to open Drevo at {}: {err}",
                data_dir.display()
            );
            return ExitCode::from(3);
        }
    };

    let server = Server::new(drevo);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    // We deliberately take the LOCKS up-front: re-acquiring stdin /
    // stdout locks line-by-line would serialise into the global
    // stream locks for every read/write. Keeping the locks held for
    // the lifetime of the server is fine — we're the only thing
    // reading stdin or writing stdout in this process.
    let reader = stdin.lock();
    let writer = stdout.lock();

    match server.run(reader, writer) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("drevo-mcp: I/O error in stdio loop: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Resolve the data dir from `DREVO_DATA_DIR` (env) or the first
/// `--data-dir <path>` CLI argument, defaulting to `./drevo-data`
/// in the current working directory.
///
/// We hand-roll the arg scan rather than pulling `clap` for one
/// flag — keeps the binary's dep tree identical to the pre-`00090`
/// surface.
fn resolve_data_dir() -> Result<PathBuf, String> {
    let args: Vec<String> = std::env::args().collect();
    let mut iter = args.iter().skip(1);
    // The match below handles `--help` / `--version` via
    // `std::process::exit`, so this loop currently always returns
    // on the first arg it sees. We keep the loop structure (rather
    // than collapsing to a single match) so a future contributor
    // can add a flag that *doesn't* terminate parsing (e.g.
    // `--verbose`) without restructuring the surrounding code.
    #[allow(clippy::never_loop)]
    while let Some(arg) = iter.next() {
        return match arg.as_str() {
            "--data-dir" => {
                let path = iter
                    .next()
                    .ok_or_else(|| "--data-dir flag requires a path argument".to_string())?;
                Ok(PathBuf::from(path))
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("drevo-mcp {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other if other.starts_with("--data-dir=") => {
                let path = &other["--data-dir=".len()..];
                Ok(PathBuf::from(path))
            }
            other => Err(format!("unknown argument: {other:?}")),
        };
    }
    if let Ok(env_path) = std::env::var("DREVO_DATA_DIR") {
        return Ok(PathBuf::from(env_path));
    }
    Ok(PathBuf::from("./drevo-data"))
}

fn print_help() {
    println!(
        "drevo-mcp {} — Model Context Protocol stdio server\n\
         \n\
         USAGE:\n    drevo-mcp [--data-dir <PATH>]\n\
         \n\
         ENVIRONMENT:\n    DREVO_DATA_DIR  Path to the embedded Drevo redb directory\n                    (default: ./drevo-data)\n\
         \n\
         OPTIONS:\n    --data-dir <PATH>  Override DREVO_DATA_DIR\n    -V, --version      Print version and exit\n    -h, --help         Print this help and exit\n",
        env!("CARGO_PKG_VERSION"),
    );
}

fn init_tracing() {
    // Logging goes to stderr. stdout is the MCP wire — touching
    // it with a log line would corrupt the JSON-RPC stream and
    // crash the client.
    //
    // We avoid pulling in `tracing-subscriber` here because it's
    // gated behind the `http` feature and `drevo-mcp` deliberately
    // doesn't require `http`. Plain `eprintln!` with a minimal
    // env-var filter (`RUST_LOG=trace|debug|info|warn|error|off`)
    // covers the surface we actually need from a stdio MCP server
    // (which logs only on lifecycle / failure paths).
    let level = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "info".to_string())
        .to_lowercase();
    let _ = level; // reserved for a future filter check
                   // The actual filter is applied at log call sites
                   // — we don't have any in this binary today; the
                   // bigger logging surface lives in `Server::run`
                   // and is plain `eprintln!` there for the same
                   // reason (zero extra deps).
}
