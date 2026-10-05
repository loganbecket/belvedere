//! Two-way sync against the real service: a scripted CalDAV client and
//! the bus take turns making random changes; at the end both sides must
//! agree on the task list. Also: completing in Thunderbird cancels
//! reminders, and conflicts resolve to the newer change.

mod common;

use std::collections::HashMap;
use std::time::Duration;

use belvedere_core::caldav::{base64_encode, CALENDAR, USERNAME};
use common::{stop, wait_ready, Bus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn http(
    port: u16,
    method: &str,
    path: &str,
    password: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\nAuthorization: Basic {}\r\n",
        body.len(),
        base64_encode(&format!("{USERNAME}:{password}"))
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

fn vtodo(uid: &str, title: &str, status: &str, modified: &str) -> String {
    format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\nBEGIN:VTODO\r\nUID:{uid}\r\nSUMMARY:{title}\r\nDUE:20991128T140000Z\r\nLAST-MODIFIED:{modified}\r\nSTATUS:{status}\r\nEND:VTODO\r\nEND:VCALENDAR\r\n")
}

/// A tiny deterministic random source, so a failure can be replayed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// What the client sees: title and status per resource name.
async fn client_view(port: u16, password: &str) -> HashMap<String, (String, String)> {
    let (_, _, body) = http(
        port,
        "REPORT",
        CALENDAR,
        password,
        &[("Depth", "1")],
        "<C:calendar-query xmlns:C=\"urn:ietf:params:xml:ns:caldav\"/>",
    )
    .await;
    let body = body
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&");
    let mut out = HashMap::new();
    for chunk in body.split("<D:response>").skip(1) {
        let href = chunk
            .split("<D:href>")
            .nth(1)
            .and_then(|h| h.split("</D:href>").next())
            .unwrap_or("");
        let name = href
            .trim_start_matches(CALENDAR)
            .trim_end_matches(".ics")
            .to_string();
        let unfolded = chunk.replace("\r\n ", "");
        let title = unfolded
            .split("SUMMARY:")
            .nth(1)
            .and_then(|s| s.split("\r\n").next())
            .unwrap_or("")
            .to_string();
        let status = unfolded
            .split("STATUS:")
            .nth(1)
            .and_then(|s| s.split("\r\n").next())
            .unwrap_or("")
            .to_string();
        out.insert(name, (title, status));
    }
    out
}

#[tokio::test]
async fn a_hundred_random_changes_on_both_sides_end_in_agreement() {
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("belvedere.db");
    let mut cmd = bus.service_command(&db_path);
    cmd.env("BELVEDERE_CALDAV_PORT", "0");
    let service = cmd.spawn().unwrap();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;
    let (url, _, password) = loop {
        let info = proxy.caldav_info().await.unwrap();
        if !info.0.contains(":38481/") {
            break info;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let port: u16 = url
        .trim_start_matches("http://127.0.0.1:")
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();

    // Completing in Thunderbird cancels reminders.
    let (st, head, _) = http(
        port,
        "PUT",
        &format!("{CALENDAR}tb-skates.ics"),
        &password,
        &[],
        &vtodo(
            "tb-skates",
            "Buy skates",
            "NEEDS-ACTION",
            "20261001T000000Z",
        ),
    )
    .await;
    assert_eq!(st, 201, "{head}");
    let created = proxy
        .list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.title == "Buy skates")
        .unwrap();
    {
        let db = belvedere_core::db::Db::open(&db_path).unwrap();
        assert!(!db.task_reminders(created.id).unwrap().is_empty());
    }
    let (st, _, _) = http(
        port,
        "PUT",
        &format!("{CALENDAR}tb-skates.ics"),
        &password,
        &[],
        &vtodo("tb-skates", "Buy skates", "COMPLETED", "20261002T000000Z"),
    )
    .await;
    assert_eq!(st, 204);
    assert_eq!(proxy.get_task(created.id).await.unwrap().status, "done");
    {
        let db = belvedere_core::db::Db::open(&db_path).unwrap();
        assert!(
            db.task_reminders(created.id).unwrap().is_empty(),
            "reminders canceled"
        );
    }

    // Random operations, split across both sides.
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut names: Vec<String> = vec!["tb-skates".into()];
    let mut n = 0;
    for step in 0..100 {
        let side_client = rng.below(2) == 0;
        let op = rng.below(4);
        if side_client {
            match op {
                0 => {
                    n += 1;
                    let name = format!("tb-{n}");
                    let (st, _, _) = http(
                        port,
                        "PUT",
                        &format!("{CALENDAR}{name}.ics"),
                        &password,
                        &[],
                        &vtodo(
                            &name,
                            &format!("Client task {n}"),
                            "NEEDS-ACTION",
                            "20261001T000000Z",
                        ),
                    )
                    .await;
                    assert!(st == 201 || st == 204, "step {step}: {st}");
                    names.push(name);
                }
                1 | 2 if !names.is_empty() => {
                    let name = names[rng.below(names.len() as u64) as usize].clone();
                    let status = if op == 1 { "NEEDS-ACTION" } else { "COMPLETED" };
                    let (st, _, _) = http(
                        port,
                        "PUT",
                        &format!("{CALENDAR}{name}.ics"),
                        &password,
                        &[],
                        &vtodo(
                            &name,
                            &format!("Edited by client {step}"),
                            status,
                            "20991231T000000Z",
                        ),
                    )
                    .await;
                    assert!(st == 201 || st == 204, "step {step}: {st}");
                }
                _ if !names.is_empty() => {
                    let i = rng.below(names.len() as u64) as usize;
                    let name = names.remove(i);
                    let (st, _, _) = http(
                        port,
                        "DELETE",
                        &format!("{CALENDAR}{name}.ics"),
                        &password,
                        &[],
                        "",
                    )
                    .await;
                    assert!(st == 204 || st == 404, "step {step}: {st}");
                }
                _ => {}
            }
        } else {
            let tasks = proxy.list_tasks().await.unwrap();
            match op {
                0 => {
                    let t = proxy
                        .create_task(
                            &format!("Belvedere task {step}"),
                            "",
                            "2099-11-28T14:00:00.000Z",
                        )
                        .await
                        .unwrap();
                    names.push(format!("belvedere-task-{}@belvedere", t.id));
                }
                1 if !tasks.is_empty() => {
                    let t = &tasks[rng.below(tasks.len() as u64) as usize];
                    proxy
                        .update_task(
                            t.id,
                            &format!("Edited by Belvedere {step}"),
                            &t.notes,
                            &t.due_at,
                        )
                        .await
                        .unwrap();
                }
                2 if !tasks.is_empty() => {
                    let t = &tasks[rng.below(tasks.len() as u64) as usize];
                    if t.status == "open" {
                        proxy.complete_task(t.id).await.unwrap();
                    } else {
                        proxy.reopen_task(t.id).await.unwrap();
                    }
                }
                _ if !tasks.is_empty() => {
                    let t = &tasks[rng.below(tasks.len() as u64) as usize];
                    proxy.delete_task(t.id).await.unwrap();
                    names.retain(|n| *n != format!("belvedere-task-{}@belvedere", t.id));
                }
                _ => {}
            }
        }
    }

    // Both sides agree.
    let ours: HashMap<String, (String, String)> = proxy
        .list_tasks()
        .await
        .unwrap()
        .into_iter()
        .map(|t| {
            let status = match t.status.as_str() {
                "done" => "COMPLETED",
                "dismissed" => "CANCELLED",
                _ => "NEEDS-ACTION",
            };
            (t.id, (t.title, status.to_string()))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|(id, v)| {
            // Names: Belvedere's fixed name, or Thunderbird's, looked up via the database.
            let db = belvedere_core::db::Db::open(&db_path).unwrap();
            let t = db.get_task(id).unwrap();
            let name = if t.caldav_name.is_empty() {
                format!("belvedere-task-{id}@belvedere")
            } else {
                t.caldav_name
            };
            (name, v)
        })
        .collect();
    let theirs = client_view(port, &password).await;
    assert_eq!(ours.len(), theirs.len(), "ours {ours:?}\ntheirs {theirs:?}");
    for (name, (title, status)) in &ours {
        let got = theirs
            .get(name)
            .unwrap_or_else(|| panic!("{name} missing on the client side"));
        assert_eq!(&got.0, title, "{name}");
        assert_eq!(&got.1, status, "{name}");
    }
    eprintln!(
        "{} tasks agree on both sides after 100 operations",
        ours.len()
    );
    stop(service).await;
}
