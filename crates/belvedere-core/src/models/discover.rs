//! Finds model files already on this machine: LM Studio's folder, Ollama's
//! blob store, and Belvedere's own downloads. Read-only.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::gguf::{self, GgufInfo};
use crate::db::ModelSource;

/// A model file found on disk, with what its metadata says.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// A readable name: GGUF `general.name`, Ollama's `model:tag`, or the
    /// file name without extension.
    pub name: String,
    pub path: PathBuf,
    pub source: ModelSource,
    pub size_bytes: u64,
    pub info: GgufInfo,
}

impl Found {
    /// The quantization to show: from metadata, else from the file name.
    pub fn quantization(&self) -> String {
        if !self.info.quantization.is_empty() {
            return self.info.quantization.clone();
        }
        self.path
            .file_name()
            .and_then(|f| f.to_str())
            .and_then(gguf::quantization_from_name)
            .unwrap_or_default()
    }
}

/// Where to look. Each location is optional; missing ones are skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locations {
    pub lm_studio: Vec<PathBuf>,
    pub ollama: Vec<PathBuf>,
    pub belvedere: Vec<PathBuf>,
}

impl Locations {
    /// The usual places on a Pop!_OS / COSMIC machine.
    pub fn standard() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let h = |rel: &str| home.as_ref().map(|h| h.join(rel));
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| h(".local/share"));
        Locations {
            lm_studio: [
                h(".var/app/ai.lmstudio.lm-studio/.lmstudio/models"),
                h(".lmstudio/models"),
                h(".cache/lm-studio/models"),
            ]
            .into_iter()
            .flatten()
            .collect(),
            ollama: [
                h(".ollama/models"),
                Some(PathBuf::from("/usr/share/ollama/.ollama/models")),
                Some(PathBuf::from("/var/lib/ollama/.ollama/models")),
            ]
            .into_iter()
            .flatten()
            .collect(),
            belvedere: data_home
                .map(|d| vec![d.join("belvedere").join("models")])
                .unwrap_or_default(),
        }
    }
}

/// Something that went wrong with one location or file; the scan goes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub found: Vec<Found>,
    pub skipped: Vec<Skipped>,
}

/// Scans every location. Never fails as a whole: problems are reported
/// in `skipped`.
pub fn scan(locations: &Locations) -> Scan {
    let mut scan = Scan::default();
    for dir in &locations.lm_studio {
        scan_gguf_tree(dir, ModelSource::LmStudio, &mut scan);
    }
    for dir in &locations.belvedere {
        scan_gguf_tree(dir, ModelSource::Belvedere, &mut scan);
    }
    for dir in &locations.ollama {
        scan_ollama(dir, &mut scan);
    }
    scan.found.sort_by(|a, b| a.name.cmp(&b.name));
    scan
}

/// Every `.gguf` under `dir`, recursively. Helper files (projectors) are
/// recorded as skipped so the caller can see them without listing them.
fn scan_gguf_tree(dir: &Path, source: ModelSource, scan: &mut Scan) {
    if !dir.is_dir() {
        scan.skipped.push(Skipped {
            path: dir.to_path_buf(),
            reason: "not present".into(),
        });
        return;
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(err) => {
                scan.skipped.push(Skipped {
                    path: d,
                    reason: err.to_string(),
                });
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("gguf"))
            {
                inspect(path, None, source, scan);
            }
        }
    }
}

/// Reads one file's metadata and files it as found or skipped.
fn inspect(path: PathBuf, name: Option<String>, source: ModelSource, scan: &mut Scan) {
    let size_bytes = match std::fs::metadata(&path) {
        Ok(m) => m.len(),
        Err(err) => {
            scan.skipped.push(Skipped {
                path,
                reason: err.to_string(),
            });
            return;
        }
    };
    let info = match gguf::read_info(&path) {
        Ok(info) => info,
        Err(err) => {
            scan.skipped.push(Skipped {
                path,
                reason: err.to_string(),
            });
            return;
        }
    };
    if info.is_helper() {
        scan.skipped.push(Skipped {
            path,
            reason: "helper file, not a chat model".into(),
        });
        return;
    }
    let name = name
        .or_else(|| (!info.name.is_empty()).then(|| info.name.clone()))
        .or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "model".into());
    scan.found.push(Found {
        name,
        path,
        source,
        size_bytes,
        info,
    });
}

/// An Ollama manifest: the list of layers that make up one model.
#[derive(Debug, Deserialize)]
struct Manifest {
    layers: Vec<Layer>,
}

#[derive(Debug, Deserialize)]
struct Layer {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
}

const OLLAMA_MODEL_LAYER: &str = "application/vnd.ollama.image.model";

/// Ollama keeps `manifests/<registry>/<namespace>/<model>/<tag>` JSON files
/// pointing at content-addressed blobs. The model weights blob is a GGUF.
fn scan_ollama(dir: &Path, scan: &mut Scan) {
    let manifests = dir.join("manifests");
    if !manifests.is_dir() {
        scan.skipped.push(Skipped {
            path: dir.to_path_buf(),
            reason: "not present".into(),
        });
        return;
    }
    for (manifest_path, name) in ollama_manifests(&manifests, scan) {
        let text = match std::fs::read_to_string(&manifest_path) {
            Ok(t) => t,
            Err(err) => {
                scan.skipped.push(Skipped {
                    path: manifest_path,
                    reason: err.to_string(),
                });
                continue;
            }
        };
        match ollama_blob_path(dir, &text) {
            Ok(blob) => inspect(blob, Some(name), ModelSource::Ollama, scan),
            Err(reason) => scan.skipped.push(Skipped {
                path: manifest_path,
                reason,
            }),
        }
    }
}

/// Walks `manifests/<registry>/<namespace>/<model>/<tag>` and yields each
/// manifest with its name: `model:tag`, or `namespace/model:tag` for
/// anything outside the default `library` namespace.
fn ollama_manifests(manifests: &Path, scan: &mut Scan) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let registries = match std::fs::read_dir(manifests) {
        Ok(r) => r,
        Err(err) => {
            scan.skipped.push(Skipped {
                path: manifests.to_path_buf(),
                reason: err.to_string(),
            });
            return out;
        }
    };
    for registry in registries.flatten().filter(|e| e.path().is_dir()) {
        for namespace in dirs_in(&registry.path()) {
            let ns_name = file_name(&namespace);
            for model in dirs_in(&namespace) {
                let model_name = file_name(&model);
                for tag in std::fs::read_dir(&model)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.path().is_file())
                {
                    let tag_name = file_name(&tag.path());
                    let name = if ns_name == "library" {
                        format!("{model_name}:{tag_name}")
                    } else {
                        format!("{ns_name}/{model_name}:{tag_name}")
                    };
                    out.push((tag.path(), name));
                }
            }
        }
    }
    out
}

fn dirs_in(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

/// From a manifest's JSON, the path of its model-weights blob.
pub fn ollama_blob_path(models_dir: &Path, manifest_json: &str) -> Result<PathBuf, String> {
    let manifest: Manifest =
        serde_json::from_str(manifest_json).map_err(|e| format!("bad manifest: {e}"))?;
    let layer = manifest
        .layers
        .iter()
        .find(|l| l.media_type == OLLAMA_MODEL_LAYER)
        .ok_or_else(|| "manifest has no model layer".to_string())?;
    // "sha256:abc..." is stored as "sha256-abc...".
    let file = layer.digest.replacen(':', "-", 1);
    Ok(models_dir.join("blobs").join(file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::gguf::fixture::{gguf, V};

    fn model_bytes(name: &str, arch: &str) -> Vec<u8> {
        gguf(&[
            ("general.architecture", V::Str(arch)),
            ("general.type", V::Str("model")),
            ("general.name", V::Str(name)),
            ("general.file_type", V::U32(15)),
            (
                "tokenizer.chat_template",
                V::Str("{% if tools %}{% endif %}"),
            ),
        ])
    }

    fn projector_bytes() -> Vec<u8> {
        gguf(&[
            ("general.architecture", V::Str("clip")),
            ("general.type", V::Str("mmproj")),
        ])
    }

    #[test]
    fn lm_studio_tree_lists_models_and_skips_projectors() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("publisher").join("Some-Model-GGUF");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join("Some-Model-Q4_K_M.gguf"),
            model_bytes("Some Model", "llama"),
        )
        .unwrap();
        std::fs::write(repo.join("mmproj-F16.gguf"), projector_bytes()).unwrap();
        std::fs::write(repo.join("README.md"), "not a model").unwrap();

        let scan = scan(&Locations {
            lm_studio: vec![dir.path().to_path_buf()],
            ollama: vec![],
            belvedere: vec![],
        });
        assert_eq!(scan.found.len(), 1);
        let m = &scan.found[0];
        assert_eq!(m.name, "Some Model");
        assert_eq!(m.source, ModelSource::LmStudio);
        assert_eq!(m.quantization(), "Q4_K_M");
        assert!(m.info.supports_tools());
        assert!(scan
            .skipped
            .iter()
            .any(|s| s.path.ends_with("mmproj-F16.gguf") && s.reason.contains("helper")));
    }

    #[test]
    fn ollama_manifests_give_real_names_and_blob_paths() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir
            .path()
            .join("manifests/registry.ollama.ai/library/qwen2.5");
        std::fs::create_dir_all(&lib).unwrap();
        let other = dir
            .path()
            .join("manifests/registry.ollama.ai/someone/custom");
        std::fs::create_dir_all(&other).unwrap();
        let blobs = dir.path().join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();

        let digest = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        let manifest = format!(
            r#"{{"schemaVersion":2,"layers":[
                {{"mediaType":"application/vnd.ollama.image.system","digest":"sha256:aaaa","size":10}},
                {{"mediaType":"application/vnd.ollama.image.model","digest":"{digest}","size":123}}
            ]}}"#
        );
        std::fs::write(lib.join("7b"), &manifest).unwrap();
        std::fs::write(other.join("latest"), &manifest).unwrap();
        std::fs::write(
            blobs.join(digest.replacen(':', "-", 1)),
            model_bytes("ignored: ollama name wins", "qwen2"),
        )
        .unwrap();

        let scan = scan(&Locations {
            lm_studio: vec![],
            ollama: vec![dir.path().to_path_buf()],
            belvedere: vec![],
        });
        let names: Vec<_> = scan.found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["qwen2.5:7b", "someone/custom:latest"]);
        assert!(scan.found.iter().all(|f| f.source == ModelSource::Ollama));
        assert!(scan.found[0].path.ends_with(digest.replacen(':', "-", 1)));
    }

    #[test]
    fn manifest_without_a_model_layer_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir
            .path()
            .join("manifests/registry.ollama.ai/library/broken");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("latest"), r#"{"schemaVersion":2,"layers":[]}"#).unwrap();
        std::fs::write(lib.join("garbage"), "not json").unwrap();

        let scan = scan(&Locations {
            lm_studio: vec![],
            ollama: vec![dir.path().to_path_buf()],
            belvedere: vec![],
        });
        assert!(scan.found.is_empty());
        assert_eq!(scan.skipped.len(), 2);
    }

    #[test]
    fn missing_and_unreadable_locations_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.gguf");
        std::fs::write(&bad, b"not a gguf at all").unwrap();

        let scan = scan(&Locations {
            lm_studio: vec![dir.path().to_path_buf()],
            ollama: vec![PathBuf::from("/definitely/not/here")],
            belvedere: vec![PathBuf::from("/also/not/here")],
        });
        assert!(scan.found.is_empty());
        let reasons: Vec<_> = scan.skipped.iter().map(|s| s.reason.as_str()).collect();
        assert!(reasons.contains(&"not present"));
        assert!(reasons.iter().any(|r| r.contains("not a GGUF")));
    }

    #[test]
    fn standard_locations_cover_flatpak_lm_studio_and_system_ollama() {
        let locations = Locations::standard();
        assert!(locations
            .lm_studio
            .iter()
            .any(|p| p.ends_with(".var/app/ai.lmstudio.lm-studio/.lmstudio/models")));
        assert!(locations
            .ollama
            .iter()
            .any(|p| p == Path::new("/usr/share/ollama/.ollama/models")));
        assert!(locations
            .belvedere
            .iter()
            .any(|p| p.ends_with("belvedere/models")));
    }
}
