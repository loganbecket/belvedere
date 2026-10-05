//! Mail Belvedere has seen: a normalized copy of each message's essentials
//! (never the raw message), keyed by Message-ID so the same message in two
//! folders, or after a move, is one message. Plus where each mbox file
//! had been read to, so only new bytes are read next time.

use rusqlite::{params, OptionalExtension, Row};

use super::{now, Db, Result};

/// A seen message, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailMessage {
    pub id: i64,
    pub message_id: String,
    pub account: String,
    pub folder: String,
    pub from_addr: String,
    pub from_name: String,
    /// Comma-separated.
    pub to_addrs: String,
    pub subject: String,
    /// RFC 3339, or empty if the message had no usable date.
    pub date: String,
    /// Plain text body, possibly truncated.
    pub body_text: String,
    /// Attachment file names, JSON array.
    pub attachments: String,
    /// Where it was read from.
    pub mbox_path: String,
    pub mbox_offset: i64,
    pub seen_at: String,
    /// Set once a later stage (extraction) has handled it.
    pub processed_at: Option<String>,
    /// Message-IDs this message answers, space-separated, nearest first.
    pub replies_to: String,
    /// Text read from PDF attachments, or empty.
    pub attachment_text: String,
}

/// What a caller supplies for a newly seen message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewMailMessage {
    pub message_id: String,
    pub account: String,
    pub folder: String,
    pub from_addr: String,
    pub from_name: String,
    pub to_addrs: String,
    pub subject: String,
    pub date: String,
    pub body_text: String,
    pub attachments: Vec<String>,
    pub mbox_path: String,
    pub mbox_offset: i64,
    pub replies_to: Vec<String>,
    pub attachment_text: String,
}

impl MailMessage {
    /// The Message-IDs this message answers, nearest first.
    pub fn replies_to_ids(&self) -> Vec<&str> {
        self.replies_to.split_whitespace().collect()
    }

    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(MailMessage {
            id: row.get("id")?,
            message_id: row.get("message_id")?,
            account: row.get("account")?,
            folder: row.get("folder")?,
            from_addr: row.get("from_addr")?,
            from_name: row.get("from_name")?,
            to_addrs: row.get("to_addrs")?,
            subject: row.get("subject")?,
            date: row.get("date")?,
            body_text: row.get("body_text")?,
            attachments: row.get("attachments")?,
            mbox_path: row.get("mbox_path")?,
            mbox_offset: row.get("mbox_offset")?,
            seen_at: row.get("seen_at")?,
            processed_at: row.get("processed_at")?,
            replies_to: row.get("replies_to")?,
            attachment_text: row.get("attachment_text")?,
        })
    }
}

const COLUMNS: &str = "id, message_id, account, folder, from_addr, from_name, to_addrs, subject, date, body_text, attachments, mbox_path, mbox_offset, seen_at, processed_at, replies_to, attachment_text";

/// Where reading of one mbox file stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MboxState {
    pub mbox_path: String,
    /// Bytes read so far.
    pub scanned_to: i64,
    /// Size and modification time (seconds) when last read, to notice
    /// rewrites (Thunderbird compacting the folder).
    pub size: i64,
    pub mtime: i64,
    /// The first bytes of the file when last read; if they change, the
    /// file was rewritten and must be read from the start.
    pub head: Vec<u8>,
}

impl Db {
    /// Records a message unless one with the same Message-ID is already
    /// known. Returns the stored row and whether it was new.
    pub fn record_mail(&self, m: &NewMailMessage) -> Result<(MailMessage, bool)> {
        if let Some(existing) = self.mail_by_message_id(&m.message_id)? {
            return Ok((existing, false));
        }
        let attachments = serde_json::to_string(&m.attachments).unwrap_or_else(|_| "[]".into());
        self.conn.execute(
            "INSERT INTO mail_messages (message_id, account, folder, from_addr, from_name, to_addrs, subject, date, body_text, attachments, mbox_path, mbox_offset, seen_at, replies_to, attachment_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                m.message_id,
                m.account,
                m.folder,
                m.from_addr,
                m.from_name,
                m.to_addrs,
                m.subject,
                m.date,
                m.body_text,
                attachments,
                m.mbox_path,
                m.mbox_offset,
                now(),
                m.replies_to.join(" "),
                m.attachment_text
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        let row = self.conn.query_row(
            &format!("SELECT {COLUMNS} FROM mail_messages WHERE id = ?1"),
            [id],
            MailMessage::from_row,
        )?;
        Ok((row, true))
    }

    pub fn get_mail(&self, id: i64) -> Result<MailMessage> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM mail_messages WHERE id = ?1"),
                [id],
                MailMessage::from_row,
            )
            .optional()?
            .ok_or(super::DbError::NotFound(id))
    }

    pub fn mail_by_message_id(&self, message_id: &str) -> Result<Option<MailMessage>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM mail_messages WHERE message_id = ?1"),
                [message_id],
                MailMessage::from_row,
            )
            .optional()?)
    }

    /// Most recently seen first.
    pub fn recent_mail(&self, limit: usize) -> Result<Vec<MailMessage>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM mail_messages ORDER BY seen_at DESC, id DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], MailMessage::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Messages not yet handled by a later stage, oldest first.
    pub fn unprocessed_mail(&self, limit: usize) -> Result<Vec<MailMessage>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM mail_messages WHERE processed_at IS NULL ORDER BY id LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], MailMessage::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn unprocessed_mail_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM mail_messages WHERE processed_at IS NULL",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn mark_mail_processed(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE mail_messages SET processed_at = ?2 WHERE id = ?1",
            params![id, now()],
        )?;
        Ok(())
    }

    /// Messages whose sender, subject, or text contains any of the words,
    /// newest first.
    pub fn search_mail(&self, query: &str, limit: usize) -> Result<Vec<MailMessage>> {
        let words: Vec<String> = query
            .split_whitespace()
            .filter(|w| w.len() > 1)
            .map(|w| format!("%{}%", w.replace(['%', '_'], "")))
            .collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let clause = words
            .iter()
            .enumerate()
            .map(|(i, _)| {
                format!(
                    "(subject LIKE ?{n} OR from_addr LIKE ?{n} OR from_name LIKE ?{n} OR body_text LIKE ?{n})",
                    n = i + 1
                )
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT {COLUMNS} FROM mail_messages WHERE {clause} ORDER BY date DESC, id DESC LIMIT {limit}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(words.iter()),
            MailMessage::from_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mail_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM mail_messages", [], |r| r.get(0))?)
    }

    pub fn mbox_state(&self, mbox_path: &str) -> Result<Option<MboxState>> {
        Ok(self
            .conn
            .query_row(
                "SELECT mbox_path, scanned_to, size, mtime, head FROM mail_folder_state WHERE mbox_path = ?1",
                [mbox_path],
                |r| {
                    Ok(MboxState {
                        mbox_path: r.get(0)?,
                        scanned_to: r.get(1)?,
                        size: r.get(2)?,
                        mtime: r.get(3)?,
                        head: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn set_mbox_state(&self, s: &MboxState) -> Result<()> {
        self.conn.execute(
            "INSERT INTO mail_folder_state (mbox_path, scanned_to, size, mtime, head, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (mbox_path) DO UPDATE SET scanned_to = excluded.scanned_to, size = excluded.size,
                mtime = excluded.mtime, head = excluded.head, updated_at = excluded.updated_at",
            params![s.mbox_path, s.scanned_to, s.size, s.mtime, s.head, now()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, folder: &str) -> NewMailMessage {
        NewMailMessage {
            message_id: id.into(),
            account: "acct".into(),
            folder: folder.into(),
            from_addr: "billing@example.invalid".into(),
            from_name: "Billing".into(),
            to_addrs: "me@example.invalid".into(),
            subject: "Your bill".into(),
            date: "2026-10-20T14:00:00+00:00".into(),
            body_text: "Due soon.".into(),
            attachments: vec!["bill.pdf".into()],
            mbox_path: format!("/p/ImapMail/x/{folder}"),
            mbox_offset: 0,
            replies_to: vec!["<0@x>".into()],
            attachment_text: String::new(),
        }
    }

    #[test]
    fn same_message_id_in_another_folder_is_not_new() {
        let db = Db::open_in_memory().unwrap();
        let (a, new_a) = db.record_mail(&msg("<1@x>", "INBOX")).unwrap();
        assert!(new_a);
        assert_eq!(a.attachments, "[\"bill.pdf\"]");
        assert_eq!(a.replies_to, "<0@x>");
        let (b, new_b) = db.record_mail(&msg("<1@x>", "Archive")).unwrap();
        assert!(!new_b);
        assert_eq!(b.id, a.id);
        assert_eq!(b.folder, "INBOX", "the first sighting is kept");
        assert_eq!(db.mail_count().unwrap(), 1);
    }

    #[test]
    fn recent_and_unprocessed_and_marking() {
        let db = Db::open_in_memory().unwrap();
        let (a, _) = db.record_mail(&msg("<1@x>", "INBOX")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let (b, _) = db.record_mail(&msg("<2@x>", "INBOX")).unwrap();
        let recent = db.recent_mail(10).unwrap();
        assert_eq!(
            recent.iter().map(|m| m.id).collect::<Vec<_>>(),
            [b.id, a.id]
        );
        assert_eq!(db.recent_mail(1).unwrap().len(), 1);

        assert_eq!(db.unprocessed_mail(10).unwrap().len(), 2);
        assert_eq!(db.unprocessed_mail_count().unwrap(), 2);
        db.mark_mail_processed(a.id).unwrap();
        assert_eq!(db.unprocessed_mail_count().unwrap(), 1);
        let left = db.unprocessed_mail(10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, b.id);
        assert!(db
            .mail_by_message_id("<1@x>")
            .unwrap()
            .unwrap()
            .processed_at
            .is_some());
    }

    #[test]
    fn mbox_state_round_trips() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.mbox_state("/p/INBOX").unwrap(), None);
        let s = MboxState {
            mbox_path: "/p/INBOX".into(),
            scanned_to: 1234,
            size: 5000,
            mtime: 1_700_000_000,
            head: b"From - Mon".to_vec(),
        };
        db.set_mbox_state(&s).unwrap();
        assert_eq!(db.mbox_state("/p/INBOX").unwrap(), Some(s.clone()));
        let s2 = MboxState {
            scanned_to: 5000,
            ..s
        };
        db.set_mbox_state(&s2).unwrap();
        assert_eq!(db.mbox_state("/p/INBOX").unwrap().unwrap().scanned_to, 5000);
    }
}
