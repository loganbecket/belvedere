//! Keeps a current reading of Thunderbird's calendars: read at startup,
//! again whenever a calendar database file changes, and checked every
//! minute in case a change was missed. Read-only, like the mail.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use belvedere_core::calendar::{self, Reading, WINDOW_DAYS};
use belvedere_core::ipc::OBJECT_PATH;
use chrono::{DateTime, Utc};
use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::dbus::Service;

/// The latest reading and when it was taken.
#[derive(Debug, Default)]
pub struct State {
    pub reading: Reading,
    pub read_at: Option<DateTime<Utc>>,
    /// Size and modification time of each database file at the last read.
    stamps: Vec<(PathBuf, u64, Option<SystemTime>)>,
}

pub type SharedCalendar = Arc<Mutex<State>>;

/// How often to look for changes the file watcher may have missed.
const CHECK_EVERY: Duration = Duration::from_secs(60);

/// Changes settle for this long before re-reading (SQLite writes in bursts).
const SETTLE: Duration = Duration::from_secs(2);

fn stamps(files: &[PathBuf]) -> Vec<(PathBuf, u64, Option<SystemTime>)> {
    let mut out = Vec::new();
    for f in files {
        for suffix in ["", "-wal"] {
            let mut p = f.as_os_str().to_owned();
            p.push(suffix);
            let p = PathBuf::from(p);
            if let Ok(meta) = std::fs::metadata(&p) {
                out.push((p, meta.len(), meta.modified().ok()));
            }
        }
    }
    out
}

/// The window read: from yesterday to 60 days ahead.
fn window(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    (
        now - chrono::Duration::days(1),
        now + chrono::Duration::days(WINDOW_DAYS),
    )
}

/// Reads the calendars once and stores the result. Returns whether
/// anything in the reading changed.
pub fn refresh(profile: &Path, state: &SharedCalendar) -> bool {
    let started = std::time::Instant::now();
    let now = Utc::now();
    let (from, to) = window(now);
    let reading = match calendar::read(profile, from, to) {
        Ok(r) => r,
        Err(err) => {
            warn!("could not read the calendars: {err}");
            return false;
        }
    };
    let new_stamps = stamps(&reading.files);
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    let changed = st.reading.events != reading.events || st.reading.tasks != reading.tasks;
    info!(
        calendars = reading.calendars.len(),
        events = reading.events.len(),
        tasks = reading.tasks.len(),
        files = reading.files.len(),
        millis = started.elapsed().as_millis() as u64,
        changed,
        "calendar read"
    );
    st.reading = reading;
    st.read_at = Some(now);
    st.stamps = new_stamps;
    changed
}

/// Whether any database file changed since the last read.
fn files_changed(profile: &Path, state: &SharedCalendar) -> bool {
    let files = calendar::database_files(profile);
    let now = stamps(&files);
    let st = state.lock().unwrap_or_else(|e| e.into_inner());
    now != st.stamps
}

async fn announce(bus: &zbus::Connection) {
    if let Ok(iface) = bus
        .object_server()
        .interface::<_, Service>(OBJECT_PATH)
        .await
    {
        let _ = Service::calendar_changed(iface.signal_emitter()).await;
    }
}

/// Runs forever: the first reading, then re-reads on file changes and on
/// a minute check.
pub async fn run(state: SharedCalendar, bus: zbus::Connection) {
    let Some(profile) = crate::mail::profile() else {
        warn!("no Thunderbird profile found; calendar reading is off");
        return;
    };
    let dir = profile.dir.join("calendar-data");
    {
        let p = profile.dir.clone();
        let s = state.clone();
        let _ = tokio::task::spawn_blocking(move || refresh(&p, &s)).await;
    }
    announce(&bus).await;

    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let _watcher = if std::env::var_os("BELVEDERE_NO_MAIL_WATCH").is_none() && dir.is_dir() {
        match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Ok(event) = event {
                if matches!(
                    event.kind,
                    notify::EventKind::Modify(_) | notify::EventKind::Create(_)
                ) {
                    let _ = tx.send(());
                }
            }
        }) {
            Ok(mut w) => {
                if let Err(err) = w.watch(&dir, RecursiveMode::NonRecursive) {
                    warn!(dir = %dir.display(), "could not watch the calendar: {err}");
                }
                Some(w)
            }
            Err(err) => {
                warn!("could not watch the calendar folder: {err}");
                None
            }
        }
    } else {
        None
    };

    let mut tick = tokio::time::interval(CHECK_EVERY);
    tick.tick().await;
    loop {
        tokio::select! {
            Some(()) = rx.recv() => {
                // Let the burst of writes finish.
                let settle = tokio::time::sleep(SETTLE);
                tokio::pin!(settle);
                loop {
                    tokio::select! {
                        Some(()) = rx.recv() => {}
                        _ = &mut settle => break,
                    }
                }
            }
            _ = tick.tick() => {
                if !files_changed(&profile.dir, &state) {
                    continue;
                }
            }
        }
        let p = profile.dir.clone();
        let s = state.clone();
        let changed = tokio::task::spawn_blocking(move || refresh(&p, &s))
            .await
            .unwrap_or(false);
        if changed {
            announce(&bus).await;
        }
    }
}
