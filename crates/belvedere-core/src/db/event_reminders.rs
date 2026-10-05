//! Belvedere's reminders for calendar events. One row per event
//! occurrence, keyed by the event so a re-read of the calendar updates
//! rather than duplicates; `fired_at` makes firing restart-safe.

use rusqlite::{params, Row};

use super::{now, Db, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventReminder {
    pub id: i64,
    pub event_key: String,
    pub calendar_id: String,
    pub title: String,
    /// RFC 3339.
    pub start_at: String,
    pub all_day: bool,
    /// RFC 3339.
    pub fire_at: String,
    pub fired_at: Option<String>,
}

/// What the calendar reading wants a reminder for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventReminderPlan {
    pub event_key: String,
    pub calendar_id: String,
    pub title: String,
    pub start_at: String,
    pub all_day: bool,
    pub fire_at: String,
}

impl EventReminder {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(EventReminder {
            id: row.get("id")?,
            event_key: row.get("event_key")?,
            calendar_id: row.get("calendar_id")?,
            title: row.get("title")?,
            start_at: row.get("start_at")?,
            all_day: row.get::<_, i64>("all_day")? != 0,
            fire_at: row.get("fire_at")?,
            fired_at: row.get("fired_at")?,
        })
    }
}

const COLUMNS: &str = "id, event_key, calendar_id, title, start_at, all_day, fire_at, fired_at";

/// What a replan changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Replanned {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
}

impl Db {
    /// Makes the pending reminders match `plans`: new events get a row,
    /// changed ones (time or title) are updated, events no longer planned
    /// lose their unfired row. Fired rows are kept, so a reminder never
    /// fires twice for the same occurrence, even across restarts.
    pub fn replan_event_reminders(&self, plans: &[EventReminderPlan]) -> Result<Replanned> {
        let ts = now();
        let mut out = Replanned::default();
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            let existing = self.all_event_reminders()?;
            for p in plans {
                match existing.iter().find(|e| e.event_key == p.event_key) {
                    None => {
                        self.conn.execute(
                            "INSERT INTO event_reminders (event_key, calendar_id, title, start_at, all_day, fire_at, created_at, updated_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                            params![p.event_key, p.calendar_id, p.title, p.start_at, p.all_day as i64, p.fire_at, ts],
                        )?;
                        out.added += 1;
                    }
                    Some(e)
                        if e.fired_at.is_none()
                            && (e.fire_at != p.fire_at
                                || e.title != p.title
                                || e.start_at != p.start_at) =>
                    {
                        self.conn.execute(
                            "UPDATE event_reminders SET title = ?2, start_at = ?3, all_day = ?4, fire_at = ?5, updated_at = ?6 WHERE id = ?1",
                            params![e.id, p.title, p.start_at, p.all_day as i64, p.fire_at, ts],
                        )?;
                        out.updated += 1;
                    }
                    Some(e) if e.fired_at.is_some() && e.start_at != p.start_at => {
                        // The event moved after we reminded: remind again
                        // for the new time.
                        self.conn.execute(
                            "UPDATE event_reminders SET title = ?2, start_at = ?3, all_day = ?4, fire_at = ?5, fired_at = NULL, updated_at = ?6 WHERE id = ?1",
                            params![e.id, p.title, p.start_at, p.all_day as i64, p.fire_at, ts],
                        )?;
                        out.updated += 1;
                    }
                    Some(_) => {}
                }
            }
            for e in existing.iter().filter(|e| e.fired_at.is_none()) {
                if !plans.iter().any(|p| p.event_key == e.event_key) {
                    self.conn
                        .execute("DELETE FROM event_reminders WHERE id = ?1", [e.id])?;
                    out.removed += 1;
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(out)
            }
            Err(err) => {
                self.conn.execute_batch("ROLLBACK")?;
                Err(err)
            }
        }
    }

    pub fn all_event_reminders(&self) -> Result<Vec<EventReminder>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM event_reminders ORDER BY fire_at, id"
        ))?;
        let rows = stmt.query_map([], EventReminder::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Marks every unfired event reminder at or before `now` fired and
    /// returns them, each with whether it is late (more than `late_after`
    /// past its time).
    pub fn fire_due_event_reminders(
        &self,
        now: &str,
        late_after: chrono::Duration,
    ) -> Result<Vec<(EventReminder, bool)>> {
        let now_dt = chrono::DateTime::parse_from_rfc3339(now)
            .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?;
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<Vec<(EventReminder, bool)>> {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT {COLUMNS} FROM event_reminders WHERE fired_at IS NULL AND fire_at <= ?1 ORDER BY fire_at, id"
            ))?;
            let due: Vec<EventReminder> = stmt
                .query_map([now], EventReminder::from_row)?
                .collect::<rusqlite::Result<_>>()?;
            let mut fired = Vec::with_capacity(due.len());
            for r in due {
                self.conn.execute(
                    "UPDATE event_reminders SET fired_at = ?2 WHERE id = ?1",
                    params![r.id, now],
                )?;
                let late = chrono::DateTime::parse_from_rfc3339(&r.fire_at)
                    .map(|t| now_dt - t > late_after)
                    .unwrap_or(false);
                fired.push((
                    EventReminder {
                        fired_at: Some(now.to_string()),
                        ..r
                    },
                    late,
                ));
            }
            Ok(fired)
        })();
        match result {
            Ok(fired) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(fired)
            }
            Err(err) => {
                self.conn.execute_batch("ROLLBACK")?;
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(key: &str, start: &str, fire: &str) -> EventReminderPlan {
        EventReminderPlan {
            event_key: key.into(),
            calendar_id: "home".into(),
            title: format!("Event {key}"),
            start_at: start.into(),
            all_day: false,
            fire_at: fire.into(),
        }
    }

    #[test]
    fn replanning_adds_updates_removes_and_never_refires_a_fired_one() {
        let db = Db::open_in_memory().unwrap();
        let first = db
            .replan_event_reminders(&[
                plan("a", "2026-10-20T21:00:00Z", "2026-10-20T20:45:00Z"),
                plan("b", "2026-10-21T21:00:00Z", "2026-10-21T20:45:00Z"),
            ])
            .unwrap();
        assert_eq!(
            first,
            Replanned {
                added: 2,
                updated: 0,
                removed: 0
            }
        );

        // Fake clock: a is due, b is not.
        let fired = db
            .fire_due_event_reminders("2026-10-20T20:45:03Z", chrono::Duration::minutes(10))
            .unwrap();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].0.event_key, "a");
        assert!(!fired[0].1, "three seconds late is on time");
        assert!(db
            .fire_due_event_reminders("2026-10-20T20:45:04Z", chrono::Duration::minutes(10))
            .unwrap()
            .is_empty());

        // Same plan again (a restart re-reads the calendar): nothing changes,
        // a stays fired.
        let again = db
            .replan_event_reminders(&[
                plan("a", "2026-10-20T21:00:00Z", "2026-10-20T20:45:00Z"),
                plan("b", "2026-10-21T21:00:00Z", "2026-10-21T20:45:00Z"),
            ])
            .unwrap();
        assert_eq!(again, Replanned::default());
        assert!(db
            .fire_due_event_reminders("2026-10-20T21:00:00Z", chrono::Duration::minutes(10))
            .unwrap()
            .is_empty());

        // b moves; c appears; a (fired) moves too and gets a fresh reminder.
        let moved = db
            .replan_event_reminders(&[
                plan("a", "2026-10-22T21:00:00Z", "2026-10-22T20:45:00Z"),
                plan("b", "2026-10-21T22:00:00Z", "2026-10-21T21:45:00Z"),
                plan("c", "2026-10-23T21:00:00Z", "2026-10-23T20:45:00Z"),
            ])
            .unwrap();
        assert_eq!(
            moved,
            Replanned {
                added: 1,
                updated: 2,
                removed: 0
            }
        );
        let pending: Vec<String> = db
            .all_event_reminders()
            .unwrap()
            .into_iter()
            .filter(|r| r.fired_at.is_none())
            .map(|r| r.event_key)
            .collect();
        assert_eq!(pending, ["b", "a", "c"]);

        // b vanishes from the calendar: its pending reminder goes.
        let gone = db
            .replan_event_reminders(&[
                plan("a", "2026-10-22T21:00:00Z", "2026-10-22T20:45:00Z"),
                plan("c", "2026-10-23T21:00:00Z", "2026-10-23T20:45:00Z"),
            ])
            .unwrap();
        assert_eq!(
            gone,
            Replanned {
                added: 0,
                updated: 0,
                removed: 1
            }
        );

        // Long past its time: reported late.
        let late = db
            .fire_due_event_reminders("2026-10-23T12:00:00Z", chrono::Duration::minutes(10))
            .unwrap();
        assert_eq!(late.len(), 1);
        assert!(late[0].1);
    }
}
