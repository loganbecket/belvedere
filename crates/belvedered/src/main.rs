//! The Belvedere background service.
//!
//! Right now it opens its database, stays alive, logs to the journal, and
//! shuts down cleanly when asked. Everything else arrives in later chunks.

mod calendar;
mod chat;
mod dbus;
mod mail;
mod models;
mod notify;
mod pipeline;
mod scheduler;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use belvedere_core::db::Db;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// How often the service writes a heartbeat line, so a glance at the
/// journal shows it is alive.
const HEARTBEAT: Duration = Duration::from_secs(300);

fn main() {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("{}", belvedere_core::version_line("belvedered"));
        return;
    }
    if std::env::args().any(|arg| arg == "--list-mail") {
        list_mail_and_exit();
    }
    if std::env::args().any(|arg| arg == "--list-calendar") {
        list_calendar_and_exit();
    }

    init_logging();

    let db = match open_database() {
        Ok(db) => db,
        Err(err) => {
            error!("{err:#}");
            std::process::exit(1);
        }
    };
    models::rescan(&db);

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(run(db));
}

/// Debug aid: prints Thunderbird's accounts and folders as Belvedere sees
/// them, then exits. Same data as the `ListMailAccounts` bus method.
fn list_mail_and_exit() -> ! {
    use belvedere_core::thunderbird;
    let Some(profile) = thunderbird::find_profile() else {
        eprintln!("no Thunderbird profile found");
        std::process::exit(1);
    };
    println!("profile: {} ({})", profile.dir.display(), profile.source);
    match thunderbird::accounts(&profile.dir) {
        Ok(accounts) => {
            for a in accounts {
                println!(
                    "{} [{}] {} ({} folders)",
                    a.key,
                    a.kind,
                    a.name,
                    a.folders.len()
                );
                for f in a.folders {
                    println!(
                        "    {:<9} {}{}",
                        f.role.as_str(),
                        f.path,
                        if f.mbox.is_some() {
                            ""
                        } else {
                            "  (no local mail)"
                        }
                    );
                }
            }
            std::process::exit(0);
        }
        Err(err) => {
            eprintln!("could not read accounts: {err}");
            std::process::exit(1);
        }
    }
}

/// Debug aid: prints the calendars and the next two weeks of events as
/// Belvedere reads them (titles shortened), then exits.
fn list_calendar_and_exit() -> ! {
    use belvedere_core::calendar;
    let Some(profile) = mail::profile() else {
        eprintln!("no Thunderbird profile found");
        std::process::exit(1);
    };
    let now = chrono::Utc::now();
    match calendar::read(&profile.dir, now, now + chrono::Duration::days(14)) {
        Ok(reading) => {
            for c in &reading.calendars {
                println!(
                    "calendar {} [{}]{}{}",
                    c.name,
                    c.kind,
                    if c.enabled { "" } else { " (disabled)" },
                    if c.kind == "storage" || reading.on_disk.contains(&c.id) {
                        ""
                    } else {
                        " (not on disk: Thunderbird holds it in memory only)"
                    }
                );
            }
            println!("{} event(s) in the next 14 days:", reading.events.len());
            for e in &reading.events {
                let local = e.start.with_timezone(&chrono::Local);
                let title: String = e.title.chars().take(24).collect();
                println!(
                    "  {} {} {}{}",
                    local.format("%a %b %-d %H:%M"),
                    if e.all_day { "(all day)" } else { "         " },
                    title,
                    if e.recurrence_id.is_some() {
                        " (recurring)"
                    } else {
                        ""
                    }
                );
            }
            println!("{} task(s)", reading.tasks.len());
            std::process::exit(0);
        }
        Err(err) => {
            eprintln!("could not read the calendars: {err}");
            std::process::exit(1);
        }
    }
}

/// Logs go to journald when it is reachable, otherwise to stderr (which
/// systemd also captures). `RUST_LOG` controls verbosity; default `info`.
fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,llama_cpp_2=warn,llama_cpp_sys_2=warn"));
    let registry = tracing_subscriber::registry().with(filter);

    match tracing_journald::layer() {
        Ok(journald) => registry.with(journald).init(),
        Err(err) => {
            registry.with(fmt::layer().with_target(false)).init();
            warn!("journald unavailable ({err}); logging to stderr");
        }
    }
}

/// Opens Belvedere's database at its default location, creating it on
/// first run. `BELVEDERE_DB` overrides the path (used by tests).
fn open_database() -> anyhow::Result<Db> {
    let path = match std::env::var_os("BELVEDERE_DB") {
        Some(p) => std::path::PathBuf::from(p),
        None => Db::default_path()
            .context("cannot determine data directory: neither XDG_DATA_HOME nor HOME is set")?,
    };
    let db = Db::open(&path).with_context(|| format!("opening database at {}", path.display()))?;
    info!(path = %path.display(), schema = db.schema_version()?, "database ready");
    Ok(db)
}

/// Unloads the model after it has sat unused for the configured time
/// (setting `model_idle_unload_minutes`, default 5), so its memory is only
/// held while something is actually being asked of it.
async fn idle_unload(engine: belvedere_core::engine::Engine, db: dbus::SharedDb) {
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    loop {
        tick.tick().await;
        let minutes = db
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_setting("model_idle_unload_minutes")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(5);
        let idle_after = Duration::from_secs(minutes * 60);
        let state = engine.state();
        if belvedere_core::engine::should_unload(
            &state,
            engine.last_used(),
            std::time::Instant::now(),
            idle_after,
        ) {
            info!(model = state.model_name(), minutes, "model idle; unloading");
            engine.unload().await;
        }
    }
}

async fn run(db: Db) {
    info!(version = belvedere_core::VERSION, "belvedered starting");

    let db = Arc::new(Mutex::new(db));
    let engine = belvedere_core::engine::Engine::new();
    let engine_for_pipeline = engine.clone();
    tokio::spawn(idle_unload(engine.clone(), db.clone()));
    let calendar_state: calendar::SharedCalendar = Default::default();
    // Kept alive for the whole run; dropping it would leave the bus.
    let bus = match dbus::serve(db.clone(), engine, calendar_state.clone()).await {
        Ok(conn) => conn,
        Err(err) => {
            error!("could not start D-Bus service: {err}");
            std::process::exit(1);
        }
    };
    let name_lost = dbus::name_lost(&bus);
    tokio::pin!(name_lost);
    // One notifier for everything that shows notifications, so the click
    // loop in the scheduler can act on all of them.
    let notifier = match notify::Notifier::new(&bus).await {
        Ok(n) => Some(std::sync::Arc::new(tokio::sync::Mutex::new(n))),
        Err(err) => {
            error!("notifications unavailable: {err}; reminders will not be shown");
            None
        }
    };
    if let Some(n) = &notifier {
        tokio::spawn(scheduler::run(db.clone(), bus.clone(), n.clone()));
    }
    // Calendar: read at startup, on change, and checked every minute.
    tokio::spawn(calendar::run(calendar_state, bus.clone(), db.clone()));
    // Mail: scan Thunderbird's folders at startup and whenever they change.
    let (mail_tx, mail_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
    tokio::spawn(mail::run(db.clone(), mail_tx));
    // New mail is read by the model and becomes tasks or suggestions.
    tokio::spawn(pipeline::run(
        db,
        engine_for_pipeline,
        bus.clone(),
        mail_rx,
        notifier,
    ));

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
            _ = &mut name_lost => {
                // Unreachable on the bus is as good as dead; exit non-zero
                // so systemd restarts us and we claim the name again.
                error!("lost the bus name {}; exiting so systemd can restart the service", belvedere_core::ipc::BUS_NAME);
                std::process::exit(1);
            }
        }
    }

    info!("belvedered stopped");
}
