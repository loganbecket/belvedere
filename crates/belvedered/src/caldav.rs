//! The CalDAV listener: loopback only, password protected, serving
//! Belvedere's tasks to Thunderbird. The thinking is in the shared
//! crate; this is sockets and settings.

use std::net::SocketAddr;

use belvedere_core::caldav::{self, IncomingTodo, Item, Request, Response, Write};
use belvedere_core::db::{Db, NewTask, SourceKind, TaskStatus};
use belvedere_core::ipc::OBJECT_PATH;
use belvedere_core::schedule;
use chrono::Local;
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

/// The task a resource name refers to, deleted or not.
fn task_for_name(db: &Db, name: &str) -> Option<belvedere_core::db::Task> {
    if let Some(id) = name
        .strip_prefix("belvedere-task-")
        .and_then(|r| r.strip_suffix("@belvedere"))
        .and_then(|n| n.parse::<i64>().ok())
    {
        return db.get_task(id).ok().filter(|t| t.caldav_name.is_empty());
    }
    db.task_by_caldav_name(name).ok().flatten()
}

fn status_of(task: &belvedere_core::db::Task) -> &'static str {
    match task.status {
        TaskStatus::Open => "open",
        TaskStatus::Done => "done",
        TaskStatus::Dismissed => "dismissed",
    }
}

/// One line describing a version of a task, for the history.
fn version_line(who: &str, when: &str, title: &str, due: Option<&str>, status: &str) -> String {
    let when = chrono::DateTime::parse_from_rfc3339(when)
        .map(|d| {
            d.with_timezone(&Local)
                .format("%b %-d %-I:%M %p")
                .to_string()
        })
        .unwrap_or_else(|_| "earlier".into());
    let due = due
        .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
        .map(|d| format!(", due {}", d.with_timezone(&Local).format("%b %-d")))
        .unwrap_or_default();
    format!("{who}'s version from {when}: {title} ({status}{due})")
}

/// Sets a task's status from a client's word, keeping reminders in step.
fn apply_status(
    db: &Db,
    id: i64,
    status: &str,
    due: Option<&str>,
) -> belvedere_core::db::Result<()> {
    match status {
        "done" => {
            db.complete_task(id)?;
            db.cancel_task_reminders(id)?;
        }
        "dismissed" => {
            db.dismiss_task(id, "not needed (from Thunderbird)")?;
            db.cancel_task_reminders(id)?;
        }
        _ => {
            db.reopen_task(id)?;
            let plan = due
                .map(|d| {
                    schedule::plan_reminders_with_lead(d, Local::now(), schedule::lead_days(db))
                })
                .unwrap_or_default();
            db.replace_task_reminders(id, &plan)?;
        }
    }
    Ok(())
}

fn etag_of(db: &Db, id: i64) -> Option<String> {
    db.get_task(id).ok().map(|t| {
        Item {
            task: t,
            source: None,
        }
        .etag()
    })
}

/// Carries out a write from the client. Conflicts (the client's ETag is
/// stale, so both sides changed the task) go to the more recent change;
/// the losing version is kept as a line in the task's history.
pub fn apply_write(db: &Db, write: &Write) -> Response {
    match write {
        Write::Put {
            name,
            todo,
            if_match,
        } => match task_for_name(db, name) {
            Some(task) => {
                let current = Item {
                    task: task.clone(),
                    source: None,
                }
                .etag();
                let stale = if_match.as_deref().is_some_and(|m| m != current);
                let mut notes = todo.notes.clone();
                if stale && !caldav::client_wins(todo.last_modified.as_deref(), &task.updated_at) {
                    // Belvedere's change is newer: keep it, remember theirs.
                    let line = version_line(
                        "Thunderbird",
                        todo.last_modified.as_deref().unwrap_or(""),
                        &todo.title,
                        todo.due_at.as_deref(),
                        &todo.status,
                    );
                    let kept = format!("{}\n{line}", task.notes.trim_end());
                    let _ = db.update_task(
                        task.id,
                        &NewTask {
                            title: task.title.clone(),
                            notes: kept,
                            due_at: task.due_at.clone(),
                        },
                    );
                    return caldav::written(200, etag_of(db, task.id).as_deref());
                }
                if stale {
                    // Theirs is newer: apply it, remember ours.
                    let line = version_line(
                        "Belvedere",
                        &task.updated_at,
                        &task.title,
                        task.due_at.as_deref(),
                        status_of(&task),
                    );
                    notes = format!("{}\n{line}", notes.trim_end());
                }
                if task.deleted_at.is_some() {
                    let _ = db.restore_task(task.id);
                }
                let updated = db.update_task(
                    task.id,
                    &NewTask {
                        title: todo.title.clone(),
                        notes,
                        due_at: todo.due_at.clone(),
                    },
                );
                if let Err(err) = updated {
                    return Response::new_text(500, &format!("could not update: {err}"));
                }
                if let Err(err) = apply_status(db, task.id, &todo.status, todo.due_at.as_deref()) {
                    return Response::new_text(500, &format!("could not set status: {err}"));
                }
                caldav::written(204, etag_of(db, task.id).as_deref())
            }
            None => {
                let created = db.create_task(&NewTask {
                    title: todo.title.clone(),
                    notes: todo.notes.clone(),
                    due_at: todo.due_at.clone(),
                });
                let task = match created {
                    Ok(t) => t,
                    Err(err) => {
                        return Response::new_text(500, &format!("could not create: {err}"))
                    }
                };
                let _ = db.set_task_caldav_name(task.id, name);
                let _ = db.set_task_kind(task.id, "thunderbird", "");
                if let Err(err) = apply_status(db, task.id, &todo.status, todo.due_at.as_deref()) {
                    return Response::new_text(500, &format!("could not set status: {err}"));
                }
                caldav::written(201, etag_of(db, task.id).as_deref())
            }
        },
        Write::Delete { name, .. } => match task_for_name(db, name) {
            Some(task) if task.deleted_at.is_none() => {
                // A delete is the newest thing that happened; it wins.
                let _ = db.delete_task(task.id);
                let _ = db.cancel_task_reminders(task.id);
                caldav::written(204, None)
            }
            _ => Response::new_text(404, "no such task"),
        },
    }
}

/// What a client's PUT does, for the log.
pub fn describe(todo: &IncomingTodo) -> String {
    format!("{} [{}]", todo.title, todo.status)
}

async fn announce(bus: &zbus::Connection) {
    if let Ok(iface) = bus
        .object_server()
        .interface::<_, crate::dbus::Service>(OBJECT_PATH)
        .await
    {
        let _ = crate::dbus::Service::tasks_changed(iface.signal_emitter()).await;
    }
}

/// Runs forever: accepts connections and answers CalDAV requests.
pub async fn run(db: SharedDb, bus: zbus::Connection) {
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
        let bus = bus.clone();
        tokio::spawn(async move {
            if let Err(err) = serve(stream, db, bus).await {
                tracing::debug!("calendar connection ended: {err}");
            }
        });
    }
}

/// Serves one connection, request after request, until it closes.
async fn serve(mut stream: TcpStream, db: SharedDb, bus: zbus::Connection) -> std::io::Result<()> {
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
        let mut changed = false;
        let response = match caldav::write_request(&req, &password) {
            Some(Ok(write)) => {
                let r = apply_write(&lock(&db), &write);
                changed = (200..300).contains(&r.status);
                if changed {
                    match &write {
                        Write::Put { todo, .. } => {
                            info!(what = describe(todo), "task written from Thunderbird")
                        }
                        Write::Delete { .. } => info!("task deleted from Thunderbird"),
                    }
                }
                r
            }
            Some(Err(refused)) => refused,
            None => caldav::handle(&req, &password, &items),
        };
        if changed {
            announce(&bus).await;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use belvedere_core::db::Db;

    fn todo(title: &str, status: &str, modified: Option<&str>) -> IncomingTodo {
        IncomingTodo {
            uid: "tb-1".into(),
            title: title.into(),
            notes: String::new(),
            due_at: Some("2099-11-28T14:00:00.000Z".into()),
            status: status.into(),
            last_modified: modified.map(str::to_string),
        }
    }

    fn put(name: &str, t: IncomingTodo, if_match: Option<&str>) -> Write {
        Write::Put {
            name: name.into(),
            todo: t,
            if_match: if_match.map(str::to_string),
        }
    }

    fn plain(title: &str) -> NewTask {
        NewTask {
            title: title.into(),
            notes: String::new(),
            due_at: Some("2099-11-28T14:00:00.000Z".into()),
        }
    }

    #[test]
    fn thunderbird_creates_edits_completes_and_deletes() {
        let db = Db::open_in_memory().unwrap();
        let r = apply_write(&db, &put("tb-1", todo("Buy skates", "open", None), None));
        assert_eq!(r.status, 201);
        let task = db.task_by_caldav_name("tb-1").unwrap().unwrap();
        assert_eq!(task.title, "Buy skates");
        assert_eq!(task.kind, "thunderbird");
        let planned = db.task_reminders(task.id).unwrap().len();
        assert!(planned >= 2, "reminders planned");
        let r = apply_write(
            &db,
            &put(
                "tb-1",
                todo("Buy new skates", "open", None),
                etag_of(&db, task.id).as_deref(),
            ),
        );
        assert_eq!(r.status, 204);
        assert_eq!(db.get_task(task.id).unwrap().title, "Buy new skates");
        apply_write(
            &db,
            &put("tb-1", todo("Buy new skates", "done", None), None),
        );
        let t = db.get_task(task.id).unwrap();
        assert_eq!(t.status, TaskStatus::Done);
        assert!(
            db.task_reminders(task.id).unwrap().is_empty(),
            "completing cancels reminders"
        );
        apply_write(
            &db,
            &put("tb-1", todo("Buy new skates", "open", None), None),
        );
        assert_eq!(db.get_task(task.id).unwrap().status, TaskStatus::Open);
        assert_eq!(
            db.task_reminders(task.id).unwrap().len(),
            planned,
            "reopening plans them again"
        );
        let r = apply_write(
            &db,
            &Write::Delete {
                name: "tb-1".into(),
                if_match: None,
            },
        );
        assert_eq!(r.status, 204);
        assert!(db.get_task(task.id).unwrap().deleted_at.is_some());
        assert_eq!(
            apply_write(
                &db,
                &Write::Delete {
                    name: "tb-1".into(),
                    if_match: None
                }
            )
            .status,
            404
        );
        // Belvedere's own tasks are addressed by their fixed name.
        let ours = db.create_task(&plain("Ours")).unwrap();
        let name = format!("belvedere-task-{}@belvedere", ours.id);
        apply_write(&db, &put(&name, todo("Ours, renamed", "done", None), None));
        assert_eq!(db.get_task(ours.id).unwrap().title, "Ours, renamed");
        assert_eq!(db.get_task(ours.id).unwrap().status, TaskStatus::Done);
    }

    #[test]
    fn conflicts_go_to_the_newer_change_and_keep_the_loser_in_history() {
        let db = Db::open_in_memory().unwrap();
        apply_write(&db, &put("tb-1", todo("Buy skates", "open", None), None));
        let task = db.task_by_caldav_name("tb-1").unwrap().unwrap();
        let stale = etag_of(&db, task.id).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        db.update_task(task.id, &plain("Buy skates and tape"))
            .unwrap();
        // An older Thunderbird edit with the stale tag: Belvedere wins.
        let r = apply_write(
            &db,
            &put(
                "tb-1",
                todo("Buy skates (old)", "open", Some("2020-01-01T00:00:00Z")),
                Some(&stale),
            ),
        );
        assert_eq!(r.status, 200);
        let t = db.get_task(task.id).unwrap();
        assert_eq!(t.title, "Buy skates and tape");
        assert!(
            t.notes.contains("Thunderbird's version from") && t.notes.contains("Buy skates (old)"),
            "{}",
            t.notes
        );
        // A newer Thunderbird edit with a stale tag: Thunderbird wins, ours kept.
        let stale = etag_of(&db, task.id).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        db.update_task(task.id, &plain("Belvedere's title"))
            .unwrap();
        let r = apply_write(
            &db,
            &put(
                "tb-1",
                todo("Thunderbird's title", "open", Some("2099-01-01T00:00:00Z")),
                Some(&stale),
            ),
        );
        assert_eq!(r.status, 204);
        let t = db.get_task(task.id).unwrap();
        assert_eq!(t.title, "Thunderbird's title");
        assert!(
            t.notes.contains("Belvedere's version from") && t.notes.contains("Belvedere's title")
        );
        // Belvedere deleted, Thunderbird edited afterwards: the edit restores it.
        db.delete_task(task.id).unwrap();
        apply_write(&db, &put("tb-1", todo("Back again", "open", None), None));
        let t = db.get_task(task.id).unwrap();
        assert!(t.deleted_at.is_none());
        assert_eq!(t.title, "Back again");
        // Belvedere edited, Thunderbird deleted afterwards: the delete wins.
        let stale = etag_of(&db, task.id).unwrap();
        db.update_task(task.id, &plain("Edited here")).unwrap();
        apply_write(
            &db,
            &Write::Delete {
                name: "tb-1".into(),
                if_match: Some(stale),
            },
        );
        assert!(db.get_task(task.id).unwrap().deleted_at.is_some());
    }
}
