//! A small CalDAV server's brain: Belvedere's tasks as a standard
//! calendar of VTODO items, and the handling of the few requests a
//! calendar client makes to discover, list, and fetch them. Pure: it
//! turns a request plus the current tasks into a response, and the
//! service does the listening. Writes are refused in this version; two-way
//! sync comes later.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::db::{Task, TaskStatus};

/// The URL layout. One principal, one calendar home, one calendar.
pub const PRINCIPAL: &str = "/principals/belvedere/";
pub const HOME: &str = "/calendars/belvedere/";
pub const CALENDAR: &str = "/calendars/belvedere/tasks/";
pub const USERNAME: &str = "belvedere";
pub const CALENDAR_NAME: &str = "Belvedere";

/// A task as the calendar shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub task: Task,
    /// Message-ID of the email it came from, if any.
    pub source: Option<String>,
}

impl Item {
    pub fn uid(&self) -> String {
        format!("belvedere-task-{}@belvedere", self.task.id)
    }

    pub fn href(&self) -> String {
        format!("{CALENDAR}{}.ics", self.uid())
    }

    /// Changes whenever the task does.
    pub fn etag(&self) -> String {
        format!("\"{}-{}\"", self.task.id, hex(&self.task.updated_at))
    }
}

fn hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(s.as_bytes());
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// A tag for the whole calendar: changes whenever any task changes or
/// the set of tasks does.
pub fn ctag(items: &[Item]) -> String {
    let mut ids: Vec<String> = items
        .iter()
        .map(|i| format!("{}:{}", i.task.id, i.task.updated_at))
        .collect();
    ids.sort();
    format!("\"{}\"", hex(&ids.join("|")))
}

fn ical_time(rfc3339: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|d| d.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ").to_string())
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace('\n', "\\n")
}

/// Folds a content line at 75 octets, as iCalendar asks.
fn fold(line: &str) -> String {
    let mut out = String::new();
    let mut count = 0;
    for c in line.chars() {
        let w = c.len_utf8();
        if count + w > 74 {
            out.push_str("\r\n ");
            count = 1;
        }
        out.push(c);
        count += w;
    }
    out
}

/// The iCalendar text of one task.
pub fn vtodo(item: &Item) -> String {
    let t = &item.task;
    let mut lines: Vec<String> = vec![
        "BEGIN:VCALENDAR".into(),
        "VERSION:2.0".into(),
        "PRODID:-//Belvedere//Tasks//EN".into(),
        "BEGIN:VTODO".into(),
        format!("UID:{}", item.uid()),
        format!(
            "DTSTAMP:{}",
            ical_time(&t.updated_at)
                .unwrap_or_else(|| Utc::now().format("%Y%m%dT%H%M%SZ").to_string())
        ),
        format!("SUMMARY:{}", escape(&t.title)),
    ];
    if let Some(c) = ical_time(&t.created_at) {
        lines.push(format!("CREATED:{c}"));
    }
    if let Some(m) = ical_time(&t.updated_at) {
        lines.push(format!("LAST-MODIFIED:{m}"));
    }
    if !t.notes.trim().is_empty() {
        lines.push(format!("DESCRIPTION:{}", escape(t.notes.trim())));
    }
    if let Some(due) = t.due_at.as_deref().and_then(ical_time) {
        lines.push(format!("DUE:{due}"));
    }
    match t.status {
        TaskStatus::Open => {
            lines.push("STATUS:NEEDS-ACTION".into());
            lines.push("PERCENT-COMPLETE:0".into());
        }
        TaskStatus::Done => {
            lines.push("STATUS:COMPLETED".into());
            lines.push("PERCENT-COMPLETE:100".into());
            if let Some(c) = t.completed_at.as_deref().and_then(ical_time) {
                lines.push(format!("COMPLETED:{c}"));
            }
        }
        TaskStatus::Dismissed => {
            lines.push("STATUS:CANCELLED".into());
            if let Some(r) = &t.dismiss_reason {
                lines.push(format!("X-BELVEDERE-DISMISS-REASON:{}", escape(r)));
            }
        }
    }
    if !t.kind.is_empty() {
        lines.push(format!("CATEGORIES:{}", escape(&t.kind)));
    }
    if let Some(mid) = &item.source {
        lines.push(format!("URL:mid:{}", mid.trim_matches(['<', '>'])));
        lines.push(format!("X-BELVEDERE-SOURCE:{}", escape(mid)));
    }
    lines.push(format!("X-BELVEDERE-TASK-ID:{}", t.id));
    lines.push("END:VTODO".into());
    lines.push("END:VCALENDAR".into());
    lines
        .iter()
        .map(|l| fold(l))
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n"
}

/// A parsed HTTP request: just what a CalDAV client sends.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Parses the head of a request (everything before the blank line);
    /// the body is added by the caller once Content-Length is known.
    pub fn parse_head(head: &str) -> Option<Request> {
        let mut lines = head.split("\r\n");
        let first = lines.next()?;
        let mut parts = first.split_whitespace();
        let method = parts.next()?.to_string();
        let target = parts.next()?;
        // Strip any scheme and host a client might send.
        let path = if let Some(idx) = target.find("://") {
            let rest = &target[idx + 3..];
            rest.find('/').map(|i| &rest[i..]).unwrap_or("/")
        } else {
            target
        };
        let path = path.split('?').next().unwrap_or("/").to_string();
        let mut headers = HashMap::new();
        for l in lines {
            if let Some((k, v)) = l.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        Some(Request {
            method,
            path,
            headers,
            body: String::new(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    fn new(status: u16, body: String, content_type: &str) -> Self {
        let mut headers = vec![
            ("DAV".to_string(), "1, 3, calendar-access".to_string()),
            ("Content-Length".to_string(), body.len().to_string()),
        ];
        if !body.is_empty() {
            headers.push(("Content-Type".to_string(), content_type.to_string()));
        }
        Response {
            status,
            headers,
            body,
        }
    }

    fn xml(status: u16, body: String) -> Self {
        Self::new(status, body, "application/xml; charset=utf-8")
    }

    fn empty(status: u16) -> Self {
        Self::new(status, String::new(), "text/plain")
    }

    pub fn reason(&self) -> &'static str {
        match self.status {
            200 => "OK",
            207 => "Multi-Status",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            _ => "OK",
        }
    }

    /// The bytes on the wire.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {} {}\r\n", self.status, self.reason());
        for (k, v) in &self.headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(self.body.as_bytes());
        bytes
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Whether the request carries the right Basic credentials.
pub fn authorized(req: &Request, password: &str) -> bool {
    let Some(auth) = req.header("authorization") else {
        return false;
    };
    let Some(encoded) = auth.strip_prefix("Basic ") else {
        return false;
    };
    let Some(decoded) = base64_decode(encoded.trim()) else {
        return false;
    };
    let Some((user, pass)) = decoded.split_once(':') else {
        return false;
    };
    user == USERNAME && constant_time_eq(pass, password)
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

fn base64_decode(s: &str) -> Option<String> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits: u32 = 0;
    let mut n = 0;
    let mut out = Vec::new();
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = TABLE.iter().position(|t| *t == c)? as u32;
        bits = (bits << 6) | v;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push(((bits >> n) & 0xff) as u8);
        }
    }
    String::from_utf8(out).ok()
}

pub fn base64_encode(s: &str) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = s.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn depth(req: &Request) -> u32 {
    match req.header("depth") {
        Some("1") => 1,
        Some("infinity") => 1,
        _ => 0,
    }
}

/// A `<D:response>` for one href with the given found props.
fn response_xml(href: &str, props: &str) -> String {
    format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop>{props}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
        xml_escape(href)
    )
}

const MULTISTATUS_OPEN: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:CS=\"http://calendarserver.org/ns/\">";
const MULTISTATUS_CLOSE: &str = "</D:multistatus>";

fn principal_props() -> String {
    format!(
        "<D:resourcetype><D:collection/><D:principal/></D:resourcetype><D:displayname>{CALENDAR_NAME}</D:displayname><D:current-user-principal><D:href>{PRINCIPAL}</D:href></D:current-user-principal><D:principal-URL><D:href>{PRINCIPAL}</D:href></D:principal-URL><C:calendar-home-set><D:href>{HOME}</D:href></C:calendar-home-set><C:calendar-user-address-set><D:href>mailto:belvedere@localhost</D:href></C:calendar-user-address-set>"
    )
}

fn home_props() -> String {
    format!(
        "<D:resourcetype><D:collection/></D:resourcetype><D:displayname>Belvedere calendars</D:displayname><D:current-user-principal><D:href>{PRINCIPAL}</D:href></D:current-user-principal><D:owner><D:href>{PRINCIPAL}</D:href></D:owner>"
    )
}

fn calendar_props(items: &[Item]) -> String {
    format!(
        "<D:resourcetype><D:collection/><C:calendar/></D:resourcetype><D:displayname>{CALENDAR_NAME}</D:displayname><C:calendar-description>Belvedere's tasks</C:calendar-description><C:supported-calendar-component-set><C:comp name=\"VTODO\"/></C:supported-calendar-component-set><CS:getctag>{ctag}</CS:getctag><D:sync-token>belvedere-sync-{token}</D:sync-token><D:current-user-principal><D:href>{PRINCIPAL}</D:href></D:current-user-principal><D:owner><D:href>{PRINCIPAL}</D:href></D:owner><D:current-user-privilege-set><D:privilege><D:read/></D:privilege></D:current-user-privilege-set><D:getcontenttype>text/calendar</D:getcontenttype>",
        ctag = xml_escape(&ctag(items)),
        token = ctag(items).trim_matches('"')
    )
}

fn item_props(item: &Item, with_data: bool) -> String {
    let data = if with_data {
        format!(
            "<C:calendar-data>{}</C:calendar-data>",
            xml_escape(&vtodo(item))
        )
    } else {
        String::new()
    };
    format!(
        "<D:resourcetype/><D:getetag>{}</D:getetag><D:getcontenttype>text/calendar; charset=utf-8; component=VTODO</D:getcontenttype>{data}",
        xml_escape(&item.etag())
    )
}

/// The item a path names, if any.
fn item_at<'a>(path: &str, items: &'a [Item]) -> Option<&'a Item> {
    let name = path.strip_prefix(CALENDAR)?;
    let uid = name.strip_suffix(".ics")?;
    items.iter().find(|i| i.uid() == uid)
}

/// Hrefs named in a REPORT body.
fn hrefs_in(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("href") {
        let after = &rest[start..];
        let Some(gt) = after.find('>') else { break };
        let content = &after[gt + 1..];
        let Some(end) = content.find('<') else { break };
        let href = content[..end].trim();
        if !href.is_empty() {
            out.push(href.replace("&amp;", "&"));
        }
        rest = &content[end..];
    }
    out
}

/// Answers one request. Everything except OPTIONS needs the password.
pub fn handle(req: &Request, password: &str, items: &[Item]) -> Response {
    let method = req.method.to_ascii_uppercase();
    if method == "OPTIONS" {
        let mut r = Response::empty(200);
        r.headers.push((
            "Allow".into(),
            "OPTIONS, GET, HEAD, PROPFIND, REPORT".into(),
        ));
        return r;
    }
    if !authorized(req, password) {
        let mut r = Response::new(401, "Belvedere: password required.\n".into(), "text/plain");
        r.headers.push((
            "WWW-Authenticate".into(),
            "Basic realm=\"Belvedere\"".into(),
        ));
        return r;
    }
    let path = if req.path.is_empty() {
        "/"
    } else {
        req.path.as_str()
    };
    let path_slash = if path.ends_with('/') || path.ends_with(".ics") {
        path.to_string()
    } else {
        format!("{path}/")
    };
    match method.as_str() {
        "PROPFIND" => {
            let d = depth(req);
            let mut body = String::from(MULTISTATUS_OPEN);
            match path_slash.as_str() {
                "/" | "/.well-known/caldav/" => {
                    body.push_str(&response_xml(
                        "/",
                        &format!("<D:resourcetype><D:collection/></D:resourcetype><D:current-user-principal><D:href>{PRINCIPAL}</D:href></D:current-user-principal>"),
                    ));
                }
                PRINCIPAL | "/principals/" => {
                    body.push_str(&response_xml(PRINCIPAL, &principal_props()));
                }
                HOME => {
                    body.push_str(&response_xml(HOME, &home_props()));
                    if d >= 1 {
                        body.push_str(&response_xml(CALENDAR, &calendar_props(items)));
                    }
                }
                CALENDAR => {
                    body.push_str(&response_xml(CALENDAR, &calendar_props(items)));
                    if d >= 1 {
                        for i in items {
                            body.push_str(&response_xml(&i.href(), &item_props(i, false)));
                        }
                    }
                }
                _ => match item_at(&path_slash, items) {
                    Some(i) => body.push_str(&response_xml(&i.href(), &item_props(i, false))),
                    None => return Response::empty(404),
                },
            }
            body.push_str(MULTISTATUS_CLOSE);
            Response::xml(207, body)
        }
        "REPORT" => {
            if path_slash != CALENDAR {
                return Response::empty(404);
            }
            let wanted = hrefs_in(&req.body);
            let mut body = String::from(MULTISTATUS_OPEN);
            if wanted.is_empty() {
                // calendar-query (or anything else): every task.
                for i in items {
                    body.push_str(&response_xml(&i.href(), &item_props(i, true)));
                }
            } else {
                for href in wanted {
                    match item_at(&href, items) {
                        Some(i) => body.push_str(&response_xml(&i.href(), &item_props(i, true))),
                        None => body.push_str(&format!(
                            "<D:response><D:href>{}</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response>",
                            xml_escape(&href)
                        )),
                    }
                }
            }
            body.push_str(MULTISTATUS_CLOSE);
            Response::xml(207, body)
        }
        "GET" | "HEAD" => match item_at(&path_slash, items) {
            Some(i) => {
                let text = vtodo(i);
                let mut r = Response::new(
                    200,
                    if method == "HEAD" {
                        String::new()
                    } else {
                        text.clone()
                    },
                    "text/calendar; charset=utf-8",
                );
                r.headers.push(("ETag".into(), i.etag()));
                if method == "HEAD" {
                    r.headers.retain(|(k, _)| k != "Content-Length");
                    r.headers
                        .push(("Content-Length".into(), text.len().to_string()));
                }
                r
            }
            None if path_slash == CALENDAR || path_slash == HOME || path_slash == "/" => {
                Response::new(
                    200,
                    "Belvedere calendar. Subscribe with a CalDAV client.\n".into(),
                    "text/plain",
                )
            }
            None => Response::empty(404),
        },
        "PUT" | "DELETE" | "MKCALENDAR" | "MKCOL" | "PROPPATCH" | "MOVE" | "COPY" => {
            // Read-only until two-way sync lands.
            Response::new(
                403,
                "Belvedere's calendar is read-only for now.\n".into(),
                "text/plain",
            )
        }
        _ => Response::empty(405),
    }
}

/// A fresh random password, 24 hex characters, from the system's
/// randomness.
pub fn generate_password() -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 12];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: i64, title: &str, status: TaskStatus) -> Task {
        Task {
            id,
            title: title.into(),
            notes: "From City Power <billing@citypower.invalid>: Your statement\nAmount: $84.12"
                .into(),
            due_at: Some("2026-10-31T14:00:00.000Z".into()),
            status,
            dismiss_reason: (status == TaskStatus::Dismissed).then(|| "already paid".to_string()),
            created_at: "2026-10-15T14:00:00.000Z".into(),
            updated_at: "2026-10-16T09:30:00.000Z".into(),
            completed_at: (status == TaskStatus::Done)
                .then(|| "2026-10-20T09:00:00.000Z".to_string()),
            deleted_at: None,
            kind: "bill".into(),
            reference: String::new(),
        }
    }

    fn items() -> Vec<Item> {
        vec![
            Item {
                task: task(
                    1,
                    "Pay the City Power bill; it's due, really",
                    TaskStatus::Open,
                ),
                source: Some("<stmt@citypower.invalid>".into()),
            },
            Item {
                task: task(2, "Call the dentist", TaskStatus::Done),
                source: None,
            },
            Item {
                task: task(3, "Old rent", TaskStatus::Dismissed),
                source: None,
            },
        ]
    }

    fn request(
        method: &str,
        path: &str,
        depth: Option<&str>,
        body: &str,
        password: Option<&str>,
    ) -> Request {
        let mut headers = HashMap::new();
        if let Some(d) = depth {
            headers.insert("depth".into(), d.into());
        }
        if let Some(p) = password {
            headers.insert(
                "authorization".into(),
                format!("Basic {}", base64_encode(&format!("{USERNAME}:{p}"))),
            );
        }
        Request {
            method: method.into(),
            path: path.into(),
            headers,
            body: body.into(),
        }
    }

    #[test]
    fn vtodo_carries_title_notes_due_status_and_source() {
        let its = items();
        let folded = vtodo(&its[0]);
        assert!(folded.lines().all(|l| l.len() <= 76), "lines are folded");
        let open = folded.replace("\r\n ", "");
        assert!(open.contains("UID:belvedere-task-1@belvedere"));
        assert!(open.contains("SUMMARY:Pay the City Power bill\\; it's due\\, really"));
        assert!(open.contains("DESCRIPTION:From City Power <billing@citypower.invalid>: Your statement\\nAmount: $84.12"));
        assert!(open.contains("DUE:20261031T140000Z"));
        assert!(open.contains("STATUS:NEEDS-ACTION"));
        assert!(open.contains("URL:mid:stmt@citypower.invalid"));
        assert!(open.contains("X-BELVEDERE-SOURCE:<stmt@citypower.invalid>"));
        let done = vtodo(&its[1]).replace("\r\n ", "");
        assert!(
            done.contains("STATUS:COMPLETED")
                && done.contains("COMPLETED:20261020T090000Z")
                && done.contains("PERCENT-COMPLETE:100")
        );
        assert!(!done.contains("URL:"));
        let dismissed = vtodo(&its[2]).replace("\r\n ", "");
        assert!(
            dismissed.contains("STATUS:CANCELLED")
                && dismissed.contains("X-BELVEDERE-DISMISS-REASON:already paid")
        );
        // The etag and ctag move with changes.
        let mut changed = its.clone();
        changed[0].task.updated_at = "2026-10-17T09:30:00.000Z".into();
        assert_ne!(its[0].etag(), changed[0].etag());
        assert_ne!(ctag(&its), ctag(&changed));
        assert_ne!(ctag(&its), ctag(&its[1..]));
    }

    #[test]
    fn password_is_required_everywhere_but_options() {
        let its = items();
        assert_eq!(
            handle(&request("OPTIONS", "/", None, "", None), "pw", &its).status,
            200
        );
        let no = handle(&request("PROPFIND", "/", Some("0"), "", None), "pw", &its);
        assert_eq!(no.status, 401);
        assert!(no
            .headers
            .iter()
            .any(|(k, v)| k == "WWW-Authenticate" && v.contains("Basic")));
        assert_eq!(
            handle(
                &request("PROPFIND", "/", Some("0"), "", Some("wrong")),
                "pw",
                &its
            )
            .status,
            401
        );
        assert_eq!(
            handle(
                &request("GET", &its[0].href(), None, "", Some("wrong")),
                "pw",
                &its
            )
            .status,
            401
        );
        let mut bad_user = request("PROPFIND", "/", Some("0"), "", None);
        bad_user.headers.insert(
            "authorization".into(),
            format!("Basic {}", base64_encode("someone:pw")),
        );
        assert_eq!(handle(&bad_user, "pw", &its).status, 401);
        assert_eq!(
            handle(
                &request("PROPFIND", "/", Some("0"), "", Some("pw")),
                "pw",
                &its
            )
            .status,
            207
        );
    }

    #[test]
    fn discovery_listing_and_fetching() {
        let its = items();
        let root = handle(
            &request("PROPFIND", "/", Some("0"), "", Some("pw")),
            "pw",
            &its,
        );
        assert!(root.body.contains(&format!(
            "<D:current-user-principal><D:href>{PRINCIPAL}</D:href>"
        )));
        let principal = handle(
            &request("PROPFIND", PRINCIPAL, Some("0"), "", Some("pw")),
            "pw",
            &its,
        );
        assert!(principal
            .body
            .contains(&format!("<C:calendar-home-set><D:href>{HOME}</D:href>")));
        let home = handle(
            &request("PROPFIND", HOME, Some("1"), "", Some("pw")),
            "pw",
            &its,
        );
        assert!(home.body.contains(CALENDAR) && home.body.contains("<C:comp name=\"VTODO\"/>"));
        let listing = handle(
            &request("PROPFIND", CALENDAR, Some("1"), "", Some("pw")),
            "pw",
            &its,
        );
        assert_eq!(listing.body.matches("<D:getetag>").count(), 3);
        assert!(listing.body.contains("getctag"));
        assert!(
            !listing.body.contains("BEGIN:VCALENDAR"),
            "listing has no data"
        );
        let multiget = format!(
            "<?xml version=\"1.0\"?><C:calendar-multiget xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:prop><D:getetag/><C:calendar-data/></D:prop><D:href>{}</D:href><D:href>{}nope.ics</D:href></C:calendar-multiget>",
            its[1].href(),
            CALENDAR
        );
        let fetched = handle(
            &request("REPORT", CALENDAR, Some("1"), &multiget, Some("pw")),
            "pw",
            &its,
        );
        assert_eq!(fetched.status, 207);
        assert!(fetched.body.contains("SUMMARY:Call the dentist"));
        assert!(!fetched.body.contains("SUMMARY:Pay the City Power"));
        assert!(fetched.body.contains("404 Not Found"));
        let query = handle(&request("REPORT", CALENDAR, Some("1"), "<C:calendar-query xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><C:filter/></C:calendar-query>", Some("pw")), "pw", &its);
        assert_eq!(query.body.matches("BEGIN:VTODO").count(), 3);
        let one = handle(
            &request("GET", &its[0].href(), None, "", Some("pw")),
            "pw",
            &its,
        );
        assert_eq!(one.status, 200);
        assert!(one.body.starts_with("BEGIN:VCALENDAR"));
        assert!(one
            .headers
            .iter()
            .any(|(k, v)| k == "ETag" && *v == its[0].etag()));
        assert_eq!(
            handle(
                &request(
                    "GET",
                    &format!("{CALENDAR}missing.ics"),
                    None,
                    "",
                    Some("pw")
                ),
                "pw",
                &its
            )
            .status,
            404
        );
        assert_eq!(
            handle(
                &request("PUT", &its[0].href(), None, "BEGIN:VCALENDAR", Some("pw")),
                "pw",
                &its
            )
            .status,
            403
        );
        assert_eq!(
            handle(
                &request("DELETE", &its[0].href(), None, "", Some("pw")),
                "pw",
                &its
            )
            .status,
            403
        );
    }

    #[test]
    fn request_heads_parse_and_base64_round_trips() {
        let r = Request::parse_head("PROPFIND http://127.0.0.1:38481/calendars/belvedere/?x=1 HTTP/1.1\r\nHost: 127.0.0.1\r\nDepth: 1\r\nContent-Length: 12").unwrap();
        assert_eq!(r.method, "PROPFIND");
        assert_eq!(r.path, HOME);
        assert_eq!(r.header("depth"), Some("1"));
        assert_eq!(r.header("content-length"), Some("12"));
        assert_eq!(
            base64_decode(&base64_encode("belvedere:abc123")).unwrap(),
            "belvedere:abc123"
        );
        assert_eq!(base64_encode("ab"), "YWI=");
        assert_eq!(generate_password().unwrap().len(), 24);
    }
}
