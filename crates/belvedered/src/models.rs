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

/// Imports a GGUF file, or every GGUF file in a folder. By reference the
/// file stays where it is (source `import`); as a copy it goes into
/// Belvedere's own folder (source `belvedere`, so removing it later
/// deletes the copy, never the original). A file that is not a GGUF
/// model is refused with a plain reason.
pub fn import(
    db: &Db,
    path: &std::path::Path,
    copy: bool,
    own_models_dir: &std::path::Path,
) -> Result<Vec<belvedere_core::db::Model>, String> {
    let path = path
        .canonicalize()
        .map_err(|_| format!("there is nothing at {}", path.display()))?;
    let files: Vec<std::path::PathBuf> = if path.is_dir() {
        let mut found = Vec::new();
        collect_gguf(&path, &mut found, 0);
        if found.is_empty() {
            return Err(format!("no GGUF model files in {}", path.display()));
        }
        found.sort();
        found
    } else {
        vec![path.clone()]
    };
    let mut imported = Vec::new();
    let mut problems = Vec::new();
    for file in files {
        match import_one(db, &file, copy, own_models_dir) {
            Ok(m) => imported.push(m),
            Err(e) => problems.push(e),
        }
    }
    if imported.is_empty() {
        return Err(problems.join("; "));
    }
    if !problems.is_empty() {
        warn!("some files were not imported: {}", problems.join("; "));
    }
    Ok(imported)
}

fn collect_gguf(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>, depth: usize) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_gguf(&p, out, depth + 1);
        } else if p
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("gguf"))
        {
            out.push(p);
        }
    }
}

fn import_one(
    db: &Db,
    file: &std::path::Path,
    copy: bool,
    own_models_dir: &std::path::Path,
) -> Result<belvedere_core::db::Model, String> {
    let shown = file.display();
    if !file.is_file() {
        return Err(format!("{shown} is not a file"));
    }
    if !file
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gguf"))
    {
        return Err(format!(
            "{shown} is not a GGUF model file (the name should end in .gguf)"
        ));
    }
    let info = models::gguf::read_info(file)
        .map_err(|e| format!("{shown} is not a readable GGUF model file: {e}"))?;
    if info.is_helper() {
        return Err(format!(
            "{shown} is a helper file (a projector), not a chat model"
        ));
    }
    // Named the way the scanner names models, so a copy keeps its name
    // after a restart: the GGUF's own name, else the file name.
    let name = if info.name.trim().is_empty() {
        file.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "model".into())
    } else {
        info.name.trim().to_string()
    };
    let (final_path, source) = if copy {
        std::fs::create_dir_all(own_models_dir)
            .map_err(|e| format!("could not create the models folder: {e}"))?;
        let target = own_models_dir.join(file.file_name().unwrap_or_default());
        if target.exists() && target.canonicalize().ok() != file.canonicalize().ok() {
            return Err(format!(
                "{} already exists in Belvedere's models folder",
                target.display()
            ));
        }
        if target.canonicalize().ok() != file.canonicalize().ok() {
            std::fs::copy(file, &target).map_err(|e| format!("could not copy {shown}: {e}"))?;
        }
        (target, ModelSource::Belvedere)
    } else {
        (file.to_path_buf(), ModelSource::Import)
    };
    let size = std::fs::metadata(&final_path)
        .map(|m| m.len() as i64)
        .unwrap_or(0);
    let path_str = final_path.to_string_lossy().into_owned();
    // Re-importing something once removed brings it back.
    let mut forgotten = forgotten_paths(db);
    if forgotten.remove(&path_str) {
        let joined: Vec<String> = forgotten.into_iter().collect();
        let _ = db.set_setting("forgotten_model_paths", &joined.join("\n"));
    }
    let quantization = models::gguf::quantization_from_name(&path_str)
        .unwrap_or_else(|| info.quantization.clone());
    let model = db
        .upsert_model(&NewModel {
            name: &name,
            path: &path_str,
            source,
            size_bytes: size,
            quantization: &quantization,
            supports_tools: Some(info.supports_tools()),
        })
        .map_err(|e| e.to_string())?;
    info!(name = model.name, path = path_str, copy, "model imported");
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use belvedere_core::models::gguf::fixture::{gguf, V};

    #[test]
    fn imports_files_and_folders_by_reference_or_copy_and_refuses_non_models() {
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("belvedere/models");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("nested")).unwrap();
        let good = gguf(&[
            ("general.architecture", V::Str("qwen3")),
            ("general.name", V::Str("Good")),
        ]);
        std::fs::write(elsewhere.join("good-Q4_K_M.gguf"), &good).unwrap();
        std::fs::write(elsewhere.join("nested/second.gguf"), &good).unwrap();
        std::fs::write(elsewhere.join("notes.txt"), b"hello").unwrap();
        std::fs::write(elsewhere.join("fake.gguf"), b"not really a model").unwrap();
        let db = Db::open_in_memory().unwrap();

        // A text file and a fake are refused with a reason.
        let err = import(&db, &elsewhere.join("notes.txt"), false, &own).unwrap_err();
        assert!(err.contains("not a GGUF model file"), "{err}");
        let err = import(&db, &elsewhere.join("fake.gguf"), false, &own).unwrap_err();
        assert!(err.contains("not a readable GGUF"), "{err}");
        assert!(import(&db, &dir.path().join("missing.gguf"), false, &own).is_err());

        // By reference: the file stays put.
        let one = import(&db, &elsewhere.join("good-Q4_K_M.gguf"), false, &own).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].source, ModelSource::Import);
        assert_eq!(one[0].quantization, "Q4_K_M");
        assert!(one[0].path.starts_with(elsewhere.to_str().unwrap()));
        assert!(!own.exists());

        // A folder: every model in it (the fake is skipped), as copies.
        let many = import(&db, &elsewhere, true, &own).unwrap();
        assert_eq!(many.len(), 2, "{many:?}");
        assert!(many.iter().all(|m| m.source == ModelSource::Belvedere));
        assert!(own.join("good-Q4_K_M.gguf").is_file() && own.join("second.gguf").is_file());
        assert!(
            elsewhere.join("good-Q4_K_M.gguf").is_file(),
            "originals untouched"
        );
        assert_eq!(db.list_models().unwrap().len(), 3);

        // Removed, then imported again: it comes back.
        let id = many[1].id;
        forget(&db, id, &own).unwrap();
        assert!(!own.join("second.gguf").is_file());
        let again = import(&db, &elsewhere.join("nested/second.gguf"), true, &own).unwrap();
        assert_eq!(
            again[0].name, "Good",
            "the GGUF's own name, like the scanner uses"
        );
        assert!(!forgotten_paths(&db).contains(&again[0].path));
    }

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
