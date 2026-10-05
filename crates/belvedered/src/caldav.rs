//! The CalDAV listener: loopback only, password protected, serving
//! Belvedere's tasks to Thunderbird. The thinking is in the shared
//! crate; this is sockets and settings.

use std::net::SocketAddr;

use belvedere_core::caldav::{self, Item, Request};
use belvedere_core::db::{Db, SourceKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

use crate::dbus::SharedDb;

/// The port tried first. Setting `caldav_port`; the port actually bound
/// is recorded in `caldav_bound_port` for the window to show.
pub const DEFAULT_PORT: u16 = 38481;

/// Most bytes accepted in one request.
const MAX_REQUEST: usize = 1 << 20;

fn lock(db: &SharedDb) -> std::sync::MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(|e| e.into_inner())
}

/// The password, made once and kept in settings.
pub fn password(db: &Db) -> String {
    if let Ok(Some(p)) = db.get_setting("caldav_password") {
        if !p.is_empty() {
            return p;
        }
    }
    let p = caldav::generate_password().unwrap_or_else(|_| "belvedere".into());
    let _ = db.set_setting("caldav_password", &p);
    p
}

/// What the window shows for the one-time Thunderbird setup.
pub fn connection_info(db: &Db) -> (String, String, String) {
    let port = db
        .get_setting("caldav_bound_port")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    (
        format!("http://127.0.0.1:{port}{}", caldav::CALENDAR),
        caldav::USERNAME.to_string(),
        password(db),
    )
}

/// Every task (not deleted) with the email it came from.
pub fn items(db: &Db) -> Vec<Item> {
    db.list_tasks()
        .unwrap_or_default()
        .into_iter()
        .map(|task| {
            let source = db
                .task_sources(task.id)
                .unwrap_or_default()
                .into_iter()
                .rfind(|s| s.kind == SourceKind::Email)
                .map(|s| s.reference);
            Item { task, source }
        })
        .collect()
}

/// Binds to the loopback address only. `BELVEDERE_CALDAV_PORT` (tests)
/// overrides the setting; 0 means any free port.
async fn bind(db: &SharedDb) -> Option<TcpListener> {
    let wanted: u16 = std::env::var("BELVEDERE_CALDAV_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| {
            lock(db)
                .get_setting("caldav_port")
                .ok()
                .flatten()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(DEFAULT_PORT);
    for port in [wanted, 0] {
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        match TcpListener::bind(addr).await {
            Ok(l) => {
                if port != wanted {
                    warn!(wanted, "port in use; the calendar is on another port");
                }
                return Some(l);
            }
            Err(err) => warn!(port, "could not bind the calendar port: {err}"),
        }
    }
    None
}

/// Runs forever: accepts connections and answers CalDAV requests.
pub async fn run(db: SharedDb) {
    let Some(listener) = bind(&db).await else {
        warn!("the Thunderbird calendar is off: no port could be bound");
        return;
    };
    let bound = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    {
        let d = lock(&db);
        let _ = d.set_setting("caldav_bound_port", &bound.to_string());
        let _ = password(&d);
    }
    info!(port = bound, "Thunderbird calendar listening on 127.0.0.1");
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(err) => {
                warn!("calendar accept failed: {err}");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
        };
        if !peer.ip().is_loopback() {
            // Cannot happen with a loopback bind, but never serve anyone else.
            continue;
        }
        let db = db.clone();
        tokio::spawn(async move {
            if let Err(err) = serve(stream, db).await {
                tracing::debug!("calendar connection ended: {err}");
            }
        });
    }
}

/// Serves one connection, request after request, until it closes.
async fn serve(mut stream: TcpStream, db: SharedDb) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    loop {
        // Read until the end of the head.
        let head_end = loop {
            if let Some(pos) = find(&buf, b"\r\n\r\n") {
                break pos;
            }
            if buf.len() > MAX_REQUEST {
                return Err(std::io::Error::other("request too large"));
            }
            let mut chunk = [0u8; 4096];
            let n =
                tokio::time::timeout(std::time::Duration::from_secs(30), stream.read(&mut chunk))
                    .await
                    .map_err(|_| std::io::Error::other("idle"))??;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut req = match Request::parse_head(&head) {
            Some(r) => r,
            None => {
                stream
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                    .await?;
                return Ok(());
            }
        };
        let length: usize = req
            .header("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if length > MAX_REQUEST {
            return Err(std::io::Error::other("body too large"));
        }
        let body_start = head_end + 4;
        while buf.len() < body_start + length {
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        req.body = String::from_utf8_lossy(&buf[body_start..body_start + length]).into_owned();
        buf.drain(..body_start + length);

        let (password, items) = {
            let d = lock(&db);
            (password(&d), items(&d))
        };
        let response = caldav::handle(&req, &password, &items);
        info!(
            method = req.method,
            path = req.path,
            status = response.status,
            "calendar request"
        );
        let close = req
            .header("connection")
            .is_some_and(|c| c.eq_ignore_ascii_case("close"));
        stream.write_all(&response.to_bytes()).await?;
        if close {
            return Ok(());
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
