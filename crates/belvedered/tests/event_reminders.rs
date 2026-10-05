//! Event reminders against the real service: fires within 10 seconds of
//! its time, defers to a Thunderbird alarm, and does not fire twice
//! across a restart.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{fake_notifications, fake_profile, stop, wait_ready, Bus};

fn micros(d: chrono::DateTime<chrono::Utc>) -> i64 {
    d.timestamp_micros()
}

fn calendar_db(profile: &Path) -> rusqlite::Connection {
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
         CREATE TABLE cal_alarms (cal_id TEXT, item_id TEXT, recurrence_id INTEGER, recurrence_id_tz TEXT, icalString TEXT);
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

fn add_event(
    conn: &rusqlite::Connection,
    id: &str,
    title: &str,
    start: chrono::DateTime<chrono::Utc>,
) {
    conn.execute(
        "INSERT INTO cal_events (cal_id, id, title, flags, event_start, event_end, event_start_tz, event_end_tz, ical_status)
         VALUES ('home', ?1, ?2, 0, ?3, ?4, 'UTC', 'UTC', 'CONFIRMED')",
        rusqlite::params![id, title, micros(start), micros(start + chrono::Duration::hours(1))],
    )
    .unwrap();
}

#[tokio::test]
async fn reminds_on_time_defers_to_thunderbird_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (profile, _files) = fake_profile(dir.path(), &["INBOX"]);
    let prefs = std::fs::read_to_string(profile.join("prefs.js")).unwrap();
    std::fs::write(
        profile.join("prefs.js"),
        format!("{prefs}user_pref(\"calendar.registry.home.name\", \"Home\");\nuser_pref(\"calendar.registry.home.type\", \"storage\");\n"),
    )
    .unwrap();
    let conn = calendar_db(&profile);
    // Two events 15 minutes and 20 seconds away: the reminder (15 minutes
    // before) is due in 20 seconds. One has a Thunderbird alarm.
    let start = chrono::Utc::now() + chrono::Duration::minutes(15) + chrono::Duration::seconds(20);
    add_event(&conn, "practice", "Practice", start);
    add_event(&conn, "dentist", "Dentist", start);
    conn.execute(
        "INSERT INTO cal_alarms (cal_id, item_id, icalString) VALUES ('home', 'dentist', 'BEGIN:VALARM\nACTION:DISPLAY\nTRIGGER;VALUE=DURATION:-PT10M\nEND:VALARM')",
        [],
    )
    .unwrap();
    drop(conn);
    let expected_fire = start - chrono::Duration::minutes(15);

    let bus = Bus::start().await;
    let (_daemon, shown) = fake_notifications(&bus).await;
    let db_path = dir.path().join("belvedere.db");
    let spawn = || {
        let mut cmd = bus.service_command(&db_path);
        cmd.env("BELVEDERE_TB_PROFILE", &profile);
        cmd.spawn().unwrap()
    };
    let service = spawn();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;

    // Wait for the reminder.
    let deadline = Instant::now() + Duration::from_secs(60);
    let fired_at = loop {
        if let Some(s) = shown.lock().unwrap().first() {
            break s.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no event reminder within a minute"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let fired_wall =
        chrono::Utc::now() - chrono::Duration::from_std(fired_at.at.elapsed()).unwrap();
    let off = (fired_wall - expected_fire).num_seconds();
    eprintln!(
        "reminder fired {off} s after its time: {} / {}",
        fired_at.summary, fired_at.body
    );
    assert!((0..=10).contains(&off), "fired {off} s off its time");
    assert_eq!(fired_at.summary, "Practice");
    assert!(fired_at.body.starts_with("Starts at"), "{}", fired_at.body);

    // Only Practice: Dentist has a Thunderbird alarm.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let all: Vec<String> = shown
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.summary.clone())
        .collect();
    assert_eq!(all, ["Practice"], "Thunderbird reminds about the dentist");

    // Restart: the fired reminder stays fired.
    stop(service).await;
    let service = spawn();
    let _proxy = wait_ready(&conn).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let all: Vec<String> = shown
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.summary.clone())
        .collect();
    assert_eq!(all, ["Practice"], "no second reminder after a restart");
    assert!(proxy.list_events("", "").await.unwrap().len() == 2);
    stop(service).await;
}
