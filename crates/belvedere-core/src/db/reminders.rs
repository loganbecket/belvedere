//! Reminders: when to nudge about a task.

use rusqlite::{params, OptionalExtension, Row};

use super::{now, Db, DbError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reminder {
    pub id: i64,
    pub task_id: i64,
    /// RFC 3339.
    pub fire_at: String,
    /// Set once the notification went out.
    pub fired_at: Option<String>,
    /// If snoozed, the new time it should fire instead of `fire_at`.
    pub snoozed_until: Option<String>,
    pub created_at: String,
}

impl Reminder {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Reminder {
            id: row.get("id")?,
            task_id: row.get("task_id")?,
            fire_at: row.get("fire_at")?,
            fired_at: row.get("fired_at")?,
            snoozed_until: row.get("snoozed_until")?,
            created_at: row.get("created_at")?,
        })
    }

    /// The time this reminder is actually due: the snooze if there is one.
    pub fn effective_at(&self) -> &str {
        self.snoozed_until.as_deref().unwrap_or(&self.fire_at)
    }
}

const COLUMNS: &str = "id, task_id, fire_at, fired_at, snoozed_until, created_at";

impl Db {
    pub fn create_reminder(&self, task_id: i64, fire_at: &str) -> Result<Reminder> {
        self.conn.execute(
            "INSERT INTO reminders (task_id, fire_at, created_at) VALUES (?1, ?2, ?3)",
            params![task_id, fire_at, now()],
        )?;
        self.get_reminder(self.conn.last_insert_rowid())
    }

    pub fn get_reminder(&self, id: i64) -> Result<Reminder> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM reminders WHERE id = ?1"),
                [id],
                Reminder::from_row,
            )
            .optional()?
            .ok_or(DbError::NotFound(id))
    }

    pub fn task_reminders(&self, task_id: i64) -> Result<Vec<Reminder>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM reminders WHERE task_id = ?1 ORDER BY fire_at, id"
        ))?;
        let rows = stmt.query_map([task_id], Reminder::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Unfired reminders whose effective time is at or before `at`, for
    /// tasks that are open and not deleted. Soonest first.
    pub fn due_reminders(&self, at: &str) -> Result<Vec<Reminder>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT r.{} FROM reminders r
             JOIN tasks t ON t.id = r.task_id
             WHERE r.fired_at IS NULL
               AND COALESCE(r.snoozed_until, r.fire_at) <= ?1
               AND t.status = 'open' AND t.deleted_at IS NULL
             ORDER BY COALESCE(r.snoozed_until, r.fire_at), r.id",
            COLUMNS.replace(", ", ", r.")
        ))?;
        let rows = stmt.query_map([at], Reminder::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mark_reminder_fired(&self, id: i64) -> Result<Reminder> {
        let changed = self.conn.execute(
            "UPDATE reminders SET fired_at = ?2 WHERE id = ?1 AND fired_at IS NULL",
            params![id, now()],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_reminder(id)
    }

    /// Pushes a reminder to a new time and clears any earlier firing, so
    /// it goes off again.
    pub fn snooze_reminder(&self, id: i64, until: &str) -> Result<Reminder> {
        let changed = self.conn.execute(
            "UPDATE reminders SET snoozed_until = ?2, fired_at = NULL WHERE id = ?1",
            params![id, until],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_reminder(id)
    }

    pub fn delete_reminder(&self, id: i64) -> Result<()> {
        let changed = self
            .conn
            .execute("DELETE FROM reminders WHERE id = ?1", [id])?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        Ok(())
    }

    /// Replaces a task's unfired reminders with a fresh plan. Fired ones
    /// stay as history.
    pub fn replace_task_reminders(
        &self,
        task_id: i64,
        fire_ats: &[String],
    ) -> Result<Vec<Reminder>> {
        self.conn.execute(
            "DELETE FROM reminders WHERE task_id = ?1 AND fired_at IS NULL",
            [task_id],
        )?;
        let ts = now();
        for fire_at in fire_ats {
            self.conn.execute(
                "INSERT INTO reminders (task_id, fire_at, created_at) VALUES (?1, ?2, ?3)",
                params![task_id, fire_at, ts],
            )?;
        }
        self.task_reminders(task_id)
    }

    /// Everything due at `now`, marked fired in the same transaction so a
    /// crash or restart between "decide" and "notify" can at worst drop a
    /// reminder, never double it. Each comes with whether it is late:
    /// more than `late_after` past its time, as after the computer was off.
    pub fn fire_due(
        &self,
        now: &str,
        late_after: chrono::Duration,
    ) -> Result<Vec<(Reminder, bool)>> {
        let now_dt = chrono::DateTime::parse_from_rfc3339(now)
            .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?;
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<Vec<(Reminder, bool)>> {
            let due = self.due_reminders(now)?;
            let mut fired = Vec::with_capacity(due.len());
            for reminder in due {
                self.conn.execute(
                    "UPDATE reminders SET fired_at = ?2 WHERE id = ?1",
                    params![reminder.id, now],
                )?;
                let late = chrono::DateTime::parse_from_rfc3339(reminder.effective_at())
                    .map(|t| now_dt - t > late_after)
                    .unwrap_or(false);
                fired.push((self.get_reminder(reminder.id)?, late));
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

    /// Drops every unfired reminder for a task, e.g. when it is dismissed.
    pub fn cancel_task_reminders(&self, task_id: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM reminders WHERE task_id = ?1 AND fired_at IS NULL",
            [task_id],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NewTask;

    fn task(db: &Db) -> i64 {
        db.create_task(&NewTask {
            title: "Pay the electric bill".into(),
            ..Default::default()
        })
        .unwrap()
        .id
    }

    #[test]
    fn create_read_fire_delete() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let r = db.create_reminder(t, "2026-10-17T09:00:00.000Z").unwrap();
        assert_eq!(db.get_reminder(r.id).unwrap(), r);
        assert_eq!(db.task_reminders(t).unwrap(), vec![r.clone()]);

        let fired = db.mark_reminder_fired(r.id).unwrap();
        assert!(fired.fired_at.is_some());
        assert!(matches!(
            db.mark_reminder_fired(r.id),
            Err(DbError::NotFound(_))
        ));

        db.delete_reminder(r.id).unwrap();
        assert!(matches!(db.get_reminder(r.id), Err(DbError::NotFound(_))));
    }

    #[test]
    fn due_respects_snooze_firing_and_task_state() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let early = db.create_reminder(t, "2026-10-17T09:00:00.000Z").unwrap();
        let late = db.create_reminder(t, "2026-10-20T09:00:00.000Z").unwrap();

        let now = "2026-10-18T00:00:00.000Z";
        let due: Vec<_> = db
            .due_reminders(now)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(due, [early.id]);

        // Snoozing past "now" takes it out of the due list.
        let snoozed = db
            .snooze_reminder(early.id, "2026-10-19T09:00:00.000Z")
            .unwrap();
        assert_eq!(snoozed.effective_at(), "2026-10-19T09:00:00.000Z");
        assert!(db.due_reminders(now).unwrap().is_empty());

        // Once fired, it never comes back.
        db.mark_reminder_fired(late.id).unwrap();
        assert!(db.due_reminders("2027-01-01T00:00:00.000Z").unwrap().len() == 1);

        // A done task's reminders are not due.
        db.complete_task(t).unwrap();
        assert!(db
            .due_reminders("2027-01-01T00:00:00.000Z")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn deleted_tasks_do_not_fire() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        db.create_reminder(t, "2026-10-17T09:00:00.000Z").unwrap();
        db.delete_task(t).unwrap();
        assert!(db
            .due_reminders("2027-01-01T00:00:00.000Z")
            .unwrap()
            .is_empty());
        db.restore_task(t).unwrap();
        assert_eq!(
            db.due_reminders("2027-01-01T00:00:00.000Z").unwrap().len(),
            1
        );
    }

    #[test]
    fn cancel_leaves_fired_history() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let a = db.create_reminder(t, "2026-10-17T09:00:00.000Z").unwrap();
        db.create_reminder(t, "2026-10-20T09:00:00.000Z").unwrap();
        db.mark_reminder_fired(a.id).unwrap();
        assert_eq!(db.cancel_task_reminders(t).unwrap(), 1);
        assert_eq!(db.task_reminders(t).unwrap().len(), 1);
    }

    #[test]
    fn replace_keeps_fired_history_and_resets_the_plan() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let old = db.create_reminder(t, "2026-10-17T09:00:00.000Z").unwrap();
        db.mark_reminder_fired(old.id).unwrap();
        db.create_reminder(t, "2026-10-18T09:00:00.000Z").unwrap();
        let now = db
            .replace_task_reminders(
                t,
                &[
                    "2026-10-20T09:00:00.000Z".into(),
                    "2026-10-20T14:30:00.000Z".into(),
                ],
            )
            .unwrap();
        let times: Vec<_> = now.iter().map(|r| r.fire_at.as_str()).collect();
        assert_eq!(
            times,
            [
                "2026-10-17T09:00:00.000Z",
                "2026-10-20T09:00:00.000Z",
                "2026-10-20T14:30:00.000Z"
            ]
        );
        assert!(now[0].fired_at.is_some());
    }

    #[test]
    fn fire_due_fires_once_even_across_a_restart() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        db.create_reminder(t, "2026-10-20T09:00:00.000Z").unwrap();
        let later = db.create_reminder(t, "2026-10-20T15:00:00.000Z").unwrap();
        let late_after = chrono::Duration::minutes(10);

        let fired = db.fire_due("2026-10-20T09:00:05.000Z", late_after).unwrap();
        assert_eq!(fired.len(), 1);
        assert!(!fired[0].1, "five seconds late is on time");
        assert!(fired[0].0.fired_at.is_some());

        // "Restart": a fresh pass over the same database at the same moment
        // finds nothing new.
        assert!(db
            .fire_due("2026-10-20T09:00:06.000Z", late_after)
            .unwrap()
            .is_empty());

        // The later reminder is untouched until its time.
        assert_eq!(db.get_reminder(later.id).unwrap().fired_at, None);
    }

    #[test]
    fn fire_due_marks_long_overdue_reminders_late() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        db.create_reminder(t, "2026-10-20T09:00:00.000Z").unwrap();
        // The machine was off all morning.
        let fired = db
            .fire_due("2026-10-20T13:00:00.000Z", chrono::Duration::minutes(10))
            .unwrap();
        assert_eq!(fired.len(), 1);
        assert!(fired[0].1, "four hours late is late");
    }

    #[test]
    fn snoozed_reminder_fires_again_at_the_new_time() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let r = db.create_reminder(t, "2026-10-20T09:00:00.000Z").unwrap();
        let late_after = chrono::Duration::minutes(10);
        assert_eq!(
            db.fire_due("2026-10-20T09:00:01.000Z", late_after)
                .unwrap()
                .len(),
            1
        );
        db.snooze_reminder(r.id, "2026-10-20T10:00:01.000Z")
            .unwrap();
        assert!(db
            .fire_due("2026-10-20T09:30:00.000Z", late_after)
            .unwrap()
            .is_empty());
        assert_eq!(
            db.fire_due("2026-10-20T10:00:02.000Z", late_after)
                .unwrap()
                .len(),
            1
        );
        assert!(db
            .fire_due("2026-10-20T10:00:03.000Z", late_after)
            .unwrap()
            .is_empty());
    }
}
