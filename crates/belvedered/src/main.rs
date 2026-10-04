//! The Belvedere background service.
//!
//! Right now it only stays alive, logs to the journal, and shuts down
//! cleanly when asked. Everything else arrives in later chunks.

use std::time::Duration;

use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// How often the service writes a heartbeat line, so a glance at the
/// journal shows it is alive.
const HEARTBEAT: Duration = Duration::from_secs(300);

fn main() {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("{}", belvedere_core::version_line("belvedered"));
        return;
    }

    init_logging();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(run());
}

/// Logs go to journald when it is reachable, otherwise to stderr (which
/// systemd also captures). `RUST_LOG` controls verbosity; default `info`.
fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);

    match tracing_journald::layer() {
        Ok(journald) => registry.with(journald).init(),
        Err(err) => {
            registry.with(fmt::layer().with_target(false)).init();
            warn!("journald unavailable ({err}); logging to stderr");
        }
    }
}

async fn run() {
    info!(version = belvedere_core::VERSION, "belvedered starting");

    let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await; // the first tick fires immediately; skip it

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                info!("SIGTERM received; shutting down");
                break;
            }
            _ = sigint.recv() => {
                info!("SIGINT received; shutting down");
                break;
            }
            _ = heartbeat.tick() => {
                info!("alive");
            }
        }
    }

    info!("belvedered stopped");
}
