//! Keeps the models table in step with what's on disk.

use belvedere_core::db::{Db, ModelSource, NewModel};
use belvedere_core::models::{self, Locations};
use tracing::{info, warn};

/// Scans the standard locations and records every chat model found.
/// Models from LM Studio or Ollama whose files have disappeared are
/// forgotten; Belvedere's own and imported ones are kept (their files are
/// ours to worry about elsewhere).
pub fn rescan(db: &Db) {
    let scan = models::scan(&Locations::standard());
    for skipped in &scan.skipped {
        info!(path = %skipped.path.display(), reason = %skipped.reason, "skipped while scanning for models");
    }

    let mut seen = std::collections::HashSet::new();
    for found in &scan.found {
        let path = found.path.to_string_lossy().into_owned();
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
