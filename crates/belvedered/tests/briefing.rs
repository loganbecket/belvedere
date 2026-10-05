//! The morning briefing against the real service: fires once at its time
//! (within a minute), lists the right things, opens a conversation with
//! the full text, and does not repeat after a restart.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use belvedere_core::db::{Db, NewTask};
use common::{fake_notifications, fake_profile, stop, wait_ready, Bus};

fn calendar_with_event_today(profile: &Path) {
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
    // Later today (an hour from now) and the day after tomorrow.
    let now = chrono::Utc::now();
    for (id, title, start) in [
        ("practice", "Practice", now + chrono::Duration::hours(1)),
        ("game", "Game", now + chrono::Duration::days(2)),
    ] {
        conn.execute(
            "INSERT INTO cal_events (cal_id, id, title, flags, event_start, event_end, event_start_tz, event_end_tz, ical_status)
             VALUES ('home', ?1, ?2, 0, ?3, ?4, 'UTC', 'UTC', 'CONFIRMED')",
            rusqlite::params![
                id,
                title,
                start.timestamp_micros(),
                (start + chrono::Duration::hours(1)).timestamp_micros()
            ],
        )
        .unwrap();
    }
}

#[tokio::test]
async fn briefs_once_at_its_time_with_the_right_items() {
    let dir = tempfile::tempdir().unwrap();
    let (profile, _files) = fake_profile(dir.path(), &["INBOX"]);
    let prefs = std::fs::read_to_string(profile.join("prefs.js")).unwrap();
    std::fs::write(
        profile.join("prefs.js"),
        format!("{prefs}user_pref(\"calendar.registry.home.name\", \"Home\");\nuser_pref(\"calendar.registry.home.type\", \"storage\");\n"),
    )
    .unwrap();
    // The hour-from-now event must still be today for the test to hold.
    let now_local = chrono::Local::now();
    if now_local.time() > chrono::NaiveTime::from_hms_opt(22, 50, 0).unwrap() {
        eprintln!("too close to midnight for this test; skipping");
        return;
    }
    calendar_with_event_today(&profile);

    let db_path = dir.path().join("belvedere.db");
    let due = |d: chrono::DateTime<chrono::Local>| Some(d.with_timezone(&chrono::Utc).to_rfc3339());
    {
        let db = Db::open(&db_path).unwrap();
        let today_noon = now_local.date_naive().and_hms_opt(23, 30, 0).unwrap();
        let tasks = [
            (
                "Pay the City Power bill",
                due(now_local - chrono::Duration::days(2)),
            ),
            (
                "Call the dentist",
                due(chrono::Local::now().with_time(today_noon.time()).unwrap()),
            ),
            (
                "Renew the permit",
                due(now_local + chrono::Duration::days(2)),
            ),
            ("Far away", due(now_local + chrono::Duration::days(20))),
        ];
        for (title, due_at) in tasks {
            db.create_task(&NewTask {
                title: title.into(),
                notes: String::new(),
                due_at,
            })
            .unwrap();
        }
        // The briefing time: the next whole minute.
        let next_minute = (now_local + chrono::Duration::minutes(1))
            .format("%H:%M")
            .to_string();
        db.set_setting("briefing_time", &next_minute).unwrap();
    }

    let bus = Bus::start().await;
    let (_daemon, shown) = fake_notifications(&bus).await;
    let spawn = || {
        let mut cmd = bus.service_command(&db_path);
        cmd.env("BELVEDERE_TB_PROFILE", &profile);
        cmd.env_remove("BELVEDERE_NO_BRIEFING");
        cmd.spawn().unwrap()
    };
    let service = spawn();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;

    // The briefing is ready immediately over the bus.
    let started = Instant::now();
    let (summary, text) = proxy.briefing().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(60));
    eprintln!("briefing in {:?}: {summary}\n{text}", started.elapsed());

    // It fires within a minute of its time.
    let deadline = Instant::now() + Duration::from_secs(125);
    let fired = loop {
        if let Some(s) = shown
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.summary == "Good morning")
            .cloned()
        {
            break s;
        }
        assert!(Instant::now() < deadline, "no briefing within two minutes");
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert!(
        fired.body.contains("task") && fired.body.contains("overdue"),
        "{}",
        fired.body
    );
    assert!(fired.body.contains("event"), "{}", fired.body);

    // The conversation holds the full text in the right order.
    let conversations = proxy.list_conversations().await.unwrap();
    let briefing = conversations
        .iter()
        .find(|c| c.title.starts_with("Morning briefing"))
        .expect("a briefing conversation");
    let messages = proxy.get_messages(briefing.id).await.unwrap();
    assert_eq!(messages.len(), 1);
    let full = &messages[0].content;
    let at = |s: &str| {
        full.find(s)
            .unwrap_or_else(|| panic!("{s:?} missing in {full}"))
    };
    assert!(at("Pay the City Power bill") < at("Call the dentist"));
    assert!(at("Call the dentist") < at("Practice"));
    assert!(at("Practice") < at("Renew the permit"));
    assert!(full.contains("Game"));
    assert!(!full.contains("Far away"));

    // A restart the same day does not brief again.
    stop(service).await;
    let service = spawn();
    let _ = wait_ready(&conn).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let count = shown
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.summary == "Good morning")
        .count();
    assert_eq!(count, 1, "one briefing per day");
    stop(service).await;
}
