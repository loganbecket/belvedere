//! Fires reminders when they come due and acts on the buttons people
//! press on them.

use std::time::Duration;

use belvedere_core::db::{Db, Reminder};
use belvedere_core::ipc::OBJECT_PATH;
use belvedere_core::schedule;
use chrono::{Local, Utc};
use tracing::{error, info, warn};

use crate::dbus::{Service, SharedDb};
use crate::notify::{Action, Clicked, SharedNotifier, Signal, Subject};
use crate::pipeline::LEAD_DAYS;

/// How often to look for due reminders. The plan allows 10 seconds.
pub fn tick_interval() -> Duration {
    std::env::var("BELVEDERE_TICK_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(5))
}

/// A reminder this much past its time is reported as missed rather than
/// as if it were on time (the computer was off, or the service was down).
const LATE_AFTER: chrono::Duration = chrono::Duration::minutes(10);

/// How long Snooze pushes a reminder.
const SNOOZE: chrono::Duration = chrono::Duration::hours(1);

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Runs forever: fires due reminders on every tick and handles clicks.
pub async fn run(db: SharedDb, bus: zbus::Connection, notifier: SharedNotifier) {
    let mut clicks = match notifier.lock().await.clicks().await {
        Ok(c) => c,
        Err(err) => {
            error!("cannot listen for notification clicks: {err}");
            return;
        }
    };
    let mut tick = tokio::time::interval(tick_interval());

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let mut n = notifier.lock().await;
                fire_due(&db, &mut n).await;
                fire_due_events(&db, &mut n).await;
            }
            signal = clicks.next() => match signal {
                Signal::Action { id, key } => {
                    let clicked = notifier.lock().await.resolve(id, &key);
                    if let Some(clicked) = clicked {
                        handle_click(&db, &bus, clicked).await;
                    }
                }
                Signal::Closed { id } => notifier.lock().await.forget(id),
                Signal::Ignore => {}
                Signal::Gone => {
                    warn!("notification daemon went away; reminders paused until restart");
                    return;
                }
            },
        }
    }
}

fn lock(db: &SharedDb) -> std::sync::MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(|e| e.into_inner())
}

async fn fire_due(db: &SharedDb, notifier: &mut crate::notify::Notifier) {
    let fired = match lock(db).fire_due(&now_rfc3339(), LATE_AFTER) {
        Ok(fired) => fired,
        Err(err) => {
            error!("checking reminders: {err}");
            return;
        }
    };
    for (reminder, late) in fired {
        let task = match lock(db).get_task(reminder.task_id) {
            Ok(task) => task,
            Err(err) => {
                warn!(reminder = reminder.id, "reminder for missing task: {err}");
                continue;
            }
        };
        let (title, body) = words_for(&task.title, &reminder, late);
        let subject = Subject {
            task_id: task.id,
            reminder_id: reminder.id,
        };
        if let Err(err) = notifier.remind(subject, &title, &body).await {
            error!(task = task.id, "could not show reminder: {err}");
        }
    }
}

async fn fire_due_events(db: &SharedDb, notifier: &mut crate::notify::Notifier) {
    let fired = match lock(db).fire_due_event_reminders(&now_rfc3339(), LATE_AFTER) {
        Ok(fired) => fired,
        Err(err) => {
            error!("checking event reminders: {err}");
            return;
        }
    };
    for (r, late) in fired {
        let (title, body) = event_words(&r, late, Local::now());
        if let Err(err) = notifier.event_reminder(&title, &body).await {
            error!(event = r.event_key, "could not show event reminder: {err}");
        } else {
            info!(event = r.event_key, late, "event reminder shown");
        }
    }
}

/// Words for an event reminder: when it starts, in local time.
pub fn event_words(
    r: &belvedere_core::db::EventReminder,
    late: bool,
    now: chrono::DateTime<Local>,
) -> (String, String) {
    let start = chrono::DateTime::parse_from_rfc3339(&r.start_at)
        .map(|d| d.with_timezone(&Local))
        .unwrap_or(now);
    let when = if r.all_day {
        if start.date_naive() == now.date_naive() {
            "Today, all day".to_string()
        } else {
            format!("{}, all day", start.format("%A, %B %-d"))
        }
    } else if start.date_naive() == now.date_naive() {
        let minutes = (start - now).num_minutes();
        if minutes > 0 {
            format!(
                "Starts at {} (in {} min)",
                start.format("%-I:%M %p"),
                minutes
            )
        } else {
            format!("Started at {}", start.format("%-I:%M %p"))
        }
    } else {
        format!(
            "{} at {}",
            start.format("%A, %B %-d"),
            start.format("%-I:%M %p")
        )
    };
    let body = if late {
        format!("Missed reminder. {when}")
    } else {
        when
    };
    (r.title.clone(), body)
}

/// The notification's title and body.
fn words_for(task_title: &str, reminder: &Reminder, late: bool) -> (String, String) {
    let when = schedule::due_label(reminder.effective_at(), Local::now());
    let body = if late {
        format!("Missed reminder from {when}")
    } else {
        format!("Due {when}")
    };
    (task_title.to_string(), body)
}

/// Reopens a task Belvedere closed on its own and plans its reminders
/// again from its due date (the due-day one and the lead one; anything
/// already in the past is not re-created).
pub fn undo_close(db: &Db, task_id: i64) -> belvedere_core::db::Result<belvedere_core::db::Task> {
    let task = db.reopen_task(task_id)?;
    if let Some(due) = &task.due_at {
        let plan = schedule::plan_reminders_with_lead(due, Local::now(), LEAD_DAYS);
        db.replace_task_reminders(task_id, &plan)?;
    }
    info!(task = task_id, "close undone; task reopened");
    Ok(task)
}

async fn handle_click(db: &SharedDb, bus: &zbus::Connection, clicked: Clicked) {
    let Subject {
        task_id,
        reminder_id,
    } = clicked.subject;
    let result = match clicked.action {
        Action::Done => {
            let db = lock(db);
            db.complete_task(task_id)
                .and_then(|_| db.cancel_task_reminders(task_id))
                .map(|_| ())
        }
        Action::SnoozeHour => {
            let until = (Utc::now() + SNOOZE).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            lock(db).snooze_reminder(reminder_id, &until).map(|_| ())
        }
        Action::NotNeeded => {
            let db = lock(db);
            db.dismiss_task(task_id, "not needed")
                .and_then(|_| db.cancel_task_reminders(task_id))
                .map(|_| ())
        }
        Action::Undo => undo_close(&lock(db), task_id).map(|_| ()),
        Action::Open => {
            open_window(bus, task_id).await;
            Ok(())
        }
    };
    match result {
        Ok(()) => {
            info!(task = task_id, action = ?clicked.action, "reminder button pressed");
            if clicked.action != Action::Open {
                announce_change(bus).await;
            }
        }
        Err(err) => error!(task = task_id, "acting on reminder button: {err}"),
    }
}

/// Starts (or focuses) the window, then asks it to show the task.
/// `BELVEDERE_NO_WINDOW_LAUNCH` skips the launch (tests set it, so a
/// test run never pops a real window onto the desktop).
async fn open_window(bus: &zbus::Connection, task_id: i64) {
    let launched = std::env::var_os("BELVEDERE_NO_WINDOW_LAUNCH").is_some()
        || std::process::Command::new("belvedere").spawn().is_ok()
        || std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".local/bin/belvedere"))
            .is_some_and(|p| std::process::Command::new(p).spawn().is_ok());
    if !launched {
        warn!("could not launch the Belvedere window");
    }
    // Give a fresh window a moment to connect before telling it what to show.
    let bus = bus.clone();
    tokio::spawn(async move {
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if let Ok(iface) = bus
                .object_server()
                .interface::<_, Service>(OBJECT_PATH)
                .await
            {
                let _ = Service::show_task(iface.signal_emitter(), task_id).await;
            }
        }
    });
}

async fn announce_change(bus: &zbus::Connection) {
    if let Ok(iface) = bus
        .object_server()
        .interface::<_, Service>(OBJECT_PATH)
        .await
    {
        let _ = Service::tasks_changed(iface.signal_emitter()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missed_reminders_say_so() {
        let reminder = Reminder {
            id: 1,
            task_id: 1,
            fire_at: "2026-10-20T14:00:00.000Z".into(),
            fired_at: None,
            snoozed_until: None,
            created_at: String::new(),
        };
        let (title, on_time) = words_for("Renew the passport", &reminder, false);
        let (_, late) = words_for("Renew the passport", &reminder, true);
        assert_eq!(title, "Renew the passport");
        assert!(on_time.starts_with("Due "));
        assert!(late.starts_with("Missed reminder from "));
    }
}
