//! Suggested tasks: things Belvedere thinks might need doing but isn't sure
//! enough to add on its own. The user accepts or rejects each one.

use rusqlite::{params, OptionalExtension, Row};

use super::{now, Db, DbError, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub id: i64,
    /// The seen email it came from.
    pub mail_message_id: i64,
    pub title: String,
    pub notes: String,
    /// RFC 3339 or `None`.
    pub due_at: Option<String>,
    /// bill, deadline, ...
    pub kind: String,
    pub amount: Option<f64>,
    pub confidence: f64,
    pub created_at: String,
    pub resolved_at: Option<String>,
    /// `accepted` or `rejected` once resolved.
    pub resolution: Option<String>,
    /// The task created on accept.
    pub task_id: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewSuggestion {
    pub mail_message_id: i64,
    pub title: String,
    pub notes: String,
    pub due_at: Option<String>,
    pub kind: String,
    pub amount: Option<f64>,
    pub confidence: f64,
}

impl Suggestion {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Suggestion {
            id: row.get("id")?,
            mail_message_id: row.get("mail_message_id")?,
            title: row.get("title")?,
            notes: row.get("notes")?,
            due_at: row.get("due_at")?,
            kind: row.get("kind")?,
            amount: row.get("amount")?,
            confidence: row.get("confidence")?,
            created_at: row.get("created_at")?,
            resolved_at: row.get("resolved_at")?,
            resolution: row.get("resolution")?,
            task_id: row.get("task_id")?,
        })
    }
}

const COLUMNS: &str = "id, mail_message_id, title, notes, due_at, kind, amount, confidence, created_at, resolved_at, resolution, task_id";

impl Db {
    pub fn create_suggestion(&self, s: &NewSuggestion) -> Result<Suggestion> {
        self.conn.execute(
            "INSERT INTO suggestions (mail_message_id, title, notes, due_at, kind, amount, confidence, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![s.mail_message_id, s.title, s.notes, s.due_at, s.kind, s.amount, s.confidence, now()],
        )?;
        self.get_suggestion(self.conn.last_insert_rowid())
    }

    pub fn get_suggestion(&self, id: i64) -> Result<Suggestion> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM suggestions WHERE id = ?1"),
                [id],
                Suggestion::from_row,
            )
            .optional()?
            .ok_or(DbError::NotFound(id))
    }

    /// Open suggestions, newest first.
    pub fn list_suggestions(&self) -> Result<Vec<Suggestion>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM suggestions WHERE resolved_at IS NULL ORDER BY id DESC"
        ))?;
        let rows = stmt.query_map([], Suggestion::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Marks a suggestion accepted and records the task made from it.
    pub fn accept_suggestion(&self, id: i64, task_id: i64) -> Result<Suggestion> {
        let changed = self.conn.execute(
            "UPDATE suggestions SET resolved_at = ?2, resolution = 'accepted', task_id = ?3 WHERE id = ?1 AND resolved_at IS NULL",
            params![id, now(), task_id],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_suggestion(id)
    }

    pub fn reject_suggestion(&self, id: i64) -> Result<Suggestion> {
        let changed = self.conn.execute(
            "UPDATE suggestions SET resolved_at = ?2, resolution = 'rejected' WHERE id = ?1 AND resolved_at IS NULL",
            params![id, now()],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_suggestion(id)
    }

    /// Whether a suggestion (open or resolved) already exists for a message.
    pub fn suggestion_for_mail(&self, mail_message_id: i64) -> Result<Option<Suggestion>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM suggestions WHERE mail_message_id = ?1 ORDER BY id DESC LIMIT 1"),
                [mail_message_id],
                Suggestion::from_row,
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(mail_id: i64, title: &str) -> NewSuggestion {
        NewSuggestion {
            mail_message_id: mail_id,
            title: title.into(),
            notes: "From Billing".into(),
            due_at: Some("2026-10-31T14:00:00.000Z".into()),
            kind: "bill".into(),
            amount: Some(84.12),
            confidence: 0.6,
        }
    }

    fn mail(db: &Db, message_id: &str) -> i64 {
        db.record_mail(&crate::db::NewMailMessage {
            message_id: message_id.into(),
            account: "acct".into(),
            folder: "INBOX".into(),
            subject: "Subject".into(),
            mbox_path: "/p/INBOX".into(),
            ..Default::default()
        })
        .unwrap()
        .0
        .id
    }

    #[test]
    fn create_list_accept_reject() {
        let db = Db::open_in_memory().unwrap();
        let (m1, m2) = (mail(&db, "<1@x>"), mail(&db, "<2@x>"));
        let a = db
            .create_suggestion(&new(m1, "Pay the electric bill"))
            .unwrap();
        let b = db
            .create_suggestion(&new(m2, "Renew the passport"))
            .unwrap();
        assert_eq!(a.amount, Some(84.12));
        assert_eq!(
            db.list_suggestions()
                .unwrap()
                .iter()
                .map(|s| s.id)
                .collect::<Vec<_>>(),
            [b.id, a.id]
        );

        let accepted = db.accept_suggestion(a.id, 42).unwrap();
        assert_eq!(accepted.resolution.as_deref(), Some("accepted"));
        assert_eq!(accepted.task_id, Some(42));
        let rejected = db.reject_suggestion(b.id).unwrap();
        assert_eq!(rejected.resolution.as_deref(), Some("rejected"));
        assert!(db.list_suggestions().unwrap().is_empty());

        // Resolving twice is an error, not a silent overwrite.
        assert!(matches!(
            db.reject_suggestion(a.id),
            Err(DbError::NotFound(_))
        ));
        assert_eq!(db.suggestion_for_mail(m2).unwrap().unwrap().id, b.id);
        assert!(db.suggestion_for_mail(999).unwrap().is_none());
    }
}
