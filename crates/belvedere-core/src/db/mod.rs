//! Belvedere's own storage: one SQLite file, versioned migrations, and a
//! small data-access layer over it.
//!
//! This is the only thing Belvedere writes to. Everything read from
//! Thunderbird or the filesystem is somebody else's data and never lands
//! here except as a reference (a message ID, an event UID, a path).

mod conversations;
mod event_reminders;
mod mail;
mod migrations;
mod models;
mod reminders;
mod rules;
mod settings;
mod sources;
mod suggestions;
mod tasks;

use std::path::{Path, PathBuf};

use rusqlite::Connection;

pub use conversations::{Conversation, Message, Role};
pub use event_reminders::{EventReminder, EventReminderPlan, Replanned};
pub use mail::{MailMessage, MboxState, NewMailMessage};
pub use models::{Model, ModelSource, NewModel};
pub use reminders::Reminder;
pub use rules::Rule;
pub use sources::{SourceKind, TaskSource};
pub use suggestions::{NewSuggestion, Suggestion};
pub use tasks::{NewTask, Task, TaskStatus};

/// Anything that can go wrong talking to the database.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not create data directory {0}: {1}")]
    DataDir(PathBuf, std::io::Error),
    #[error("database is at schema version {found}, newer than this build supports ({supported})")]
    TooNew { found: u32, supported: u32 },
    #[error("no row with id {0}")]
    NotFound(i64),
}

pub type Result<T> = std::result::Result<T, DbError>;

/// An open handle to Belvedere's database.
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Where the database lives: `$XDG_DATA_HOME/belvedere/belvedere.db`,
    /// falling back to `~/.local/share/belvedere/belvedere.db`.
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
        Some(base.join("belvedere").join("belvedere.db"))
    }

    /// Opens (creating if needed) the database at `path` and brings its
    /// schema up to date.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| DbError::DataDir(dir.to_path_buf(), e))?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// A fresh, private, in-memory database. For tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let db = Self { conn };
        migrations::apply(&db.conn)?;
        Ok(db)
    }

    /// The schema version currently on disk.
    pub fn schema_version(&self) -> Result<u32> {
        migrations::current_version(&self.conn)
    }

    /// Direct access, for the rare query the typed layer doesn't cover.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// Timestamps are stored as RFC 3339 strings in UTC, so they sort as text
/// and read back without ambiguity.
pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
