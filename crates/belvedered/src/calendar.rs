//! Keeps a current reading of Thunderbird's calendars: read at startup,
//! again whenever a calendar database file changes, and checked every
//! minute in case a change was missed. Read-only, like the mail.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use belvedere_core::calendar::{self, Reading, WINDOW_DAYS};
use belvedere_core::db::{Db, EventReminderPlan};
use belvedere_core::ipc::OBJECT_PATH;
use belvedere_core::schedule::{self, EventReminderSettings};
use chrono::{DateTime, Utc};
use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::dbus::{Service, SharedDb};

fn lock(db: &SharedDb) -> std::sync::MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(|e| e.into_inner())
}

/// The event reminder settings as stored (defaults when unset).
pub fn reminder_settings(db: &Db) -> EventReminderSettings {
    let defaults = EventReminderSettings::default();
    let get = |k: &str| db.get_setting(k).ok().flatten();
    EventReminderSettings {
        lead_minutes: get("event_reminder_minutes")
            .and_then(|v| v.parse().ok())
            .unwrap_or(defaults.lead_minutes),
        all_day_time: get("all_day_reminder_time")
            .and_then(|v| chrono::NaiveTime::parse_from_str(&v, "%H:%M").ok())
            .unwrap_or(defaults.all_day_time),
        despite_thunderbird: get("event_reminders_despite_thunderbird")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(defaults.despite_thunderbird),
    }
}

/// Belvedere's reminders for the events in a reading.
pub fn plan_reminders(
    reading: &Reading,
    settings: &EventReminderSettings,
    now: DateTime<Utc>,
) -> Vec<EventReminderPlan> {
    reading
        .events
        .iter()
        .filter_map(|e| {
            let suppressed = reading
                .calendars
                .iter()
                .find(|c| c.id == e.calendar_id)
                .is_some_and(|c| c.alarms_suppressed);
            let at = schedule::event_reminder_at(
                e.start, e.all_day, e.alarm, suppressed, now, settings,
            )?;
            Some(EventReminderPlan {
                event_key: e.key(),
                calendar_id: e.calendar_id.clone(),
                title: e.title.clone(),
                start_at: e.start.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                all_day: e.all_day,
                fire_at: at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            })
        })
        .collect()
}

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

/// Reads the calendars once, stores the result, and plans Belvedere's
/// reminders for the events. Returns whether anything in the reading
/// changed.
pub fn refresh(profile: &Path, state: &SharedCalendar, db: &SharedDb) -> bool {
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
    {
        let db = lock(db);
        let plans = plan_reminders(&reading, &reminder_settings(&db), now);
        match db.replan_event_reminders(&plans) {
            Ok(r) if r.added + r.updated + r.removed > 0 => {
                info!(
                    added = r.added,
                    updated = r.updated,
                    removed = r.removed,
                    "event reminders planned"
                )
            }
            Ok(_) => {}
            Err(err) => warn!("could not plan event reminders: {err}"),
        }
    }
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
pub async fn run(state: SharedCalendar, bus: zbus::Connection, db: SharedDb) {
    let Some(profile) = crate::mail::profile() else {
        warn!("no Thunderbird profile found; calendar reading is off");
        return;
    };
    let dir = profile.dir.join("calendar-data");
    {
        let p = profile.dir.clone();
        let s = state.clone();
        let d = db.clone();
        let _ = tokio::task::spawn_blocking(move || refresh(&p, &s, &d)).await;
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
        let d = db.clone();
        let changed = tokio::task::spawn_blocking(move || refresh(&p, &s, &d))
            .await
            .unwrap_or(false);
        if changed {
            announce(&bus).await;
        }
    }
}
