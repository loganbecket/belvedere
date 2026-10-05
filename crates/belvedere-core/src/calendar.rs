//! Reading Thunderbird's calendars from disk: the events and tasks in its
//! calendar databases, with recurring events expanded for the weeks
//! ahead. Read-only, from snapshot copies, like the mail.
//!
//! Thunderbird keeps locally stored calendars in `calendar-data/local.sqlite`
//! and cached network calendars in `calendar-data/cache.sqlite`. Times are
//! microseconds since the epoch in UTC; a time zone name says how the user
//! sees them. Recurrence rules are stored as iCalendar lines.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::thunderbird::{parse_prefs, Pref, Snapshot};

/// How far ahead recurring events are expanded.
pub const WINDOW_DAYS: i64 = 60;

/// A calendar Thunderbird knows about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Calendar {
    pub id: String,
    pub name: String,
    /// `storage` (kept locally), `ics`, `caldav`, ...
    pub kind: String,
    pub enabled: bool,
    /// Network calendars with a local cache have their events on disk;
    /// without one, Thunderbird holds them only in memory.
    pub cached: bool,
}

/// One occurrence of an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub calendar_id: String,
    /// The iCalendar UID.
    pub uid: String,
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    pub location: String,
    /// For an occurrence of a recurring event: when this occurrence was
    /// originally scheduled (RFC 3339), so it can be told from the others.
    pub recurrence_id: Option<String>,
}

/// A Thunderbird task (a VTODO).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TbTask {
    pub calendar_id: String,
    pub uid: String,
    pub title: String,
    pub due: Option<DateTime<Utc>>,
    pub completed: bool,
}

/// Everything read in one pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reading {
    pub calendars: Vec<Calendar>,
    pub events: Vec<Event>,
    pub tasks: Vec<TbTask>,
    /// The database files that were read.
    pub files: Vec<PathBuf>,
    /// Calendars with at least one item in those files. A network
    /// calendar missing here is one Thunderbird holds only in memory.
    pub on_disk: Vec<String>,
}

/// The calendars registered in the profile's `prefs.js`.
pub fn calendars(profile: &Path) -> std::io::Result<Vec<Calendar>> {
    let prefs = parse_prefs(&std::fs::read_to_string(profile.join("prefs.js"))?);
    let mut ids: Vec<String> = prefs
        .keys()
        .filter_map(|k| k.strip_prefix("calendar.registry."))
        .filter_map(|rest| rest.split('.').next())
        .map(str::to_string)
        .collect();
    ids.sort();
    ids.dedup();
    let s = |k: &str| prefs.get(k).and_then(Pref::as_str).map(str::to_string);
    let b = |k: &str| match prefs.get(k) {
        Some(Pref::Bool(v)) => Some(*v),
        _ => None,
    };
    Ok(ids
        .into_iter()
        .filter(|id| s(&format!("calendar.registry.{id}.type")).is_some())
        .map(|id| Calendar {
            name: s(&format!("calendar.registry.{id}.name")).unwrap_or_default(),
            kind: s(&format!("calendar.registry.{id}.type")).unwrap_or_default(),
            enabled: !b(&format!("calendar.registry.{id}.disabled")).unwrap_or(false),
            cached: b(&format!("calendar.registry.{id}.cache.enabled")).unwrap_or(false),
            id,
        })
        .collect())
}

/// The calendar database files a profile may have.
pub fn database_files(profile: &Path) -> Vec<PathBuf> {
    ["local.sqlite", "cache.sqlite"]
        .iter()
        .map(|f| profile.join("calendar-data").join(f))
        .filter(|p| p.is_file())
        .collect()
}

/// Reads every calendar database in the profile: events overlapping
/// `from..to` (recurring ones expanded), and all tasks.
pub fn read(profile: &Path, from: DateTime<Utc>, to: DateTime<Utc>) -> std::io::Result<Reading> {
    let mut out = Reading {
        calendars: calendars(profile).unwrap_or_default(),
        ..Default::default()
    };
    for file in database_files(profile) {
        let snap = Snapshot::open(&file)?;
        read_database(&snap.conn, from, to, &mut out).map_err(std::io::Error::other)?;
        out.files.push(file);
    }
    out.events
        .sort_by(|a, b| a.start.cmp(&b.start).then(a.title.cmp(&b.title)));
    Ok(out)
}

/// Thunderbird's `flags` bits on an item.
const FLAG_ALL_DAY: i64 = 8;
const FLAG_RECURRENCE: i64 = 16;

struct Row {
    cal_id: String,
    id: String,
    title: String,
    flags: i64,
    start: i64,
    end: i64,
    start_tz: String,
    recurrence_id: Option<i64>,
    status: String,
}

/// Reads one database into `out`. Public so a test can build a database
/// of its own and read it directly.
pub fn read_database(
    conn: &rusqlite::Connection,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    out: &mut Reading,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT cal_id, id, title, flags, event_start, event_end, event_start_tz, recurrence_id, ical_status
         FROM cal_events",
    )?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            Ok(Row {
                cal_id: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                id: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                title: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                flags: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                start: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                end: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                start_tz: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                recurrence_id: r.get(7)?,
                status: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let locations = properties(conn, "LOCATION")?;
    {
        let mut ids = conn.prepare(
            "SELECT DISTINCT cal_id FROM cal_events UNION SELECT DISTINCT cal_id FROM cal_todos",
        )?;
        for id in ids.query_map([], |r| r.get::<_, Option<String>>(0))? {
            if let Some(id) = id? {
                if !out.on_disk.contains(&id) {
                    out.on_disk.push(id);
                }
            }
        }
    }

    // Exceptions (rows with a recurrence_id) override one occurrence of
    // their master.
    let mut exceptions: HashMap<(String, String), Vec<&Row>> = HashMap::new();
    for r in rows.iter().filter(|r| r.recurrence_id.is_some()) {
        exceptions
            .entry((r.cal_id.clone(), r.id.clone()))
            .or_default()
            .push(r);
    }
    let mut rule_stmt =
        conn.prepare("SELECT icalString FROM cal_recurrence WHERE cal_id = ?1 AND item_id = ?2")?;

    for r in rows.iter().filter(|r| r.recurrence_id.is_none()) {
        if r.status.eq_ignore_ascii_case("CANCELLED") {
            continue;
        }
        let location = locations
            .get(&(r.cal_id.clone(), r.id.clone()))
            .cloned()
            .unwrap_or_default();
        let zone = zone_of(&r.start_tz);
        let all_day = r.flags & FLAG_ALL_DAY != 0;
        let (start, end) = span(r.start, r.end, &zone, all_day);
        let base = Event {
            calendar_id: r.cal_id.clone(),
            uid: r.id.clone(),
            title: r.title.clone(),
            start,
            end,
            all_day,
            location,
            recurrence_id: None,
        };
        if r.flags & FLAG_RECURRENCE == 0 {
            if start < to && end > from {
                out.events.push(base);
            }
            continue;
        }
        let lines: Vec<String> = rule_stmt
            .query_map([&r.cal_id, &r.id], |row| row.get::<_, Option<String>>(0))?
            .filter_map(|l| l.ok().flatten())
            .collect();
        let overrides = exceptions
            .remove(&(r.cal_id.clone(), r.id.clone()))
            .unwrap_or_default();
        for occurrence in expand(&base, &zone, &lines, from, to) {
            // An override for this occurrence replaces it (or cancels it).
            let scheduled = occurrence.start.timestamp_micros();
            match overrides.iter().find(|o| {
                o.recurrence_id
                    .is_some_and(|rid| same_moment(rid, scheduled, &zone, all_day))
            }) {
                Some(o) if o.status.eq_ignore_ascii_case("CANCELLED") => {}
                Some(o) => {
                    let o_all_day = o.flags & FLAG_ALL_DAY != 0;
                    let (s, e) = span(o.start, o.end, &zone_of(&o.start_tz), o_all_day);
                    if s < to && e > from {
                        out.events.push(Event {
                            title: o.title.clone(),
                            start: s,
                            end: e,
                            all_day: o_all_day,
                            ..occurrence
                        });
                    }
                }
                None => out.events.push(occurrence),
            }
        }
    }

    let mut todo = conn.prepare(
        "SELECT cal_id, id, title, todo_due, todo_completed, ical_status FROM cal_todos WHERE recurrence_id IS NULL",
    )?;
    let tasks = todo.query_map([], |r| {
        let due: Option<i64> = r.get(3)?;
        let completed_at: Option<i64> = r.get(4)?;
        let status: Option<String> = r.get(5)?;
        Ok(TbTask {
            calendar_id: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            uid: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            title: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            due: due.filter(|d| *d != 0).map(micros_to_utc),
            completed: completed_at.is_some_and(|c| c != 0)
                || status.is_some_and(|s| s.eq_ignore_ascii_case("COMPLETED")),
        })
    })?;
    for t in tasks {
        out.tasks.push(t?);
    }
    Ok(())
}

fn properties(
    conn: &rusqlite::Connection,
    key: &str,
) -> rusqlite::Result<HashMap<(String, String), String>> {
    let mut stmt = conn.prepare(
        "SELECT cal_id, item_id, value FROM cal_properties WHERE key = ?1 AND recurrence_id IS NULL",
    )?;
    let rows = stmt.query_map([key], |r| {
        let value: rusqlite::types::Value = r.get(2)?;
        let text = match value {
            rusqlite::types::Value::Text(t) => t,
            rusqlite::types::Value::Blob(b) => String::from_utf8_lossy(&b).into_owned(),
            _ => String::new(),
        };
        Ok((
            (
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ),
            text,
        ))
    })?;
    rows.collect()
}

/// Whether an override's recurrence id names the occurrence that was
/// scheduled at `scheduled` (both in microseconds). All-day recurrence
/// ids are dates; compare by calendar day in the event's zone.
fn same_moment(rid: i64, scheduled: i64, zone: &Zone, all_day: bool) -> bool {
    if rid == scheduled {
        return true;
    }
    if all_day {
        return date_of(rid, zone) == date_of(scheduled, zone);
    }
    false
}

fn date_of(micros: i64, zone: &Zone) -> NaiveDate {
    match zone {
        Zone::Utc | Zone::Floating => micros_to_utc(micros).date_naive(),
        Zone::Tz(tz) => micros_to_utc(micros).with_timezone(tz).date_naive(),
    }
}

/// How a stored time is to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Utc,
    /// No zone: the stored wall-clock time is meant in the user's zone.
    Floating,
    Tz(chrono_tz::Tz),
}

/// The zone a stored name means. Names come as IANA ids, Windows names,
/// or whole VTIMEZONE blocks; anything unknown is treated as the user's
/// own zone.
pub fn zone_of(name: &str) -> Zone {
    let name = name.trim();
    if name.is_empty() || name.eq_ignore_ascii_case("floating") {
        return Zone::Floating;
    }
    if name.eq_ignore_ascii_case("UTC") || name.eq_ignore_ascii_case("Etc/UTC") || name == "Z" {
        return Zone::Utc;
    }
    let candidate = if name.starts_with("BEGIN:VTIMEZONE") {
        name.lines()
            .find_map(|l| l.strip_prefix("TZID:"))
            .unwrap_or("")
            .trim()
            .to_string()
    } else {
        name.to_string()
    };
    if let Ok(tz) = chrono_tz::Tz::from_str(&candidate) {
        return Zone::Tz(tz);
    }
    if let Some(tz) = windows_zone(&candidate) {
        return Zone::Tz(tz);
    }
    if let Some(tz) = local_tz() {
        return Zone::Tz(tz);
    }
    Zone::Floating
}

/// Common Windows and Outlook zone names.
fn windows_zone(name: &str) -> Option<chrono_tz::Tz> {
    let n = name.to_ascii_lowercase();
    let pick = |iana: &str| chrono_tz::Tz::from_str(iana).ok();
    if n.contains("eastern") {
        pick("America/New_York")
    } else if n.contains("central") && !n.contains("europe") {
        pick("America/Chicago")
    } else if n.contains("mountain") {
        pick("America/Denver")
    } else if n.contains("pacific") {
        pick("America/Los_Angeles")
    } else if n.contains("gmt") || n.contains("greenwich") {
        pick("Europe/London")
    } else if n.contains("w. europe") || n.contains("romance") || n.contains("central europe") {
        pick("Europe/Paris")
    } else {
        None
    }
}

/// The machine's own zone as an IANA zone, when it can be named.
pub fn local_tz() -> Option<chrono_tz::Tz> {
    iana_time_zone::get_timezone()
        .ok()
        .and_then(|n| chrono_tz::Tz::from_str(&n).ok())
}

fn micros_to_utc(micros: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(micros).unwrap_or(DateTime::UNIX_EPOCH)
}

/// The UTC start and end of a stored event. Timed events are stored in
/// UTC already; floating ones hold the wall-clock time as if it were UTC
/// and mean the user's zone; all-day ones hold the date as midnight UTC
/// and mean the whole local day.
fn span(start: i64, end: i64, zone: &Zone, all_day: bool) -> (DateTime<Utc>, DateTime<Utc>) {
    let s = micros_to_utc(start);
    let e = micros_to_utc(if end == 0 { start } else { end });
    if all_day {
        let day_start = |d: DateTime<Utc>| local_midnight(d.date_naive());
        let mut end_day = e.date_naive();
        if end_day <= s.date_naive() {
            end_day = s.date_naive() + Duration::days(1);
        }
        return (day_start(s), local_midnight(end_day));
    }
    match zone {
        Zone::Floating => (as_local(s.naive_utc()), as_local(e.naive_utc())),
        _ => (s, e),
    }
}

fn local_midnight(day: NaiveDate) -> DateTime<Utc> {
    as_local(day.and_hms_opt(0, 0, 0).unwrap_or_default())
}

fn as_local(naive: NaiveDateTime) -> DateTime<Utc> {
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&naive))
}

/// Expands a recurring event's rule lines into the occurrences that
/// overlap `from..to`. The duration of every occurrence is the master's.
pub fn expand(
    master: &Event,
    zone: &Zone,
    lines: &[String],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<Event> {
    let duration = master.end - master.start;
    let tz: rrule::Tz = match zone {
        Zone::Utc => rrule::Tz::UTC,
        Zone::Tz(tz) => rrule::Tz::Tz(*tz),
        Zone::Floating => local_tz().map(rrule::Tz::Tz).unwrap_or(rrule::Tz::LOCAL),
    };
    let start_in_zone = master.start.with_timezone(&tz);
    let mut text = if master.all_day {
        format!("DTSTART;VALUE=DATE:{}\n", start_in_zone.format("%Y%m%d"))
    } else if matches!(zone, Zone::Utc) {
        format!("DTSTART:{}\n", start_in_zone.format("%Y%m%dT%H%M%SZ"))
    } else {
        format!(
            "DTSTART;TZID={}:{}\n",
            tz.name(),
            start_in_zone.format("%Y%m%dT%H%M%S")
        )
    };
    let mut has_rule = false;
    for line in lines {
        for l in line.lines() {
            let l = l.trim();
            if l.is_empty() {
                continue;
            }
            let upper = l.to_ascii_uppercase();
            if upper.starts_with("RRULE") || upper.starts_with("EXRULE") {
                has_rule = true;
                text.push_str(l);
                text.push('\n');
            } else if upper.starts_with("EXDATE") || upper.starts_with("RDATE") {
                text.push_str(&with_zone(l, &tz, master.all_day));
                text.push('\n');
            }
        }
    }
    if !has_rule {
        return Vec::new();
    }
    let set = match rrule::RRuleSet::from_str(&text) {
        Ok(set) => set,
        Err(err) => {
            tracing::warn!(uid = master.uid, "cannot read recurrence rule: {err}");
            return Vec::new();
        }
    };
    // Occurrences that start before `from` may still overlap it.
    let after = (from - duration).with_timezone(&tz);
    let before = to.with_timezone(&tz);
    let result = set.after(after).before(before).all(1000);
    result
        .dates
        .into_iter()
        .map(|d| d.with_timezone(&Utc))
        .filter(|s| *s < to && *s + duration > from)
        .map(|s| Event {
            start: s,
            end: s + duration,
            recurrence_id: Some(s.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            ..master.clone()
        })
        .collect()
}

/// EXDATE and RDATE lines without a zone mean the event's zone; say so,
/// since a bare time would otherwise be read as UTC.
fn with_zone(line: &str, tz: &rrule::Tz, all_day: bool) -> String {
    let Some((head, values)) = line.split_once(':') else {
        return line.to_string();
    };
    if head.to_ascii_uppercase().contains("TZID=") || values.ends_with('Z') || all_day {
        if all_day && !head.to_ascii_uppercase().contains("VALUE=DATE") {
            return format!(
                "{head};VALUE=DATE:{}",
                values.split('T').next().unwrap_or(values)
            );
        }
        return line.to_string();
    }
    if tz.is_local() {
        return line.to_string();
    }
    format!("{head};TZID={}:{values}", tz.name())
}

/// Parses an RFC 3339 time, as the bus and tests pass them.
pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

#[allow(dead_code)]
fn optional_text(conn: &rusqlite::Connection, sql: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(sql, [], |r| r.get(0)).optional()
}

#[cfg(test)]
pub mod fixtures {
    //! A made-up calendar database in Thunderbird's layout.

    use std::path::Path;

    /// Creates `calendar-data/local.sqlite` under `profile` with the tables
    /// Belvedere reads, and returns a connection to fill it.
    pub fn create(profile: &Path) -> rusqlite::Connection {
        let dir = profile.join("calendar-data");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join("local.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE cal_events (cal_id TEXT, id TEXT, time_created INTEGER, last_modified INTEGER,
                title TEXT, priority INTEGER, privacy TEXT, ical_status TEXT, flags INTEGER,
                event_start INTEGER, event_end INTEGER, event_stamp INTEGER, event_start_tz TEXT,
                event_end_tz TEXT, recurrence_id INTEGER, recurrence_id_tz TEXT, alarm_last_ack INTEGER,
                offline_journal INTEGER);
             CREATE TABLE cal_recurrence (item_id TEXT, cal_id TEXT, icalString TEXT);
             CREATE TABLE cal_properties (item_id TEXT, key TEXT, value BLOB, recurrence_id INTEGER,
                recurrence_id_tz TEXT, cal_id TEXT);
             CREATE TABLE cal_todos (cal_id TEXT, id TEXT, time_created INTEGER, last_modified INTEGER,
                title TEXT, priority INTEGER, privacy TEXT, ical_status TEXT, flags INTEGER,
                todo_entry INTEGER, todo_due INTEGER, todo_completed INTEGER, todo_complete INTEGER,
                todo_entry_tz TEXT, todo_due_tz TEXT, todo_completed_tz TEXT, recurrence_id INTEGER,
                recurrence_id_tz TEXT, alarm_last_ack INTEGER, todo_stamp INTEGER, offline_journal INTEGER);",
        )
        .unwrap();
        conn
    }

    /// Microseconds since the epoch for a UTC time.
    pub fn micros(rfc3339: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_micros()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn event(
        conn: &rusqlite::Connection,
        cal: &str,
        id: &str,
        title: &str,
        start: &str,
        end: &str,
        tz: &str,
        flags: i64,
        recurrence_id: Option<i64>,
    ) {
        conn.execute(
            "INSERT INTO cal_events (cal_id, id, title, flags, event_start, event_end, event_start_tz, event_end_tz, recurrence_id, ical_status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, 'CONFIRMED')",
            rusqlite::params![cal, id, title, flags, micros(start), micros(end), tz, recurrence_id],
        )
        .unwrap();
    }

    pub fn rule(conn: &rusqlite::Connection, cal: &str, id: &str, line: &str) {
        conn.execute(
            "INSERT INTO cal_recurrence (item_id, cal_id, icalString) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, cal, line],
        )
        .unwrap();
    }

    pub fn property(conn: &rusqlite::Connection, cal: &str, id: &str, key: &str, value: &str) {
        conn.execute(
            "INSERT INTO cal_properties (item_id, key, value, cal_id) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, key, value.as_bytes(), cal],
        )
        .unwrap();
    }

    pub fn todo(
        conn: &rusqlite::Connection,
        cal: &str,
        id: &str,
        title: &str,
        due: Option<&str>,
        completed: bool,
    ) {
        conn.execute(
            "INSERT INTO cal_todos (cal_id, id, title, flags, todo_due, todo_completed, ical_status)
             VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)",
            rusqlite::params![
                cal,
                id,
                title,
                due.map(micros),
                completed.then(|| micros("2026-10-01T12:00:00Z")),
                if completed { "COMPLETED" } else { "NEEDS-ACTION" }
            ],
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        parse_time(s).unwrap()
    }

    fn prefs(profile: &Path) {
        std::fs::write(
            profile.join("prefs.js"),
            r#"user_pref("calendar.registry.aaa.name", "Home");
user_pref("calendar.registry.aaa.type", "storage");
user_pref("calendar.registry.bbb.name", "Team");
user_pref("calendar.registry.bbb.type", "ics");
user_pref("calendar.registry.bbb.cache.enabled", true);
user_pref("calendar.registry.ccc.name", "Old");
user_pref("calendar.registry.ccc.type", "caldav");
user_pref("calendar.registry.ccc.disabled", true);
"#,
        )
        .unwrap();
    }

    #[test]
    fn calendars_come_from_prefs() {
        let dir = tempfile::tempdir().unwrap();
        prefs(dir.path());
        let cals = calendars(dir.path()).unwrap();
        assert_eq!(cals.len(), 3);
        assert_eq!(
            (
                cals[0].name.as_str(),
                cals[0].kind.as_str(),
                cals[0].enabled,
                cals[0].cached
            ),
            ("Home", "storage", true, false)
        );
        assert!(cals[1].cached);
        assert!(!cals[2].enabled);
    }

    #[test]
    fn weekly_event_across_the_fall_time_change_keeps_its_wall_clock_time() {
        let dir = tempfile::tempdir().unwrap();
        let conn = create(dir.path());
        // Tuesdays 4:00 PM Chicago, from October 20, 2026 (CDT). After
        // November 1 the clocks fall back; 4:00 PM is then 22:00Z.
        event(
            &conn,
            "aaa",
            "practice",
            "Practice",
            "2026-10-20T21:00:00Z",
            "2026-10-20T22:30:00Z",
            "America/Chicago",
            FLAG_RECURRENCE,
            None,
        );
        rule(&conn, "aaa", "practice", "RRULE:FREQ=WEEKLY;BYDAY=TU");
        // October 27 is skipped; November 10 moved to 5:00 PM and renamed.
        rule(&conn, "aaa", "practice", "EXDATE:20261027T160000");
        event(
            &conn,
            "aaa",
            "practice",
            "Practice (late)",
            "2026-11-10T23:00:00Z",
            "2026-11-11T00:30:00Z",
            "America/Chicago",
            0,
            Some(micros("2026-11-10T22:00:00Z")),
        );
        property(&conn, "aaa", "practice", "LOCATION", "Rink B");
        drop(conn);

        let mut out = Reading::default();
        let snap = Snapshot::open(&dir.path().join("calendar-data/local.sqlite")).unwrap();
        read_database(
            &snap.conn,
            utc("2026-10-19T00:00:00Z"),
            utc("2026-11-18T00:00:00Z"),
            &mut out,
        )
        .unwrap();
        let starts: Vec<(String, String)> = out
            .events
            .iter()
            .map(|e| (e.start.to_rfc3339(), e.title.clone()))
            .collect();
        assert_eq!(
            starts,
            [
                (
                    "2026-10-20T21:00:00+00:00".to_string(),
                    "Practice".to_string()
                ),
                (
                    "2026-11-03T22:00:00+00:00".to_string(),
                    "Practice".to_string()
                ),
                (
                    "2026-11-10T23:00:00+00:00".to_string(),
                    "Practice (late)".to_string()
                ),
                (
                    "2026-11-17T22:00:00+00:00".to_string(),
                    "Practice".to_string()
                ),
            ]
        );
        assert!(out.events.iter().all(|e| e.location == "Rink B"));
        assert!(out.events.iter().all(|e| e.recurrence_id.is_some()));
        assert_eq!(
            out.events[0].end - out.events[0].start,
            Duration::minutes(90)
        );
    }

    #[test]
    fn spring_forward_daily_event_and_count_limit() {
        let dir = tempfile::tempdir().unwrap();
        let conn = create(dir.path());
        // 9:00 AM New York daily for 5 days over March 8, 2026 (EST -> EDT).
        event(
            &conn,
            "aaa",
            "standup",
            "Standup",
            "2026-03-06T14:00:00Z",
            "2026-03-06T14:15:00Z",
            "America/New_York",
            FLAG_RECURRENCE,
            None,
        );
        rule(&conn, "aaa", "standup", "RRULE:FREQ=DAILY;COUNT=5");
        drop(conn);
        let mut out = Reading::default();
        let snap = Snapshot::open(&dir.path().join("calendar-data/local.sqlite")).unwrap();
        read_database(
            &snap.conn,
            utc("2026-03-01T00:00:00Z"),
            utc("2026-04-01T00:00:00Z"),
            &mut out,
        )
        .unwrap();
        let starts: Vec<String> = out.events.iter().map(|e| e.start.to_rfc3339()).collect();
        assert_eq!(
            starts,
            [
                "2026-03-06T14:00:00+00:00",
                "2026-03-07T14:00:00+00:00",
                "2026-03-08T13:00:00+00:00",
                "2026-03-09T13:00:00+00:00",
                "2026-03-10T13:00:00+00:00",
            ]
        );
    }

    #[test]
    fn all_day_utc_floating_windows_names_and_todos() {
        let dir = tempfile::tempdir().unwrap();
        let conn = create(dir.path());
        event(
            &conn,
            "aaa",
            "holiday",
            "Holiday",
            "2026-10-31T00:00:00Z",
            "2026-11-01T00:00:00Z",
            "floating",
            FLAG_ALL_DAY,
            None,
        );
        event(
            &conn,
            "aaa",
            "call",
            "Call",
            "2026-10-22T15:00:00Z",
            "2026-10-22T15:30:00Z",
            "UTC",
            0,
            None,
        );
        event(
            &conn,
            "aaa",
            "outlook",
            "Outlook meeting",
            "2026-10-23T18:00:00Z",
            "2026-10-23T19:00:00Z",
            "BEGIN:VTIMEZONE\nTZID:Central Standard Time\nEND:VTIMEZONE",
            0,
            None,
        );
        event(
            &conn,
            "aaa",
            "old",
            "Last year",
            "2025-10-23T18:00:00Z",
            "2025-10-23T19:00:00Z",
            "UTC",
            0,
            None,
        );
        event(
            &conn,
            "aaa",
            "gone",
            "Cancelled",
            "2026-10-24T18:00:00Z",
            "2026-10-24T19:00:00Z",
            "UTC",
            0,
            None,
        );
        conn.execute(
            "UPDATE cal_events SET ical_status = 'CANCELLED' WHERE id = 'gone'",
            [],
        )
        .unwrap();
        todo(
            &conn,
            "aaa",
            "t1",
            "Renew the permit",
            Some("2026-11-30T14:00:00Z"),
            false,
        );
        todo(&conn, "aaa", "t2", "Done already", None, true);
        drop(conn);
        let mut out = Reading::default();
        let snap = Snapshot::open(&dir.path().join("calendar-data/local.sqlite")).unwrap();
        read_database(
            &snap.conn,
            utc("2026-10-19T00:00:00Z"),
            utc("2026-11-18T00:00:00Z"),
            &mut out,
        )
        .unwrap();
        out.events.sort_by_key(|e| e.start);
        let titles: Vec<&str> = out.events.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["Call", "Outlook meeting", "Holiday"]);
        let holiday = out.events.iter().find(|e| e.title == "Holiday").unwrap();
        assert!(holiday.all_day);
        assert_eq!(
            holiday.start.with_timezone(&Local).date_naive(),
            NaiveDate::from_ymd_opt(2026, 10, 31).unwrap()
        );
        assert_eq!(holiday.end - holiday.start, Duration::days(1));
        assert_eq!(
            zone_of("BEGIN:VTIMEZONE\nTZID:Central Standard Time\nEND:VTIMEZONE"),
            Zone::Tz(chrono_tz::America::Chicago)
        );
        assert_eq!(
            zone_of("(UTC-05:00) Eastern Time (US & Canada)"),
            Zone::Tz(chrono_tz::America::New_York)
        );
        assert_eq!(zone_of("Etc/UTC"), Zone::Utc);
        assert_eq!(zone_of(""), Zone::Floating);
        assert_eq!(out.tasks.len(), 2);
        assert_eq!(out.tasks[0].due, Some(utc("2026-11-30T14:00:00Z")));
        assert!(!out.tasks[0].completed);
        assert!(out.tasks[1].completed);
    }

    #[test]
    fn reading_a_profile_changes_none_of_its_files() {
        let dir = tempfile::tempdir().unwrap();
        prefs(dir.path());
        let conn = create(dir.path());
        event(
            &conn,
            "aaa",
            "call",
            "Call",
            "2026-10-22T15:00:00Z",
            "2026-10-22T15:30:00Z",
            "UTC",
            0,
            None,
        );
        drop(conn);
        let fingerprint = |p: &Path| -> Vec<(String, u64, std::time::SystemTime, Vec<u8>)> {
            let mut v: Vec<_> = walk(p)
                .into_iter()
                .map(|f| {
                    let meta = std::fs::metadata(&f).unwrap();
                    (
                        f.display().to_string(),
                        meta.len(),
                        meta.modified().unwrap(),
                        std::fs::read(&f).unwrap(),
                    )
                })
                .collect();
            v.sort();
            v
        };
        let before = fingerprint(dir.path());
        let reading = read(
            dir.path(),
            utc("2026-10-19T00:00:00Z"),
            utc("2026-11-18T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(reading.events.len(), 1);
        assert_eq!(reading.calendars.len(), 3);
        assert_eq!(reading.files.len(), 1);
        assert_eq!(
            fingerprint(dir.path()),
            before,
            "the profile must be untouched"
        );
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
        out
    }
}
