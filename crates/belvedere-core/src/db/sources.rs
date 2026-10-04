//! Where a task came from: an email, a calendar event, or a file. Only a
//! reference is stored (message ID, event UID, path), never the content.

use rusqlite::{params, Row};

use super::{now, Db, DbError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Email,
    Event,
    File,
}

impl SourceKind {
    fn as_str(self) -> &'static str {
        match self {
            SourceKind::Email => "email",
            SourceKind::Event => "event",
            SourceKind::File => "file",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "email" => Ok(SourceKind::Email),
            "event" => Ok(SourceKind::Event),
            "file" => Ok(SourceKind::File),
            other => Err(rusqlite::Error::InvalidParameterName(format!(
                "unknown source kind {other:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSource {
    pub id: i64,
    pub task_id: i64,
    pub kind: SourceKind,
    /// Message-ID, iCalendar UID, or absolute path.
    pub reference: String,
    /// Human-readable, e.g. the email subject.
    pub label: String,
    pub created_at: String,
}

impl TaskSource {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(TaskSource {
            id: row.get("id")?,
            task_id: row.get("task_id")?,
            kind: SourceKind::parse(&row.get::<_, String>("kind")?)?,
            reference: row.get("reference")?,
            label: row.get("label")?,
            created_at: row.get("created_at")?,
        })
    }
}

impl Db {
    pub fn add_task_source(
        &self,
        task_id: i64,
        kind: SourceKind,
        reference: &str,
        label: &str,
    ) -> Result<TaskSource> {
        self.conn.execute(
            "INSERT INTO task_sources (task_id, kind, reference, label, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![task_id, kind.as_str(), reference, label, now()],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(self.conn.query_row(
            "SELECT id, task_id, kind, reference, label, created_at FROM task_sources WHERE id = ?1",
            [id],
            TaskSource::from_row,
        )?)
    }

    pub fn task_sources(&self, task_id: i64) -> Result<Vec<TaskSource>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, kind, reference, label, created_at
             FROM task_sources WHERE task_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([task_id], TaskSource::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Tasks linked to a given source, so a second email about the same
    /// thing can find the task it belongs to.
    pub fn tasks_for_source(&self, kind: SourceKind, reference: &str) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT task_id FROM task_sources WHERE kind = ?1 AND reference = ?2 ORDER BY task_id",
        )?;
        let rows = stmt.query_map(params![kind.as_str(), reference], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn update_task_source_label(&self, id: i64, label: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE task_sources SET label = ?2 WHERE id = ?1",
            params![id, label],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        Ok(())
    }

    pub fn remove_task_source(&self, id: i64) -> Result<()> {
        let changed = self
            .conn
            .execute("DELETE FROM task_sources WHERE id = ?1", [id])?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        Ok(())
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
    fn add_list_update_remove() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let s = db
            .add_task_source(t, SourceKind::Email, "<abc@example.invalid>", "Your bill")
            .unwrap();
        assert_eq!(s.kind, SourceKind::Email);
        assert_eq!(db.task_sources(t).unwrap(), vec![s.clone()]);

        db.update_task_source_label(s.id, "Your October bill")
            .unwrap();
        assert_eq!(db.task_sources(t).unwrap()[0].label, "Your October bill");

        db.remove_task_source(s.id).unwrap();
        assert!(db.task_sources(t).unwrap().is_empty());
        assert!(matches!(
            db.remove_task_source(s.id),
            Err(DbError::NotFound(_))
        ));
    }

    #[test]
    fn same_source_cannot_link_to_one_task_twice() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        db.add_task_source(t, SourceKind::File, "/tmp/bill.pdf", "")
            .unwrap();
        assert!(db
            .add_task_source(t, SourceKind::File, "/tmp/bill.pdf", "")
            .is_err());
    }

    #[test]
    fn lookup_tasks_by_source() {
        let db = Db::open_in_memory().unwrap();
        let a = task(&db);
        let b = task(&db);
        db.add_task_source(a, SourceKind::Event, "uid-1", "")
            .unwrap();
        db.add_task_source(b, SourceKind::Event, "uid-1", "")
            .unwrap();
        assert_eq!(
            db.tasks_for_source(SourceKind::Event, "uid-1").unwrap(),
            [a, b]
        );
        assert!(db
            .tasks_for_source(SourceKind::Email, "uid-1")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn purging_a_task_removes_its_sources() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        db.add_task_source(t, SourceKind::Email, "<x@example.invalid>", "")
            .unwrap();
        db.delete_task(t).unwrap();
        db.purge_task(t).unwrap();
        assert!(db.task_sources(t).unwrap().is_empty());
    }
}
