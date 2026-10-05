//! Tasks: the thing Belvedere exists to produce.

use rusqlite::{params, OptionalExtension, Row};

use super::{now, Db, DbError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Open,
    Done,
    Dismissed,
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Open => "open",
            TaskStatus::Done => "done",
            TaskStatus::Dismissed => "dismissed",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "open" => Ok(TaskStatus::Open),
            "done" => Ok(TaskStatus::Done),
            "dismissed" => Ok(TaskStatus::Dismissed),
            other => Err(rusqlite::Error::InvalidParameterName(format!(
                "unknown task status {other:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: i64,
    pub title: String,
    pub notes: String,
    /// RFC 3339, or `None` for a task with no date.
    pub due_at: Option<String>,
    pub status: TaskStatus,
    pub dismiss_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    /// Set when soft-deleted. Deleted tasks stay in the table so they can
    /// be restored.
    pub deleted_at: Option<String>,
    /// What sort of thing it is: `bill`, `reply_needed`, ... or empty for
    /// a task typed in by hand.
    pub kind: String,
    /// The account, invoice, or policy number it concerns, or empty.
    pub reference: String,
}

/// What a caller supplies to create a task; everything else is derived.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewTask {
    pub title: String,
    pub notes: String,
    pub due_at: Option<String>,
}

impl Task {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Task {
            id: row.get("id")?,
            title: row.get("title")?,
            notes: row.get("notes")?,
            due_at: row.get("due_at")?,
            status: TaskStatus::parse(&row.get::<_, String>("status")?)?,
            dismiss_reason: row.get("dismiss_reason")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
            completed_at: row.get("completed_at")?,
            deleted_at: row.get("deleted_at")?,
            kind: row.get("kind")?,
            reference: row.get("reference")?,
        })
    }
}

const COLUMNS: &str = "id, title, notes, due_at, status, dismiss_reason, created_at, updated_at, completed_at, deleted_at, kind, reference";

impl Db {
    pub fn create_task(&self, new: &NewTask) -> Result<Task> {
        let ts = now();
        self.conn.execute(
            "INSERT INTO tasks (title, notes, due_at, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'open', ?4, ?4)",
            params![new.title, new.notes, new.due_at, ts],
        )?;
        self.get_task(self.conn.last_insert_rowid())
    }

    /// Fetches any task by id, deleted or not.
    pub fn get_task(&self, id: i64) -> Result<Task> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM tasks WHERE id = ?1"),
                [id],
                Task::from_row,
            )
            .optional()?
            .ok_or(DbError::NotFound(id))
    }

    /// Not deleted, with a non-empty kind (that is, made from mail), open
    /// or closed, newest first: the tasks a follow-up email might belong
    /// to. Closed ones are included so a reminder for a bill already
    /// paid lands on it quietly instead of becoming a new task.
    pub fn mail_tasks(&self, limit: usize) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM tasks WHERE deleted_at IS NULL AND kind != ''
             ORDER BY id DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], Task::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every task that has not been soft-deleted, soonest due first, then
    /// undated tasks, then by creation.
    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM tasks WHERE deleted_at IS NULL
             ORDER BY due_at IS NULL, due_at, id"
        ))?;
        let rows = stmt.query_map([], Task::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Soft-deleted tasks, most recently deleted first.
    pub fn list_deleted_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM tasks WHERE deleted_at IS NOT NULL ORDER BY deleted_at DESC"
        ))?;
        let rows = stmt.query_map([], Task::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replaces title, notes, and due date.
    pub fn update_task(&self, id: i64, new: &NewTask) -> Result<Task> {
        let changed = self.conn.execute(
            "UPDATE tasks SET title = ?2, notes = ?3, due_at = ?4, updated_at = ?5 WHERE id = ?1",
            params![id, new.title, new.notes, new.due_at, now()],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_task(id)
    }

    /// Records what sort of thing a task is (see `Task::kind`) and the
    /// account or invoice number it concerns, if any.
    pub fn set_task_kind(&self, id: i64, kind: &str, reference: &str) -> Result<Task> {
        let changed = self.conn.execute(
            "UPDATE tasks SET kind = ?2, reference = ?3 WHERE id = ?1",
            params![id, kind, reference],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_task(id)
    }

    pub fn complete_task(&self, id: i64) -> Result<Task> {
        self.set_status(id, TaskStatus::Done, None)
    }

    /// Closes a task without doing it, recording why ("already paid").
    pub fn dismiss_task(&self, id: i64, reason: &str) -> Result<Task> {
        self.set_status(id, TaskStatus::Dismissed, Some(reason))
    }

    pub fn reopen_task(&self, id: i64) -> Result<Task> {
        self.set_status(id, TaskStatus::Open, None)
    }

    fn set_status(&self, id: i64, status: TaskStatus, reason: Option<&str>) -> Result<Task> {
        let ts = now();
        // Done and dismissed both record when the task was closed.
        let completed_at = (!matches!(status, TaskStatus::Open)).then(|| ts.clone());
        let changed = self.conn.execute(
            "UPDATE tasks SET status = ?2, dismiss_reason = ?3, completed_at = ?4, updated_at = ?5
             WHERE id = ?1",
            params![id, status.as_str(), reason, completed_at, ts],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_task(id)
    }

    /// Marks a task deleted. It disappears from `list_tasks` but stays on
    /// disk; `restore_task` brings it back untouched.
    pub fn delete_task(&self, id: i64) -> Result<Task> {
        let ts = now();
        let changed = self.conn.execute(
            "UPDATE tasks SET deleted_at = ?2, updated_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![id, ts],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_task(id)
    }

    pub fn restore_task(&self, id: i64) -> Result<Task> {
        let changed = self.conn.execute(
            "UPDATE tasks SET deleted_at = NULL, updated_at = ?2 WHERE id = ?1 AND deleted_at IS NOT NULL",
            params![id, now()],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_task(id)
    }

    /// Removes a soft-deleted task for good, along with its sources and
    /// reminders. Refuses to purge a task that was never soft-deleted.
    pub fn purge_task(&self, id: i64) -> Result<()> {
        let changed = self.conn.execute(
            "DELETE FROM tasks WHERE id = ?1 AND deleted_at IS NOT NULL",
            [id],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn pay_bill() -> NewTask {
        NewTask {
            title: "Pay the electric bill".into(),
            notes: "$84.12".into(),
            due_at: Some("2026-10-20T00:00:00.000Z".into()),
        }
    }

    #[test]
    fn create_and_read() {
        let db = db();
        let t = db.create_task(&pay_bill()).unwrap();
        assert_eq!(t.title, "Pay the electric bill");
        assert_eq!(t.status, TaskStatus::Open);
        assert_eq!(t.created_at, t.updated_at);
        assert_eq!(db.get_task(t.id).unwrap(), t);
    }

    #[test]
    fn missing_task_is_not_found() {
        let db = db();
        assert!(matches!(db.get_task(99), Err(DbError::NotFound(99))));
        assert!(matches!(
            db.update_task(99, &pay_bill()),
            Err(DbError::NotFound(99))
        ));
    }

    #[test]
    fn list_orders_by_due_date_with_undated_last() {
        let db = db();
        let undated = db
            .create_task(&NewTask {
                title: "Someday".into(),
                ..Default::default()
            })
            .unwrap();
        let later = db
            .create_task(&NewTask {
                due_at: Some("2026-12-01T00:00:00.000Z".into()),
                ..pay_bill()
            })
            .unwrap();
        let sooner = db.create_task(&pay_bill()).unwrap();
        let ids: Vec<_> = db.list_tasks().unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(ids, [sooner.id, later.id, undated.id]);
    }

    #[test]
    fn update_changes_fields_and_timestamp() {
        let db = db();
        let t = db.create_task(&pay_bill()).unwrap();
        let updated = db
            .update_task(
                t.id,
                &NewTask {
                    title: "Pay the gas bill".into(),
                    notes: String::new(),
                    due_at: None,
                },
            )
            .unwrap();
        assert_eq!(updated.title, "Pay the gas bill");
        assert_eq!(updated.notes, "");
        assert_eq!(updated.due_at, None);
        assert_eq!(updated.created_at, t.created_at);
        assert!(updated.updated_at >= t.updated_at);
    }

    #[test]
    fn complete_dismiss_reopen() {
        let db = db();
        let t = db.create_task(&pay_bill()).unwrap();

        let done = db.complete_task(t.id).unwrap();
        assert_eq!(done.status, TaskStatus::Done);
        assert!(done.completed_at.is_some());

        let dismissed = db.dismiss_task(t.id, "already paid").unwrap();
        assert_eq!(dismissed.status, TaskStatus::Dismissed);
        assert_eq!(dismissed.dismiss_reason.as_deref(), Some("already paid"));
        assert!(
            dismissed.completed_at.is_some(),
            "a dismissal records when the task was closed"
        );

        let open = db.reopen_task(t.id).unwrap();
        assert_eq!(open.status, TaskStatus::Open);
        assert_eq!(open.dismiss_reason, None);
    }

    #[test]
    fn delete_is_soft_and_restorable() {
        let db = db();
        let t = db.create_task(&pay_bill()).unwrap();

        let deleted = db.delete_task(t.id).unwrap();
        assert!(deleted.deleted_at.is_some());
        assert!(db.list_tasks().unwrap().is_empty());
        assert_eq!(db.list_deleted_tasks().unwrap().len(), 1);
        // Still readable by id.
        assert_eq!(db.get_task(t.id).unwrap().title, t.title);
        // Deleting twice is an error, not a silent no-op.
        assert!(matches!(db.delete_task(t.id), Err(DbError::NotFound(_))));

        let restored = db.restore_task(t.id).unwrap();
        assert_eq!(restored.deleted_at, None);
        assert_eq!(restored.title, t.title);
        assert_eq!(restored.notes, t.notes);
        assert_eq!(restored.due_at, t.due_at);
        assert_eq!(db.list_tasks().unwrap().len(), 1);
    }

    #[test]
    fn purge_only_works_on_deleted_tasks() {
        let db = db();
        let t = db.create_task(&pay_bill()).unwrap();
        assert!(matches!(db.purge_task(t.id), Err(DbError::NotFound(_))));
        db.delete_task(t.id).unwrap();
        db.purge_task(t.id).unwrap();
        assert!(matches!(db.get_task(t.id), Err(DbError::NotFound(_))));
    }
}
