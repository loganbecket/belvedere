//! Chat history: conversations and the messages in them.

use rusqlite::{params, OptionalExtension, Row};

use super::{now, Db, DbError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: i64,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "user" => Ok(Role::User),
            "assistant" => Ok(Role::Assistant),
            "system" => Ok(Role::System),
            "tool" => Ok(Role::Tool),
            other => Err(rusqlite::Error::InvalidParameterName(format!(
                "unknown role {other:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: i64,
    pub conversation_id: i64,
    pub role: Role,
    pub content: String,
    pub created_at: String,
}

impl Conversation {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Conversation {
            id: row.get("id")?,
            title: row.get("title")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
            deleted_at: row.get("deleted_at")?,
        })
    }
}

impl Message {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Message {
            id: row.get("id")?,
            conversation_id: row.get("conversation_id")?,
            role: Role::parse(&row.get::<_, String>("role")?)?,
            content: row.get("content")?,
            created_at: row.get("created_at")?,
        })
    }
}

const CONVERSATION_COLUMNS: &str = "id, title, created_at, updated_at, deleted_at";
const MESSAGE_COLUMNS: &str = "id, conversation_id, role, content, created_at";

impl Db {
    pub fn create_conversation(&self, title: &str) -> Result<Conversation> {
        let ts = now();
        self.conn.execute(
            "INSERT INTO conversations (title, created_at, updated_at) VALUES (?1, ?2, ?2)",
            params![title, ts],
        )?;
        self.get_conversation(self.conn.last_insert_rowid())
    }

    pub fn get_conversation(&self, id: i64) -> Result<Conversation> {
        self.conn
            .query_row(
                &format!("SELECT {CONVERSATION_COLUMNS} FROM conversations WHERE id = ?1"),
                [id],
                Conversation::from_row,
            )
            .optional()?
            .ok_or(DbError::NotFound(id))
    }

    /// Non-deleted conversations, most recently active first.
    pub fn list_conversations(&self) -> Result<Vec<Conversation>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CONVERSATION_COLUMNS} FROM conversations
             WHERE deleted_at IS NULL ORDER BY updated_at DESC, id DESC"
        ))?;
        let rows = stmt.query_map([], Conversation::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn rename_conversation(&self, id: i64, title: &str) -> Result<Conversation> {
        let changed = self.conn.execute(
            "UPDATE conversations SET title = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, title, now()],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_conversation(id)
    }

    /// Soft delete; messages stay until the conversation is purged.
    pub fn delete_conversation(&self, id: i64) -> Result<Conversation> {
        let ts = now();
        let changed = self.conn.execute(
            "UPDATE conversations SET deleted_at = ?2, updated_at = ?2
             WHERE id = ?1 AND deleted_at IS NULL",
            params![id, ts],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_conversation(id)
    }

    pub fn restore_conversation(&self, id: i64) -> Result<Conversation> {
        let changed = self.conn.execute(
            "UPDATE conversations SET deleted_at = NULL, updated_at = ?2
             WHERE id = ?1 AND deleted_at IS NOT NULL",
            params![id, now()],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_conversation(id)
    }

    /// Removes a conversation and its messages for good.
    pub fn erase_conversation(&self, id: i64) -> Result<()> {
        let changed = self
            .conn
            .execute("DELETE FROM conversations WHERE id = ?1", [id])?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        Ok(())
    }

    /// Permanently removes a soft-deleted conversation and its messages.
    pub fn purge_conversation(&self, id: i64) -> Result<()> {
        let changed = self.conn.execute(
            "DELETE FROM conversations WHERE id = ?1 AND deleted_at IS NOT NULL",
            [id],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        Ok(())
    }

    /// Appends a message and bumps the conversation's activity time.
    pub fn add_message(&self, conversation_id: i64, role: Role, content: &str) -> Result<Message> {
        let ts = now();
        self.conn.execute(
            "INSERT INTO messages (conversation_id, role, content, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![conversation_id, role.as_str(), content, ts],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute(
            "UPDATE conversations SET updated_at = ?2 WHERE id = ?1",
            params![conversation_id, ts],
        )?;
        self.get_message(id)
    }

    pub fn get_message(&self, id: i64) -> Result<Message> {
        self.conn
            .query_row(
                &format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE id = ?1"),
                [id],
                Message::from_row,
            )
            .optional()?
            .ok_or(DbError::NotFound(id))
    }

    /// Messages in the order they were said.
    pub fn conversation_messages(&self, conversation_id: i64) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE conversation_id = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map([conversation_id], Message::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replaces a message's text, e.g. when a streamed reply finishes.
    pub fn update_message_content(&self, id: i64, content: &str) -> Result<Message> {
        let changed = self.conn.execute(
            "UPDATE messages SET content = ?2 WHERE id = ?1",
            params![id, content],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_message(id)
    }

    pub fn delete_message(&self, id: i64) -> Result<()> {
        let changed = self
            .conn
            .execute("DELETE FROM messages WHERE id = ?1", [id])?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_crud_with_soft_delete() {
        let db = Db::open_in_memory().unwrap();
        let c = db.create_conversation("Bills").unwrap();
        assert_eq!(db.get_conversation(c.id).unwrap(), c);

        let renamed = db.rename_conversation(c.id, "October bills").unwrap();
        assert_eq!(renamed.title, "October bills");

        db.delete_conversation(c.id).unwrap();
        assert!(db.list_conversations().unwrap().is_empty());
        db.restore_conversation(c.id).unwrap();
        assert_eq!(db.list_conversations().unwrap().len(), 1);

        assert!(matches!(
            db.purge_conversation(c.id),
            Err(DbError::NotFound(_))
        ));
        db.delete_conversation(c.id).unwrap();
        db.purge_conversation(c.id).unwrap();
        assert!(matches!(
            db.get_conversation(c.id),
            Err(DbError::NotFound(_))
        ));
    }

    #[test]
    fn messages_keep_order_and_bump_activity() {
        let db = Db::open_in_memory().unwrap();
        let older = db.create_conversation("Older").unwrap();
        let newer = db.create_conversation("Newer").unwrap();
        // Most recently created lists first.
        assert_eq!(db.list_conversations().unwrap()[0].id, newer.id);

        // Timestamps have millisecond precision; make sure the message
        // lands in a later millisecond than the conversations did.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let m1 = db.add_message(older.id, Role::User, "What's due?").unwrap();
        let m2 = db
            .add_message(older.id, Role::Assistant, "Electric bill, Oct 20.")
            .unwrap();
        assert_eq!(
            db.conversation_messages(older.id).unwrap(),
            vec![m1.clone(), m2.clone()]
        );
        assert_eq!(m2.role, Role::Assistant);
        // Activity moved the older conversation to the top.
        assert_eq!(db.list_conversations().unwrap()[0].id, older.id);

        let edited = db
            .update_message_content(m2.id, "Electric bill, due October 20.")
            .unwrap();
        assert_eq!(edited.content, "Electric bill, due October 20.");

        db.delete_message(m1.id).unwrap();
        assert_eq!(db.conversation_messages(older.id).unwrap().len(), 1);
        assert!(matches!(db.get_message(m1.id), Err(DbError::NotFound(_))));
    }

    #[test]
    fn purging_a_conversation_removes_its_messages() {
        let db = Db::open_in_memory().unwrap();
        let c = db.create_conversation("x").unwrap();
        let m = db.add_message(c.id, Role::System, "hello").unwrap();
        db.delete_conversation(c.id).unwrap();
        db.purge_conversation(c.id).unwrap();
        assert!(matches!(db.get_message(m.id), Err(DbError::NotFound(_))));
    }

    #[test]
    fn erasing_a_conversation_takes_its_messages_with_it() {
        let db = Db::open_in_memory().unwrap();
        let gone = db.create_conversation("gone").unwrap();
        let kept = db.create_conversation("kept").unwrap();
        db.add_message(gone.id, Role::User, "hi").unwrap();
        db.erase_conversation(gone.id).unwrap();
        assert!(db.get_conversation(gone.id).is_err());
        assert!(db.conversation_messages(gone.id).unwrap().is_empty());
        assert!(db.get_conversation(kept.id).is_ok());
        assert!(db.erase_conversation(gone.id).is_err());
    }
}
