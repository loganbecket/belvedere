//! The calendar reader against the real service: events come over the
//! bus, a change in Thunderbird's database is noticed within a minute,
//! and the profile is not touched.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{fake_profile, stop, wait_ready, Bus};
use futures_util::StreamExt;

fn micros(rfc3339: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .unwrap()
        .timestamp_micros()
}

/// A calendar database in Thunderbird's layout, empty.
fn calendar_db(profile: &Path) -> PathBuf {
    let dir = profile.join("calendar-data");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("local.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
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
    path
}

fn add_event(path: &Path, id: &str, title: &str, start: &str, end: &str) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute(
        "INSERT INTO cal_events (cal_id, id, title, flags, event_start, event_end, event_start_tz, event_end_tz, ical_status)
         VALUES ('home', ?1, ?2, 0, ?3, ?4, 'UTC', 'UTC', 'CONFIRMED')",
        rusqlite::params![id, title, micros(start), micros(end)],
    )
    .unwrap();
}

fn fingerprint(dir: &Path) -> Vec<(String, u64, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, &mut files);
    let mut v: Vec<_> = files
        .into_iter()
        .map(|f| {
            let meta = std::fs::metadata(&f).unwrap();
            (
                f.display().to_string(),
                meta.len(),
                std::fs::read(&f).unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

fn rfc(d: chrono::DateTime<chrono::Utc>) -> String {
    d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[tokio::test]
async fn events_arrive_over_the_bus_and_changes_are_noticed_within_a_minute() {
    let dir = tempfile::tempdir().unwrap();
    let (profile, _files) = fake_profile(dir.path(), &["INBOX"]);
    let prefs = std::fs::read_to_string(profile.join("prefs.js")).unwrap();
    std::fs::write(
        profile.join("prefs.js"),
        format!(
            "{prefs}user_pref(\"calendar.registry.home.name\", \"Home\");\nuser_pref(\"calendar.registry.home.type\", \"storage\");\n"
        ),
    )
    .unwrap();
    let db_file = calendar_db(&profile);
    let soon = chrono::Utc::now() + chrono::Duration::days(3);
    add_event(
        &db_file,
        "dentist",
        "Dentist",
        &rfc(soon),
        &rfc(soon + chrono::Duration::hours(1)),
    );
    let before = fingerprint(&profile);

    let bus = Bus::start().await;
    let db_path = dir.path().join("belvedere.db");
    let mut cmd = bus.service_command(&db_path);
    cmd.env("BELVEDERE_TB_PROFILE", &profile);
    let service = cmd.spawn().unwrap();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;
    let mut changed = proxy.receive_calendar_changed().await.unwrap();

    // The startup reading.
    let started = Instant::now();
    let events = loop {
        let events = proxy.list_events("", "").await.unwrap();
        if !events.is_empty() {
            break events;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no events after startup"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].title, "Dentist");
    assert_eq!(events[0].calendar_id, "home");
    assert!(!events[0].all_day);
    let cals = proxy.list_calendars().await.unwrap();
    assert_eq!(cals.len(), 1);
    assert!(cals[0].readable);
    assert!(!proxy.calendar_read_at().await.unwrap().is_empty());
    // Drain the startup signal if it is still queued.
    let _ = tokio::time::timeout(Duration::from_millis(500), changed.next()).await;

    // Thunderbird adds an event: noticed within a minute.
    let later = soon + chrono::Duration::days(2);
    add_event(
        &db_file,
        "haircut",
        "Haircut",
        &rfc(later),
        &rfc(later + chrono::Duration::minutes(30)),
    );
    let started = Instant::now();
    let signal = tokio::time::timeout(Duration::from_secs(60), changed.next()).await;
    assert!(signal.is_ok(), "no CalendarChanged within a minute");
    eprintln!("change noticed after {:?}", started.elapsed());
    let titles: Vec<String> = proxy
        .list_events("", "")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.title)
        .collect();
    assert_eq!(titles, ["Dentist", "Haircut"]);

    // A window that holds neither.
    let far = rfc(chrono::Utc::now() + chrono::Duration::days(30));
    let farther = rfc(chrono::Utc::now() + chrono::Duration::days(40));
    assert!(proxy.list_events(&far, &farther).await.unwrap().is_empty());

    stop(service).await;
    // Only the database this test itself wrote to may differ.
    let after = fingerprint(&profile);
    let differing: Vec<&String> = after
        .iter()
        .filter(|(name, len, bytes)| {
            !before
                .iter()
                .any(|(n, l, b)| n == name && l == len && b == bytes)
        })
        .map(|(name, _, _)| name)
        .collect();
    assert!(
        differing.iter().all(|n| n.contains("local.sqlite")),
        "the service must not touch the profile: {differing:?}"
    );
}
