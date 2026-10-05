//! Model downloads in progress or finished, so a download can be paused,
//! resumed, and picked up again after a restart.

use rusqlite::{params, OptionalExtension, Row};

use super::{now, Db, DbError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub id: i64,
    pub repo: String,
    pub file: String,
    pub url: String,
    /// Where the finished file goes; the partial file is `<path>.part`.
    pub path: String,
    pub size: i64,
    pub sha256: String,
    pub received: i64,
    /// `queued`, `downloading`, `paused`, `verifying`, `done`, `failed`.
    pub status: String,
    pub error: String,
    pub created_at: String,
    pub updated_at: String,
}

impl Download {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Download {
            id: row.get("id")?,
            repo: row.get("repo")?,
            file: row.get("file")?,
            url: row.get("url")?,
            path: row.get("path")?,
            size: row.get("size")?,
            sha256: row.get("sha256")?,
            received: row.get("received")?,
            status: row.get("status")?,
            error: row.get("error")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    pub fn part_path(&self) -> String {
        format!("{}.part", self.path)
    }
}

const COLUMNS: &str =
    "id, repo, file, url, path, size, sha256, received, status, error, created_at, updated_at";

impl Db {
    pub fn create_download(
        &self,
        repo: &str,
        file: &str,
        url: &str,
        path: &str,
        size: i64,
        sha256: &str,
    ) -> Result<Download> {
        let ts = now();
        self.conn.execute(
            "INSERT INTO downloads (repo, file, url, path, size, sha256, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![repo, file, url, path, size, sha256, ts],
        )?;
        self.get_download(self.conn.last_insert_rowid())
    }

    pub fn get_download(&self, id: i64) -> Result<Download> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM downloads WHERE id = ?1"),
                [id],
                Download::from_row,
            )
            .optional()?
            .ok_or(DbError::NotFound(id))
    }

    pub fn list_downloads(&self) -> Result<Vec<Download>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM downloads ORDER BY id DESC"))?;
        let rows = stmt.query_map([], Download::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn download_by_path(&self, path: &str) -> Result<Option<Download>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM downloads WHERE path = ?1"),
                [path],
                Download::from_row,
            )
            .optional()?)
    }

    pub fn set_download_progress(&self, id: i64, received: i64, size: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE downloads SET received = ?2, size = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, received, size, now()],
        )?;
        Ok(())
    }

    pub fn set_download_status(&self, id: i64, status: &str, error: &str) -> Result<Download> {
        let changed = self.conn.execute(
            "UPDATE downloads SET status = ?2, error = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, status, error, now()],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        self.get_download(id)
    }

    pub fn delete_download(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM downloads WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Anything that was mid-download when the service last stopped is
    /// paused now; the partial file is kept for resuming.
    pub fn pause_stale_downloads(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE downloads SET status = 'paused', updated_at = ?1 WHERE status IN ('downloading', 'queued', 'verifying')",
            [now()],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloads_round_trip_and_stale_ones_pause() {
        let db = Db::open_in_memory().unwrap();
        let d = db
            .create_download(
                "org/Repo-GGUF",
                "m.gguf",
                "https://x/m.gguf",
                "/models/m.gguf",
                1000,
                "abc",
            )
            .unwrap();
        assert_eq!(d.status, "queued");
        assert_eq!(d.part_path(), "/models/m.gguf.part");
        db.set_download_status(d.id, "downloading", "").unwrap();
        db.set_download_progress(d.id, 400, 1000).unwrap();
        assert_eq!(db.get_download(d.id).unwrap().received, 400);
        assert_eq!(db.pause_stale_downloads().unwrap(), 1);
        assert_eq!(db.get_download(d.id).unwrap().status, "paused");
        assert_eq!(
            db.download_by_path("/models/m.gguf").unwrap().unwrap().id,
            d.id
        );
        assert!(db.set_download_status(99, "done", "").is_err());
        db.delete_download(d.id).unwrap();
        assert!(db.list_downloads().unwrap().is_empty());
    }
}
