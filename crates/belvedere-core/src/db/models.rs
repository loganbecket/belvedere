//! Known model files: where they are and what they are.

use rusqlite::{params, OptionalExtension, Row};

use super::{now, Db, DbError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    LmStudio,
    Ollama,
    /// Downloaded by Belvedere; the only kind Belvedere may delete from disk.
    Belvedere,
    /// Pointed at by the user.
    Import,
}

impl ModelSource {
    fn as_str(self) -> &'static str {
        match self {
            ModelSource::LmStudio => "lmstudio",
            ModelSource::Ollama => "ollama",
            ModelSource::Belvedere => "belvedere",
            ModelSource::Import => "import",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "lmstudio" => Ok(ModelSource::LmStudio),
            "ollama" => Ok(ModelSource::Ollama),
            "belvedere" => Ok(ModelSource::Belvedere),
            "import" => Ok(ModelSource::Import),
            other => Err(rusqlite::Error::InvalidParameterName(format!(
                "unknown model source {other:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub id: i64,
    pub name: String,
    /// Absolute path to the GGUF file. Unique.
    pub path: String,
    pub source: ModelSource,
    pub size_bytes: i64,
    pub quantization: String,
    /// `None` until checked.
    pub supports_tools: Option<bool>,
    pub created_at: String,
}

impl Model {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Model {
            id: row.get("id")?,
            name: row.get("name")?,
            path: row.get("path")?,
            source: ModelSource::parse(&row.get::<_, String>("source")?)?,
            size_bytes: row.get("size_bytes")?,
            quantization: row.get("quantization")?,
            supports_tools: row.get("supports_tools")?,
            created_at: row.get("created_at")?,
        })
    }
}

/// Fields for inserting or updating a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewModel<'a> {
    pub name: &'a str,
    pub path: &'a str,
    pub source: ModelSource,
    pub size_bytes: i64,
    pub quantization: &'a str,
    pub supports_tools: Option<bool>,
}

const COLUMNS: &str =
    "id, name, path, source, size_bytes, quantization, supports_tools, created_at";

impl Db {
    /// Inserts a model, or updates the existing row for the same path, so
    /// rescans are idempotent.
    pub fn upsert_model(&self, m: &NewModel) -> Result<Model> {
        self.conn.execute(
            "INSERT INTO models (name, path, source, size_bytes, quantization, supports_tools, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (path) DO UPDATE SET
                name = excluded.name, source = excluded.source, size_bytes = excluded.size_bytes,
                quantization = excluded.quantization, supports_tools = excluded.supports_tools",
            params![m.name, m.path, m.source.as_str(), m.size_bytes, m.quantization, m.supports_tools, now()],
        )?;
        self.get_model_by_path(m.path)?
            .ok_or_else(|| DbError::NotFound(self.conn.last_insert_rowid()))
    }

    pub fn get_model(&self, id: i64) -> Result<Model> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM models WHERE id = ?1"),
                [id],
                Model::from_row,
            )
            .optional()?
            .ok_or(DbError::NotFound(id))
    }

    pub fn get_model_by_path(&self, path: &str) -> Result<Option<Model>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM models WHERE path = ?1"),
                [path],
                Model::from_row,
            )
            .optional()?)
    }

    pub fn list_models(&self) -> Result<Vec<Model>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM models ORDER BY name, id"))?;
        let rows = stmt.query_map([], Model::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Forgets a model. Never touches the file.
    pub fn delete_model(&self, id: i64) -> Result<()> {
        let changed = self
            .conn
            .execute("DELETE FROM models WHERE id = ?1", [id])?;
        if changed == 0 {
            return Err(DbError::NotFound(id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qwen() -> NewModel<'static> {
        NewModel {
            name: "Qwen3.5 9B",
            path: "/models/qwen3.5-9b-q4_k_m.gguf",
            source: ModelSource::LmStudio,
            size_bytes: 5_900_000_000,
            quantization: "Q4_K_M",
            supports_tools: None,
        }
    }

    #[test]
    fn upsert_is_idempotent_on_path() {
        let db = Db::open_in_memory().unwrap();
        let first = db.upsert_model(&qwen()).unwrap();
        let second = db
            .upsert_model(&NewModel {
                supports_tools: Some(true),
                ..qwen()
            })
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.supports_tools, Some(true));
        assert_eq!(second.created_at, first.created_at);
        assert_eq!(db.list_models().unwrap().len(), 1);
    }

    #[test]
    fn read_and_delete() {
        let db = Db::open_in_memory().unwrap();
        let m = db.upsert_model(&qwen()).unwrap();
        assert_eq!(db.get_model(m.id).unwrap(), m);
        assert_eq!(
            db.get_model_by_path(m.path.as_str()).unwrap(),
            Some(m.clone())
        );
        assert_eq!(db.get_model_by_path("/nowhere").unwrap(), None);
        assert_eq!(m.source, ModelSource::LmStudio);

        db.delete_model(m.id).unwrap();
        assert!(matches!(db.get_model(m.id), Err(DbError::NotFound(_))));
        assert!(matches!(db.delete_model(m.id), Err(DbError::NotFound(_))));
    }
}
