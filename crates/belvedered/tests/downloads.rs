//! Model downloads against the real service, with a local stand-in for
//! Hugging Face: a small model downloads and becomes selectable, a
//! download interrupted by a service restart resumes where it was, and a
//! corrupted download is detected and not offered.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use belvedere_core::ipc::RepoFileDto;
use belvedere_core::models::gguf::fixture::{gguf, V};
use common::{stop, wait_ready, Bus};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A tiny HTTP server: `/api/models?...` and `/api/models/<repo>` answer
/// JSON; `/<repo>/resolve/main/<file>` serves bytes with Range support,
/// throttled so a download can be interrupted. Counts bytes served.
async fn fake_hub(model: Vec<u8>, throttle: Duration) -> (u16, Arc<AtomicU64>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(AtomicU64::new(0));
    let served2 = served.clone();
    let model = Arc::new(model);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let model = model.clone();
            let served = served2.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut head = Vec::new();
                loop {
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    head.extend_from_slice(&buf[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                let range = text
                    .lines()
                    .find_map(|l| l.strip_prefix("Range: bytes="))
                    .and_then(|r| r.split('-').next())
                    .and_then(|s| s.trim().parse::<u64>().ok());
                let sha: String = Sha256::digest(&model[..])
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                let (status, body, extra): (u16, Vec<u8>, String) = if path
                    .starts_with("/api/models?")
                {
                    (
                        200,
                        br#"[{"id":"test/Tiny-GGUF","downloads":5,"likes":1}]"#.to_vec(),
                        String::new(),
                    )
                } else if path.starts_with("/api/models/") {
                    let json = format!(
                        r#"{{"id":"test/Tiny-GGUF","siblings":[{{"rfilename":"tiny-Q4_K_M.gguf","size":{},"lfs":{{"sha256":"{sha}","size":{}}}}},{{"rfilename":"broken-Q4_K_M.gguf","size":{},"lfs":{{"sha256":"{}","size":{}}}}}]}}"#,
                        model.len(),
                        model.len(),
                        model.len(),
                        "0".repeat(64),
                        model.len()
                    );
                    (200, json.into_bytes(), String::new())
                } else if path.contains("/resolve/main/") {
                    let start = range.unwrap_or(0) as usize;
                    let slice = model[start.min(model.len())..].to_vec();
                    let extra = if range.is_some() {
                        format!(
                            "Content-Range: bytes {start}-{}/{}\r\n",
                            model.len() - 1,
                            model.len()
                        )
                    } else {
                        String::new()
                    };
                    (if range.is_some() { 206 } else { 200 }, slice, extra)
                } else {
                    (404, Vec::new(), String::new())
                };
                let header = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n{extra}Connection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes()).await;
                // Serve in small pieces with a pause between, so a stop
                // can land in the middle.
                for chunk in body.chunks(64 * 1024) {
                    if stream.write_all(chunk).await.is_err() {
                        return;
                    }
                    served.fetch_add(chunk.len() as u64, Ordering::SeqCst);
                    if !throttle.is_zero() {
                        tokio::time::sleep(throttle).await;
                    }
                }
            });
        }
    });
    (port, served)
}

/// A real GGUF header padded out to a few megabytes.
fn tiny_model() -> Vec<u8> {
    let mut bytes = gguf(&[
        ("general.architecture", V::Str("qwen3")),
        ("general.name", V::Str("Tiny")),
        (
            "tokenizer.chat_template",
            V::Str("{% if tools %}<tool_call>{% endif %}"),
        ),
    ]);
    bytes.resize(3 * 1024 * 1024, 0);
    bytes
}

fn file(name: &str, size: u64, sha: &str) -> RepoFileDto {
    RepoFileDto {
        name: name.into(),
        size,
        sha256: sha.into(),
        fit: "fits".into(),
        quantization: "Q4_K_M".into(),
    }
}

#[tokio::test]
async fn downloads_resume_after_a_restart_and_corrupt_files_are_refused() {
    let model = tiny_model();
    let sha: String = Sha256::digest(&model)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let (port, served) = fake_hub(model.clone(), Duration::from_millis(60)).await;
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("belvedere.db");
    let spawn = || {
        let mut cmd = bus.service_command(&db_path);
        cmd.env("XDG_DATA_HOME", dir.path())
            .env("HOME", dir.path())
            .env("BELVEDERE_HF_BASE", format!("http://127.0.0.1:{port}"));
        cmd.spawn().unwrap()
    };
    let service = spawn();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;

    // Search and file listing go through the stand-in.
    let repos = proxy.search_models("tiny").await.unwrap();
    assert_eq!(repos[0].id, "test/Tiny-GGUF");
    let files = proxy.list_repo_files("test/Tiny-GGUF").await.unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].sha256, sha);
    assert_eq!(files[0].fit, "fits");

    // Start, let it get partway, then stop the service mid-download.
    let id = proxy
        .start_download(
            "test/Tiny-GGUF",
            file("tiny-Q4_K_M.gguf", model.len() as u64, &sha),
        )
        .await
        .unwrap();
    let started = Instant::now();
    loop {
        let before = served.load(Ordering::SeqCst);
        if before >= 512 * 1024 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "download never got going"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop(service).await;
    let served_before_restart = served.load(Ordering::SeqCst);
    assert!(
        served_before_restart < model.len() as u64,
        "stopped mid-way"
    );
    let part = dir.path().join("belvedere/models/tiny-Q4_K_M.gguf.part");
    let on_disk = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    assert!(on_disk > 0, "partial file kept");

    // Restart: the download is paused, not lost; resume finishes it.
    let service = spawn();
    let proxy = wait_ready(&conn).await;
    let paused = proxy.list_downloads().await.unwrap();
    assert_eq!(paused[0].status, "paused");
    assert!(paused[0].received > 0);
    proxy.resume_download(id).await.unwrap();
    let started = Instant::now();
    loop {
        let d = proxy
            .list_downloads()
            .await
            .unwrap()
            .into_iter()
            .find(|d| d.id == id)
            .unwrap();
        if d.status == "done" {
            break;
        }
        assert_ne!(d.status, "failed", "{}", d.error);
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "download did not finish: {d:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let total_served = served.load(Ordering::SeqCst);
    assert!(
        total_served < 2 * model.len() as u64 - on_disk / 2,
        "resumed rather than restarted: served {total_served} for a {} byte file",
        model.len()
    );
    let final_path = dir.path().join("belvedere/models/tiny-Q4_K_M.gguf");
    assert_eq!(std::fs::read(&final_path).unwrap(), model);
    assert!(!part.exists());
    // It is now a model Belvedere offers, and can be chosen.
    let models = proxy.list_models().await.unwrap();
    let ours = models
        .iter()
        .find(|m| m.path == final_path.to_string_lossy())
        .expect("downloaded model listed");
    assert_eq!(ours.source, "belvedere");
    assert_eq!(ours.supports_tools, "yes");
    proxy.set_model_role("chat", ours.id).await.unwrap();
    assert_eq!(proxy.model_roles().await.unwrap().0, ours.id);

    // A file whose checksum does not match is refused and removed.
    let bad = proxy
        .start_download(
            "test/Tiny-GGUF",
            file("broken-Q4_K_M.gguf", model.len() as u64, &"0".repeat(64)),
        )
        .await
        .unwrap();
    let started = Instant::now();
    loop {
        let d = proxy
            .list_downloads()
            .await
            .unwrap()
            .into_iter()
            .find(|d| d.id == bad)
            .unwrap();
        if d.status == "failed" {
            assert!(d.error.contains("checksum"), "{}", d.error);
            break;
        }
        assert_ne!(d.status, "done", "a corrupted file must not pass");
        assert!(started.elapsed() < Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!dir
        .path()
        .join("belvedere/models/broken-Q4_K_M.gguf")
        .exists());
    assert!(!dir
        .path()
        .join("belvedere/models/broken-Q4_K_M.gguf.part")
        .exists());
    assert!(proxy
        .list_models()
        .await
        .unwrap()
        .iter()
        .all(|m| !m.path.contains("broken")));
    stop(service).await;
}
