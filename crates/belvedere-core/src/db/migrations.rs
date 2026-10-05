//! Schema migrations, applied in order. The version on disk is SQLite's
//! `user_version` pragma. Each entry runs exactly once, inside a
//! transaction, and nothing is ever edited in place: a schema change is a
//! new entry at the end.

use rusqlite::Connection;

use super::{DbError, Result};

/// Every migration, oldest first. Index `n` takes the schema from version
/// `n` to `n + 1`.
pub(crate) const MIGRATIONS: &[&str] = &[
    // 0 -> 1: the initial schema.
    r#"
    CREATE TABLE tasks (
        id             INTEGER PRIMARY KEY,
        title          TEXT    NOT NULL,
        notes          TEXT    NOT NULL DEFAULT '',
        due_at         TEXT,
        status         TEXT    NOT NULL DEFAULT 'open'
                       CHECK (status IN ('open', 'done', 'dismissed')),
        dismiss_reason TEXT,
        created_at     TEXT    NOT NULL,
        updated_at     TEXT    NOT NULL,
        completed_at   TEXT,
        deleted_at     TEXT
    );
    CREATE INDEX tasks_due_at ON tasks (due_at) WHERE deleted_at IS NULL;

    CREATE TABLE task_sources (
        id         INTEGER PRIMARY KEY,
        task_id    INTEGER NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
        kind       TEXT    NOT NULL CHECK (kind IN ('email', 'event', 'file')),
        reference  TEXT    NOT NULL,
        label      TEXT    NOT NULL DEFAULT '',
        created_at TEXT    NOT NULL,
        UNIQUE (task_id, kind, reference)
    );
    CREATE INDEX task_sources_reference ON task_sources (kind, reference);

    CREATE TABLE reminders (
        id            INTEGER PRIMARY KEY,
        task_id       INTEGER NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
        fire_at       TEXT    NOT NULL,
        fired_at      TEXT,
        snoozed_until TEXT,
        created_at    TEXT    NOT NULL
    );
    CREATE INDEX reminders_pending ON reminders (fire_at) WHERE fired_at IS NULL;

    CREATE TABLE rules (
        id         INTEGER PRIMARY KEY,
        text       TEXT    NOT NULL,
        enabled    INTEGER NOT NULL DEFAULT 1,
        created_at TEXT    NOT NULL,
        updated_at TEXT    NOT NULL,
        deleted_at TEXT
    );

    CREATE TABLE conversations (
        id         INTEGER PRIMARY KEY,
        title      TEXT    NOT NULL DEFAULT '',
        created_at TEXT    NOT NULL,
        updated_at TEXT    NOT NULL,
        deleted_at TEXT
    );

    CREATE TABLE messages (
        id              INTEGER PRIMARY KEY,
        conversation_id INTEGER NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
        role            TEXT    NOT NULL CHECK (role IN ('user', 'assistant', 'system', 'tool')),
        content         TEXT    NOT NULL,
        created_at      TEXT    NOT NULL
    );
    CREATE INDEX messages_conversation ON messages (conversation_id, id);

    CREATE TABLE models (
        id             INTEGER PRIMARY KEY,
        name           TEXT    NOT NULL,
        path           TEXT    NOT NULL UNIQUE,
        source         TEXT    NOT NULL
                       CHECK (source IN ('lmstudio', 'ollama', 'belvedere', 'import')),
        size_bytes     INTEGER NOT NULL DEFAULT 0,
        quantization   TEXT    NOT NULL DEFAULT '',
        supports_tools INTEGER,
        created_at     TEXT    NOT NULL
    );

    CREATE TABLE settings (
        key        TEXT PRIMARY KEY,
        value      TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    "#,
    // 1 -> 2: mail Belvedere has seen, and how far each mbox was read.
    r#"
    CREATE TABLE mail_messages (
        id           INTEGER PRIMARY KEY,
        message_id   TEXT    NOT NULL UNIQUE,
        account      TEXT    NOT NULL,
        folder       TEXT    NOT NULL,
        from_addr    TEXT    NOT NULL DEFAULT '',
        from_name    TEXT    NOT NULL DEFAULT '',
        to_addrs     TEXT    NOT NULL DEFAULT '',
        subject      TEXT    NOT NULL DEFAULT '',
        date         TEXT    NOT NULL DEFAULT '',
        body_text    TEXT    NOT NULL DEFAULT '',
        attachments  TEXT    NOT NULL DEFAULT '[]',
        mbox_path    TEXT    NOT NULL,
        mbox_offset  INTEGER NOT NULL DEFAULT 0,
        seen_at      TEXT    NOT NULL,
        processed_at TEXT
    );
    CREATE INDEX mail_messages_unprocessed ON mail_messages (id) WHERE processed_at IS NULL;
    CREATE INDEX mail_messages_seen ON mail_messages (seen_at);

    CREATE TABLE mail_folder_state (
        mbox_path  TEXT    PRIMARY KEY,
        scanned_to INTEGER NOT NULL DEFAULT 0,
        size       INTEGER NOT NULL DEFAULT 0,
        mtime      INTEGER NOT NULL DEFAULT 0,
        head       BLOB    NOT NULL DEFAULT X'',
        updated_at TEXT    NOT NULL
    );
    "#,
    // 2 -> 3: suggested tasks awaiting a yes or no.
    r#"
    CREATE TABLE suggestions (
        id              INTEGER PRIMARY KEY,
        mail_message_id INTEGER NOT NULL REFERENCES mail_messages (id) ON DELETE CASCADE,
        title           TEXT    NOT NULL,
        notes           TEXT    NOT NULL DEFAULT '',
        due_at          TEXT,
        kind            TEXT    NOT NULL DEFAULT 'other',
        amount          REAL,
        confidence      REAL    NOT NULL DEFAULT 0,
        created_at      TEXT    NOT NULL,
        resolved_at     TEXT,
        resolution      TEXT    CHECK (resolution IN ('accepted', 'rejected')),
        task_id         INTEGER
    );
    CREATE INDEX suggestions_open ON suggestions (id) WHERE resolved_at IS NULL;
    "#,
    // 3 -> 4: which messages a message answers (to notice replies), and
    // what kind of thing a task is (bill, reply needed, ...).
    r#"
    ALTER TABLE mail_messages ADD COLUMN replies_to TEXT NOT NULL DEFAULT '';
    ALTER TABLE tasks ADD COLUMN kind TEXT NOT NULL DEFAULT '';
    "#,
    // 4 -> 5: the account or invoice number a mail-born task is about, so
    // a follow-up email can be matched to it.
    r#"
    ALTER TABLE tasks ADD COLUMN reference TEXT NOT NULL DEFAULT '';
    "#,
    // 5 -> 6: Belvedere's own reminders for calendar events.
    r#"
    CREATE TABLE event_reminders (
        id          INTEGER PRIMARY KEY,
        event_key   TEXT    NOT NULL UNIQUE,
        calendar_id TEXT    NOT NULL DEFAULT '',
        title       TEXT    NOT NULL DEFAULT '',
        start_at    TEXT    NOT NULL,
        all_day     INTEGER NOT NULL DEFAULT 0,
        fire_at     TEXT    NOT NULL,
        fired_at    TEXT,
        created_at  TEXT    NOT NULL,
        updated_at  TEXT    NOT NULL
    );
    CREATE INDEX event_reminders_pending ON event_reminders (fire_at) WHERE fired_at IS NULL;
    "#,
    // 6 -> 7: the name Thunderbird gave a task it created on the Belvedere
    // calendar, so its later requests find the same task.
    r#"
    ALTER TABLE tasks ADD COLUMN caldav_name TEXT NOT NULL DEFAULT '';
    "#,
    // 7 -> 8: text read from PDF attachments, kept with the message.
    r#"
    ALTER TABLE mail_messages ADD COLUMN attachment_text TEXT NOT NULL DEFAULT '';
    "#,
    // 8 -> 9: model downloads from Hugging Face, resumable.
    r#"
    CREATE TABLE downloads (
        id          INTEGER PRIMARY KEY,
        repo        TEXT    NOT NULL,
        file        TEXT    NOT NULL,
        url         TEXT    NOT NULL,
        path        TEXT    NOT NULL,
        size        INTEGER NOT NULL DEFAULT 0,
        sha256      TEXT    NOT NULL DEFAULT '',
        received    INTEGER NOT NULL DEFAULT 0,
        status      TEXT    NOT NULL DEFAULT 'queued'
                    CHECK (status IN ('queued', 'downloading', 'paused', 'verifying', 'done', 'failed')),
        error       TEXT    NOT NULL DEFAULT '',
        created_at  TEXT    NOT NULL,
        updated_at  TEXT    NOT NULL
    );
    "#,
];

/// The schema version this build expects.
pub(crate) const LATEST: u32 = MIGRATIONS.len() as u32;

pub(crate) fn current_version(conn: &Connection) -> Result<u32> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

/// Brings `conn` from whatever version it is at up to [`LATEST`].
pub(crate) fn apply(conn: &Connection) -> Result<()> {
    apply_up_to(conn, LATEST)
}

/// Applies migrations until the schema is at `target`. Exposed so tests
/// can stop at an older version and then upgrade from it.
pub(crate) fn apply_up_to(conn: &Connection, target: u32) -> Result<()> {
    let found = current_version(conn)?;
    if found > LATEST {
        return Err(DbError::TooNew {
            found,
            supported: LATEST,
        });
    }
    for version in found..target {
        let sql = MIGRATIONS[version as usize];
        conn.execute_batch("BEGIN")?;
        let result = conn
            .execute_batch(sql)
            .and_then(|_| conn.pragma_update(None, "user_version", version + 1));
        match result {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(err) => {
                conn.execute_batch("ROLLBACK")?;
                return Err(err.into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn
    }

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn empty_database_migrates_to_latest() {
        let conn = fresh();
        assert_eq!(current_version(&conn).unwrap(), 0);
        apply(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), LATEST);
        assert_eq!(
            table_names(&conn),
            [
                "conversations",
                "downloads",
                "event_reminders",
                "mail_folder_state",
                "mail_messages",
                "messages",
                "models",
                "reminders",
                "rules",
                "settings",
                "suggestions",
                "task_sources",
                "tasks",
            ]
        );
    }

    #[test]
    fn each_previous_version_upgrades_to_latest() {
        // Stop at every older version in turn, then upgrade from there.
        for stop in 0..LATEST {
            let conn = fresh();
            apply_up_to(&conn, stop).unwrap();
            assert_eq!(current_version(&conn).unwrap(), stop);
            apply(&conn).unwrap();
            assert_eq!(
                current_version(&conn).unwrap(),
                LATEST,
                "from version {stop}"
            );
        }
    }

    #[test]
    fn applying_twice_changes_nothing() {
        let conn = fresh();
        apply(&conn).unwrap();
        let before = table_names(&conn);
        apply(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), LATEST);
        assert_eq!(table_names(&conn), before);
    }

    #[test]
    fn a_newer_database_is_refused() {
        let conn = fresh();
        conn.pragma_update(None, "user_version", LATEST + 1)
            .unwrap();
        match apply(&conn) {
            Err(DbError::TooNew { found, supported }) => {
                assert_eq!(found, LATEST + 1);
                assert_eq!(supported, LATEST);
            }
            other => panic!("expected TooNew, got {other:?}"),
        }
    }

    #[test]
    fn a_failing_migration_rolls_back() {
        let conn = fresh();
        apply(&conn).unwrap();
        // Simulate a bad future migration by running the same shape by hand.
        conn.execute_batch("BEGIN").unwrap();
        let err = conn.execute_batch("CREATE TABLE ok (x); CREATE TABLE tasks (dup);");
        assert!(err.is_err());
        conn.execute_batch("ROLLBACK").unwrap();
        assert!(!table_names(&conn).contains(&"ok".to_string()));
    }
}
