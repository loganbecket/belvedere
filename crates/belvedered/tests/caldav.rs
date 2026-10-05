//! Thunderbird's view of the tasks: a scripted CalDAV client against the
//! real service, over plain TCP. Discovery, listing, fetching, changes
//! showing up, and refusals (no password, wrong password, not loopback).

mod common;

use std::time::Duration;

use belvedere_core::caldav::{base64_encode, CALENDAR, HOME, PRINCIPAL, USERNAME};
use common::{stop, wait_ready, Bus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// One HTTP request over a fresh connection; returns status and body.
async fn http(
    port: u16,
    method: &str,
    path: &str,
    auth: Option<(&str, &str)>,
    depth: Option<&str>,
    body: &str,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    if let Some((u, p)) = auth {
        req.push_str(&format!(
            "Authorization: Basic {}\r\n",
            base64_encode(&format!("{u}:{p}"))
        ));
    }
    if let Some(d) = depth {
        req.push_str(&format!("Depth: {d}\r\n"));
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

#[tokio::test]
async fn a_caldav_client_discovers_lists_fetches_and_sees_changes() {
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("belvedere.db");
    let mut cmd = bus.service_command(&db_path);
    cmd.env("BELVEDERE_CALDAV_PORT", "0");
    let service = cmd.spawn().unwrap();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;

    let (url, user, password) = loop {
        let (url, user, password) = proxy.caldav_info().await.unwrap();
        // The port is recorded once the listener is up.
        if !url.contains(":38481/") || std::env::var_os("BELVEDERE_CALDAV_PORT").is_none() {
            break (url, user, password);
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
    assert_eq!(user, USERNAME);
    assert_eq!(password.len(), 24);
    assert!(url.ends_with(CALENDAR));
    let auth = Some((USERNAME, password.as_str()));

    // Refusals.
    assert_eq!(
        http(port, "PROPFIND", "/", None, Some("0"), "").await.0,
        401
    );
    assert_eq!(
        http(
            port,
            "PROPFIND",
            "/",
            Some((USERNAME, "wrong")),
            Some("0"),
            ""
        )
        .await
        .0,
        401
    );
    assert_eq!(
        http(
            port,
            "GET",
            CALENDAR,
            Some(("someone", &password)),
            None,
            ""
        )
        .await
        .0,
        401
    );
    // Loopback only: the listener is not reachable on any other address.
    let listed = std::process::Command::new("ss")
        .args(["-ltnH"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let ours: Vec<&str> = listed
        .lines()
        .filter(|l| l.contains(&format!(":{port} ")))
        .collect();
    assert!(!ours.is_empty(), "listener not found in ss output");
    assert!(
        ours.iter().all(|l| l.contains("127.0.0.1:")),
        "listener must be bound to 127.0.0.1 only: {ours:?}"
    );

    // Discovery chain.
    let (st, _, body) = http(
        port,
        "PROPFIND",
        "/",
        auth,
        Some("0"),
        "<D:propfind xmlns:D=\"DAV:\"><D:prop><D:current-user-principal/></D:prop></D:propfind>",
    )
    .await;
    assert_eq!(st, 207);
    assert!(body.contains(PRINCIPAL));
    let (_, _, body) = http(port, "PROPFIND", PRINCIPAL, auth, Some("0"), "").await;
    assert!(body.contains(HOME));
    let (_, _, body) = http(port, "PROPFIND", HOME, auth, Some("1"), "").await;
    assert!(body.contains(CALENDAR) && body.contains("VTODO"));

    // Empty calendar, then a task appears.
    let (_, _, body) = http(port, "PROPFIND", CALENDAR, auth, Some("1"), "").await;
    assert!(body.contains("getctag"));
    let ctag_before = body.clone();
    assert!(!body.contains(".ics"));
    let task = proxy
        .create_task(
            "Pay the water bill",
            "Account 88-1092",
            "2099-11-28T14:00:00.000Z",
        )
        .await
        .unwrap();
    let (_, _, listing) = http(port, "PROPFIND", CALENDAR, auth, Some("1"), "").await;
    let href = format!("{CALENDAR}belvedere-task-{}@belvedere.ics", task.id);
    assert!(listing.contains(&href), "{listing}");
    assert_ne!(listing, ctag_before, "the calendar tag changed");

    // Fetch it both ways.
    let (st, head, ics) = http(port, "GET", &href, auth, None, "").await;
    assert_eq!(st, 200);
    assert!(head.contains("ETag:"));
    let unfolded = ics.replace("\r\n ", "");
    assert!(unfolded.contains("SUMMARY:Pay the water bill"));
    assert!(unfolded.contains("DESCRIPTION:Account 88-1092"));
    assert!(unfolded.contains("DUE:20991128T140000Z"));
    assert!(unfolded.contains("STATUS:NEEDS-ACTION"));
    let multiget = format!("<C:calendar-multiget xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:prop><D:getetag/><C:calendar-data/></D:prop><D:href>{href}</D:href></C:calendar-multiget>");
    let (st, _, report) = http(port, "REPORT", CALENDAR, auth, Some("1"), &multiget).await;
    assert_eq!(st, 207);
    assert!(report.contains("Pay the water bill"));

    // Edit, complete, delete: each shows at once.
    proxy
        .update_task(
            task.id,
            "Pay the water bill (corrected)",
            "Account 88-1092",
            "2099-11-28T14:00:00.000Z",
        )
        .await
        .unwrap();
    let (_, _, ics) = http(port, "GET", &href, auth, None, "").await;
    assert!(ics
        .replace("\r\n ", "")
        .contains("SUMMARY:Pay the water bill (corrected)"));
    proxy.complete_task(task.id).await.unwrap();
    let (_, _, ics) = http(port, "GET", &href, auth, None, "").await;
    assert!(ics.contains("STATUS:COMPLETED"));
    proxy.delete_task(task.id).await.unwrap();
    assert_eq!(http(port, "GET", &href, auth, None, "").await.0, 404);
    // Writes from the client are refused for now.
    assert_eq!(
        http(
            port,
            "PUT",
            &href,
            auth,
            None,
            "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"
        )
        .await
        .0,
        403
    );

    stop(service).await;
}
