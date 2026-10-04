//! Key/value settings. Values are strings; callers parse what they need.

use rusqlite::{params, OptionalExtension};

use super::{now, Db, Result};

impl Db {
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Inserts or replaces.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, now()],
        )?;
        Ok(())
    }

    /// Returns whether anything was removed.
    pub fn delete_setting(&self, key: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM settings WHERE key = ?1", [key])?
            > 0)
    }

    /// Every setting, sorted by key.
    pub fn list_settings(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM settings ORDER BY key")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_update_delete() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.get_setting("sweep_minutes").unwrap(), None);

        db.set_setting("sweep_minutes", "5").unwrap();
        assert_eq!(
            db.get_setting("sweep_minutes").unwrap().as_deref(),
            Some("5")
        );

        db.set_setting("sweep_minutes", "10").unwrap();
        assert_eq!(
            db.get_setting("sweep_minutes").unwrap().as_deref(),
            Some("10")
        );

        db.set_setting("briefing_time", "08:00").unwrap();
        assert_eq!(
            db.list_settings().unwrap(),
            vec![
                ("briefing_time".to_string(), "08:00".to_string()),
                ("sweep_minutes".to_string(), "10".to_string()),
            ]
        );

        assert!(db.delete_setting("sweep_minutes").unwrap());
        assert!(!db.delete_setting("sweep_minutes").unwrap());
        assert_eq!(db.get_setting("sweep_minutes").unwrap(), None);
    }
}
