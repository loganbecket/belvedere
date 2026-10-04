//! Standing rules the user has stated: "the car insurance is on autopay".
//! Stored as the user's own words; interpretation happens elsewhere.

use rusqlite::{params, OptionalExtension, Row};

use super::{now, Db, DbError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub id: i64,
    pub text: String,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

impl Rule {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Rule {
            id: row.get("id")?,
            text: row.get("text")?,
            enabled: row.get("enabled")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
            deleted_at: row.get("deleted_at")?,
        })
    }
}

const COLUMNS: &str = "id, text, enabled, created_at, updated_at, deleted_at";

impl Db {
    pub fn create_rule(&self, text: &str) -> Result<Rule> {
        let ts = now();
        self.conn.execute(
            "INSERT INTO rules (text, enabled, created_at, updated_at) VALUES (?1, 1, ?2, ?2)",
            params![text, ts],
        )?;
        self.get_rule(self.conn.last_insert_rowid())
    }

    pub fn get_rule(&self, id: i64) -> Result<Rule> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM rules WHERE id = ?1"),
                [id],
                Rule::from_row,
            )
            .optional()?
            .ok_or(DbError::NotFound(id))
    }

    /// Rules that are not deleted, oldest first. Disabled ones included,
    /// so the UI can show them switched off.
    pub fn list_rules(&self) -> Result<Vec<Rule>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM rules WHERE deleted_at IS NULL ORDER BY id"
        ))?;
        let rows = stmt.query_map([], Rule::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn update_rule(&self, id: i64, text: &str, enabled: bool) -> Result<Rule> {
        let changed = self.conn.execute(
            "UPDATE rules SET text = ?2, enabled = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, text, enabled, now()],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_rule(id)
    }

    /// Soft delete, restorable.
    pub fn delete_rule(&self, id: i64) -> Result<Rule> {
        let ts = now();
        let changed = self.conn.execute(
            "UPDATE rules SET deleted_at = ?2, updated_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![id, ts],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_rule(id)
    }

    pub fn restore_rule(&self, id: i64) -> Result<Rule> {
        let changed = self.conn.execute(
            "UPDATE rules SET deleted_at = NULL, updated_at = ?2 WHERE id = ?1 AND deleted_at IS NOT NULL",
            params![id, now()],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_rule(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crud_with_soft_delete() {
        let db = Db::open_in_memory().unwrap();
        let r = db.create_rule("the car insurance is on autopay").unwrap();
        assert!(r.enabled);
        assert_eq!(db.get_rule(r.id).unwrap(), r);
        assert_eq!(db.list_rules().unwrap().len(), 1);

        let off = db
            .update_rule(r.id, "car insurance is on autopay", false)
            .unwrap();
        assert!(!off.enabled);
        assert_eq!(off.text, "car insurance is on autopay");
        assert_eq!(db.list_rules().unwrap().len(), 1);

        db.delete_rule(r.id).unwrap();
        assert!(db.list_rules().unwrap().is_empty());
        assert!(db.get_rule(r.id).unwrap().deleted_at.is_some());
        assert!(matches!(db.delete_rule(r.id), Err(DbError::NotFound(_))));

        let back = db.restore_rule(r.id).unwrap();
        assert_eq!(back.deleted_at, None);
        assert_eq!(back.text, off.text);
        assert_eq!(db.list_rules().unwrap().len(), 1);
    }

    #[test]
    fn unknown_rule_is_not_found() {
        let db = Db::open_in_memory().unwrap();
        assert!(matches!(db.get_rule(7), Err(DbError::NotFound(7))));
        assert!(matches!(
            db.update_rule(7, "x", true),
            Err(DbError::NotFound(7))
        ));
    }
}
