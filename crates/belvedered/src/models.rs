//! Keeps the models table in step with what's on disk.

use belvedere_core::db::{Db, ModelSource, NewModel};
use belvedere_core::models::{self, Locations};
use tracing::{info, warn};

/// Scans the standard locations and records every chat model found.
/// Models from LM Studio or Ollama whose files have disappeared are
/// forgotten; Belvedere's own and imported ones are kept (their files are
/// ours to worry about elsewhere).
pub fn rescan(db: &Db) {
    let forgotten = forgotten_paths(db);
    let scan = models::scan(&Locations::standard());
    for skipped in &scan.skipped {
        info!(path = %skipped.path.display(), reason = %skipped.reason, "skipped while scanning for models");
    }

    let mut seen = std::collections::HashSet::new();
    for found in &scan.found {
        let path = found.path.to_string_lossy().into_owned();
        if forgotten.contains(&path) {
            continue;
        }
        seen.insert(path.clone());
        let new = NewModel {
            name: &found.name,
            path: &path,
            source: found.source,
            size_bytes: found.size_bytes as i64,
            quantization: &found.quantization(),
            supports_tools: Some(found.info.supports_tools()),
        };
        if let Err(err) = db.upsert_model(&new) {
            warn!(path, "could not record model: {err}");
        }
    }

    match db.list_models() {
        Ok(known) => {
            for model in known {
                let external = matches!(model.source, ModelSource::LmStudio | ModelSource::Ollama);
                if external && !seen.contains(&model.path) {
                    info!(name = model.name, "model file is gone; forgetting it");
                    let _ = db.delete_model(model.id);
                }
            }
        }
        Err(err) => warn!("could not list models: {err}"),
    }

    info!(
        found = scan.found.len(),
        skipped = scan.skipped.len(),
        "model scan complete"
    );
}

/// Paths the user removed from Belvedere; a rescan leaves them out.
pub fn forgotten_paths(db: &Db) -> std::collections::HashSet<String> {
    db.get_setting("forgotten_model_paths")
        .ok()
        .flatten()
        .map(|v| {
            v.lines()
                .map(str::to_string)
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Removes a model from Belvedere. The file is deleted only when
/// Belvedere downloaded it (source `belvedere`, under Belvedere's own
/// models folder); LM Studio's, Ollama's, and imported files stay. Any
/// role the model held is cleared. Returns whether a file was deleted.
pub fn forget(db: &Db, id: i64, own_models_dir: &std::path::Path) -> Result<bool, String> {
    let model = db.get_model(id).map_err(|e| e.to_string())?;
    let mut deleted_file = false;
    if model.source == ModelSource::Belvedere {
        let path = std::path::Path::new(&model.path);
        let inside = path
            .canonicalize()
            .ok()
            .zip(own_models_dir.canonicalize().ok())
            .is_some_and(|(p, d)| p.starts_with(d));
        if inside && path.is_file() {
            std::fs::remove_file(path)
                .map_err(|e| format!("could not delete the model file: {e}"))?;
            deleted_file = true;
        }
    }
    db.delete_model(id).map_err(|e| e.to_string())?;
    let mut forgotten = forgotten_paths(db);
    forgotten.insert(model.path.clone());
    let joined: Vec<String> = forgotten.into_iter().collect();
    let _ = db.set_setting("forgotten_model_paths", &joined.join("\n"));
    for key in ["chat_model_id", "background_model_id"] {
        if db.get_setting(key).ok().flatten().as_deref() == Some(id.to_string().as_str()) {
            let _ = db.delete_setting(key);
        }
    }
    info!(
        name = model.name,
        deleted_file, "model removed from Belvedere"
    );
    Ok(deleted_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(db: &Db, name: &str, path: &str, source: ModelSource) -> i64 {
        db.upsert_model(&NewModel {
            name,
            path,
            source,
            size_bytes: 10,
            quantization: "Q4",
            supports_tools: Some(true),
        })
        .unwrap()
        .id
    }

    #[test]
    fn forgetting_deletes_only_belvederes_own_downloads_and_clears_roles() {
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("belvedere/models");
        let lm = dir.path().join("lmstudio");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir_all(&lm).unwrap();
        let ours = own.join("downloaded.gguf");
        let theirs = lm.join("theirs.gguf");
        std::fs::write(&ours, b"gguf").unwrap();
        std::fs::write(&theirs, b"gguf").unwrap();
        let db = Db::open_in_memory().unwrap();
        let a = model(&db, "Ours", &ours.to_string_lossy(), ModelSource::Belvedere);
        let b = model(
            &db,
            "Theirs",
            &theirs.to_string_lossy(),
            ModelSource::LmStudio,
        );
        let c = model(
            &db,
            "Imported",
            &lm.join("imported.gguf").to_string_lossy(),
            ModelSource::Import,
        );
        db.set_setting("chat_model_id", &b.to_string()).unwrap();

        assert!(
            !forget(&db, b, &own).unwrap(),
            "LM Studio's file is not ours to delete"
        );
        assert!(theirs.is_file());
        assert!(
            db.get_setting("chat_model_id").unwrap().is_none(),
            "the role it held is cleared"
        );
        assert!(forgotten_paths(&db).contains(&theirs.to_string_lossy().into_owned()));

        assert!(forget(&db, a, &own).unwrap(), "our own download goes");
        assert!(!ours.is_file());
        assert!(!forget(&db, c, &own).unwrap());
        assert!(db.list_models().unwrap().is_empty());
    }
}
