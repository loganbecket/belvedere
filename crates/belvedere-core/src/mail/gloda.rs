//! Thunderbird's own search index (`global-messages-db.sqlite`, "Gloda").
//! Its full-text table uses a tokenizer only Thunderbird has, so the
//! plain content table behind it is searched instead. The file is opened
//! read-only and immutable: no locks taken, nothing written, nothing
//! Thunderbird would notice. It is large, so it is never copied.

use std::path::Path;

use chrono::{DateTime, Utc};

/// One matching message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// `<...>` Message-ID.
    pub message_id: String,
    pub author: String,
    pub subject: String,
    /// RFC 3339 or empty.
    pub date: String,
    pub snippet: String,
}

/// The index file of a profile, if it has one.
pub fn index_file(profile: &Path) -> Option<std::path::PathBuf> {
    let p = profile.join("global-messages-db.sqlite");
    p.is_file().then_some(p)
}

/// Messages whose author, subject, or body contains any of the words,
/// newest first.
pub fn search(index: &Path, query: &str, limit: usize) -> rusqlite::Result<Vec<Hit>> {
    let words: Vec<String> = query
        .split_whitespace()
        .filter(|w| w.len() > 1)
        .map(|w| format!("%{}%", w.replace(['%', '_'], "")))
        .collect();
    if words.is_empty() {
        return Ok(Vec::new());
    }
    let uri = format!("file:{}?immutable=1&mode=ro", index.display());
    let conn = rusqlite::Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_URI
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(500))?;
    let clause = words
        .iter()
        .enumerate()
        .map(|(i, _)| {
            format!(
                "(t.c1subject LIKE ?{n} OR t.c3author LIKE ?{n} OR t.c0body LIKE ?{n})",
                n = i + 1
            )
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    let sql = format!(
        "SELECT m.headerMessageID, t.c3author, t.c1subject, m.date, substr(t.c0body, 1, 240)
         FROM messagesText_content t JOIN messages m ON m.id = t.docid
         WHERE m.deleted = 0 AND ({clause})
         ORDER BY m.date DESC LIMIT {limit}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(words.iter()), |r| {
        let id: Option<String> = r.get(0)?;
        let author: Option<String> = r.get(1)?;
        let subject: Option<String> = r.get(2)?;
        let date: Option<i64> = r.get(3)?;
        let body: Option<String> = r.get(4)?;
        Ok(Hit {
            message_id: id
                .map(|i| format!("<{}>", i.trim().trim_matches(['<', '>'])))
                .unwrap_or_default(),
            author: author.unwrap_or_default(),
            subject: subject.unwrap_or_default(),
            date: date
                .and_then(DateTime::<Utc>::from_timestamp_micros)
                .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                .unwrap_or_default(),
            snippet: body
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searches_the_content_table_without_the_tokenizer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("global-messages-db.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, folderID INTEGER, messageKey INTEGER,
                conversationID INTEGER NOT NULL, date INTEGER, headerMessageID TEXT,
                deleted INTEGER NOT NULL default 0, jsonAttributes TEXT, notability INTEGER NOT NULL default 0);
             CREATE TABLE 'messagesText_content'(docid INTEGER PRIMARY KEY, 'c0body', 'c1subject', 'c2attachmentNames', 'c3author', 'c4recipients');
             INSERT INTO messages VALUES (1, 1, 1, 1, 1760000000000000, 'lease-1@example.invalid', 0, NULL, 0);
             INSERT INTO messagesText_content VALUES (1, 'The lease renews in January. Let me know by the 15th.', 'Lease renewal', '', 'Dana Whitfield <dana@example.invalid>', 'me@example.invalid');
             INSERT INTO messages VALUES (2, 1, 2, 2, 1761000000000000, 'gone@example.invalid', 1, NULL, 0);
             INSERT INTO messagesText_content VALUES (2, 'lease lease lease', 'Deleted lease mail', '', 'x@example.invalid', '');",
        )
        .unwrap();
        drop(conn);
        assert_eq!(index_file(dir.path()).unwrap(), path);
        let hits = search(&path, "landlord lease", 10).unwrap();
        assert_eq!(hits.len(), 1, "deleted messages are skipped");
        assert_eq!(hits[0].message_id, "<lease-1@example.invalid>");
        assert_eq!(hits[0].subject, "Lease renewal");
        assert!(hits[0].author.contains("Dana"));
        assert!(hits[0].snippet.starts_with("The lease renews"));
        assert!(hits[0].date.starts_with("2025-10-09"));
        assert!(search(&path, "nothing-here", 10).unwrap().is_empty());
    }
}
