//! Downloading models from Hugging Face into Belvedere's own folder:
//! search, file listing, and resumable downloads with a checksum check.
//! The only network use Belvedere has besides none; every request here is
//! the result of a click.

use std::collections::HashMap;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use belvedere_core::db::{Db, Download, ModelSource, NewModel};
use belvedere_core::hf;
use belvedere_core::ipc::OBJECT_PATH;
use belvedere_core::models::{gguf, Locations};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::dbus::{Service, SharedDb};

fn lock(db: &SharedDb) -> std::sync::MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(|e| e.into_inner())
}

/// Running downloads and their stop flags.
#[derive(Default)]
pub struct Manager {
    running: Mutex<HashMap<i64, Arc<AtomicBool>>>,
}

pub type SharedManager = Arc<Manager>;

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn fetch_text(url: &str) -> Result<String, String> {
    let mut response = agent()
        .get(url)
        .call()
        .map_err(|e| format!("could not reach Hugging Face: {e}"))?;
    if response.status().as_u16() >= 400 {
        return Err(format!("Hugging Face answered {}", response.status()));
    }
    response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("could not read the answer: {e}"))
}

/// Searches Hugging Face for GGUF repositories.
pub fn search(query: &str) -> Result<Vec<hf::Repo>, String> {
    let text = fetch_text(&hf::search_url(query.trim(), 20))?;
    hf::parse_search(&text)
}

/// Lists a repository's GGUF files.
pub fn files(repo: &str) -> Result<Vec<hf::RepoFile>, String> {
    let text = fetch_text(&hf::files_url(repo.trim()))?;
    hf::parse_files(&text, hf::total_memory())
}

/// Belvedere's own models folder.
pub fn own_models_dir() -> PathBuf {
    Locations::standard()
        .belvedere
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("models"))
}

/// At startup: anything mid-download when the service last stopped is
/// paused, and its progress is read from the partial file on disk (the
/// count in the database may lag behind what was written).
pub fn reconcile(db: &Db) {
    match db.pause_stale_downloads() {
        Ok(n) if n > 0 => info!(
            n,
            "downloads interrupted last time are paused and can be resumed"
        ),
        Ok(_) => {}
        Err(err) => warn!("could not check downloads: {err}"),
    }
    for d in db.list_downloads().unwrap_or_default() {
        if d.status == "paused" || d.status == "failed" {
            let on_disk = std::fs::metadata(d.part_path())
                .map(|m| m.len() as i64)
                .unwrap_or(0);
            if on_disk != d.received {
                let _ = db.set_download_progress(d.id, on_disk, d.size);
            }
        }
    }
}

/// Records a download and starts it.
pub fn start(
    db: &SharedDb,
    manager: &SharedManager,
    bus: zbus::Connection,
    repo: &str,
    file: &hf::RepoFile,
) -> Result<Download, String> {
    let dir = own_models_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create the models folder: {e}"))?;
    let safe_name = Path::new(&file.name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or("bad file name")?;
    let path = dir.join(&safe_name);
    let download = {
        let d = lock(db);
        if let Some(existing) = d
            .download_by_path(&path.to_string_lossy())
            .map_err(|e| e.to_string())?
        {
            if existing.status == "done" {
                return Err(format!("{} is already downloaded", safe_name));
            }
            existing
        } else {
            d.create_download(
                repo,
                &file.name,
                &hf::download_url(repo, &file.name),
                &path.to_string_lossy(),
                file.size as i64,
                &file.sha256,
            )
            .map_err(|e| e.to_string())?
        }
    };
    resume(db, manager, bus, download.id)?;
    Ok(download)
}

/// Starts or continues a download on its own thread.
pub fn resume(
    db: &SharedDb,
    manager: &SharedManager,
    bus: zbus::Connection,
    id: i64,
) -> Result<(), String> {
    let download = lock(db).get_download(id).map_err(|e| e.to_string())?;
    if download.status == "done" {
        return Err("already downloaded".into());
    }
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut running = manager.running.lock().unwrap_or_else(|e| e.into_inner());
        if running.contains_key(&id) {
            return Ok(());
        }
        running.insert(id, stop.clone());
    }
    let _ = lock(db).set_download_status(id, "downloading", "");
    let db = db.clone();
    let manager = manager.clone();
    // The thread has no async runtime of its own; signals go through this.
    let handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name(format!("belvedere-download-{id}"))
        .spawn(move || {
            let outcome = run_download(&db, &download, &stop, &bus, &handle);
            manager
                .running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            match outcome {
                Ok(true) => info!(id, file = download.file, "download complete and verified"),
                Ok(false) => info!(id, "download paused"),
                Err(err) => {
                    warn!(id, "download failed: {err}");
                    let _ = lock(&db).set_download_status(id, "failed", &err);
                }
            }
            announce(&bus, &handle);
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Asks a running download to stop after the current chunk.
pub fn pause(db: &SharedDb, manager: &SharedManager, id: i64) -> Result<(), String> {
    let running = manager.running.lock().unwrap_or_else(|e| e.into_inner());
    match running.get(&id) {
        Some(flag) => {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        }
        None => lock(db)
            .set_download_status(id, "paused", "")
            .map(|_| ())
            .map_err(|e| e.to_string()),
    }
}

/// Stops a download and removes it with its partial file.
pub fn cancel(db: &SharedDb, manager: &SharedManager, id: i64) -> Result<(), String> {
    let _ = pause(db, manager, id);
    // Give the thread a moment to let go of the file.
    for _ in 0..50 {
        if !manager
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&id)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let download = lock(db).get_download(id).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(download.part_path());
    if download.status != "done" {
        let _ = std::fs::remove_file(&download.path);
    }
    lock(db).delete_download(id).map_err(|e| e.to_string())
}

fn announce(bus: &zbus::Connection, handle: &tokio::runtime::Handle) {
    let bus = bus.clone();
    handle.spawn(async move {
        if let Ok(iface) = bus
            .object_server()
            .interface::<_, Service>(OBJECT_PATH)
            .await
        {
            let _ = Service::downloads_changed(iface.signal_emitter()).await;
            let _ = Service::models_changed(iface.signal_emitter()).await;
        }
    });
}

/// The download itself. Returns Ok(true) when complete and verified,
/// Ok(false) when paused.
fn run_download(
    db: &SharedDb,
    d: &Download,
    stop: &AtomicBool,
    bus: &zbus::Connection,
    handle: &tokio::runtime::Handle,
) -> Result<bool, String> {
    let part = PathBuf::from(d.part_path());
    let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part)
        .map_err(|e| format!("could not open {}: {e}", part.display()))?;
    file.seek(std::io::SeekFrom::End(0))
        .map_err(|e| e.to_string())?;

    let mut request = agent().get(&d.url);
    if have > 0 {
        request = request.header("Range", &format!("bytes={have}-"));
    }
    let mut response = request
        .call()
        .map_err(|e| format!("could not connect: {e}"))?;
    let status = response.status().as_u16();
    let mut received = have;
    if have > 0 && status == 200 {
        // The server ignored the range: start over.
        drop(file);
        file = std::fs::File::create(&part).map_err(|e| e.to_string())?;
        received = 0;
    } else if status >= 400 {
        return Err(format!("the server answered {status}"));
    } else if have > 0 && status != 206 {
        return Err(format!("the server answered {status} to a resume request"));
    }
    let total: u64 = match response
        .headers()
        .get("Content-Range")
        .and_then(|v| v.to_str().ok())
    {
        Some(cr) => cr
            .rsplit('/')
            .next()
            .and_then(|t| t.parse().ok())
            .unwrap_or(d.size as u64),
        None => response
            .headers()
            .get("Content-Length")
            .and_then(|v| v.to_str().ok())
            .and_then(|l| l.parse::<u64>().ok())
            .map(|l| l + if status == 206 { have } else { 0 })
            .unwrap_or(d.size as u64),
    };
    let _ = lock(db).set_download_progress(d.id, received as i64, total as i64);

    let mut reader = response.body_mut().as_reader();
    let mut buf = vec![0u8; 256 * 1024];
    let mut since_update = 0u64;
    loop {
        if stop.load(Ordering::SeqCst) {
            file.flush().map_err(|e| e.to_string())?;
            let _ = lock(db).set_download_progress(d.id, received as i64, total as i64);
            let _ = lock(db).set_download_status(d.id, "paused", "");
            return Ok(false);
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("read failed: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("write failed: {e}"))?;
        received += n as u64;
        since_update += n as u64;
        if since_update >= 1024 * 1024 {
            since_update = 0;
            let _ = lock(db).set_download_progress(d.id, received as i64, total as i64);
            announce(bus, handle);
        }
    }
    file.flush().map_err(|e| e.to_string())?;
    drop(file);
    let _ = lock(db).set_download_progress(d.id, received as i64, total as i64);
    if total > 0 && received != total {
        return Err(format!(
            "the connection ended early ({received} of {total} bytes); resume to continue"
        ));
    }

    // Verify before offering it: checksum when known, and a readable GGUF.
    let _ = lock(db).set_download_status(d.id, "verifying", "");
    announce(bus, handle);
    if !d.sha256.is_empty() {
        let actual = sha256_of(&part)?;
        if actual != d.sha256 {
            let _ = std::fs::remove_file(&part);
            let _ = lock(db).set_download_progress(d.id, 0, total as i64);
            return Err(
                "checksum mismatch: the file was corrupted in transit and has been removed".into(),
            );
        }
    }
    let info = gguf::read_info(&part).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        format!("not a usable model file ({e}); removed")
    })?;
    std::fs::rename(&part, &d.path).map_err(|e| format!("could not finish the file: {e}"))?;
    let size = std::fs::metadata(&d.path)
        .map(|m| m.len())
        .unwrap_or(received);
    {
        let dbl = lock(db);
        let name = Path::new(&d.path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| d.file.clone());
        let quantization = gguf::quantization_from_name(&d.file).unwrap_or_default();
        let _ = dbl.upsert_model(&NewModel {
            name: &name,
            path: &d.path,
            source: ModelSource::Belvedere,
            size_bytes: size as i64,
            quantization: &quantization,
            supports_tools: Some(info.supports_tools()),
        });
        let _ = dbl.set_download_status(d.id, "done", "");
    }
    Ok(true)
}

fn sha256_of(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
