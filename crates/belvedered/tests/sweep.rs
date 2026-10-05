//! The five-minute sweep: the real service with folder watching turned
//! off, so only the sweep can notice new mail.

mod common;

use std::time::{Duration, Instant};

use belvedere_core::db::{Db, NewMailMessage, NewTask, SourceKind};
use common::{append, cpu_time, fake_profile, mbox_message, stop, wait_ready, Bus};

/// Sweep every few seconds in tests instead of every five minutes.
const SWEEP_SECS: u64 = 4;

/// Plenty for one sweep plus the scan it triggers.
const ONE_SWEEP: Duration = Duration::from_secs(SWEEP_SECS * 3 + 5);

/// No model needed: a sent reply closes the "reply needed" task made
/// from the email it answers, and leaves a bill task alone.
#[tokio::test]
async fn a_sent_reply_closes_the_reply_needed_task_within_one_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let (profile, files) = fake_profile(dir.path(), &["INBOX", "Sent"]);
    let sent = &files[1];
    let db_path = dir.path().join("belvedere.db");

    // Two tasks already made from mail: a question to answer, a bill.
    let (reply_task, bill_task) = {
        let db = Db::open(&db_path).unwrap();
        let mut seeded = Vec::new();
        for (id, title, kind) in [
            ("q1", "Reply to Pat about the field trip", "reply_needed"),
            ("b1", "Pay the water bill", "bill"),
        ] {
            let message_id = format!("<{id}@example.invalid>");
            // Already read by the model back when the tasks were made.
            let (mail, _) = db
                .record_mail(&NewMailMessage {
                    message_id: message_id.clone(),
                    account: "someone@example.invalid".into(),
                    folder: "INBOX".into(),
                    subject: title.into(),
                    mbox_path: files[0].to_string_lossy().into_owned(),
                    ..Default::default()
                })
                .unwrap();
            db.mark_mail_processed(mail.id).unwrap();
            let task = db
                .create_task(&NewTask {
                    title: title.into(),
                    ..Default::default()
                })
                .unwrap();
            db.add_task_source(task.id, SourceKind::Email, &message_id, title)
                .unwrap();
            db.set_task_kind(task.id, kind, "").unwrap();
            db.create_reminder(task.id, "2099-01-01T09:00:00.000Z")
                .unwrap();
            seeded.push(task.id);
        }
        (seeded[0], seeded[1])
    };

    let bus = Bus::start().await;
    let mut cmd = bus.service_command(&db_path);
    cmd.env("BELVEDERE_TB_PROFILE", &profile)
        // No models anywhere: closing a replied task must not need one.
        .env("XDG_DATA_HOME", dir.path())
        .env("HOME", dir.path())
        .env("BELVEDERE_NO_MAIL_WATCH", "1")
        .env("BELVEDERE_SWEEP_SECS", SWEEP_SECS.to_string());
    let service = cmd.spawn().unwrap();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;

    // Replies to both land in Sent after the service is up.
    let now = chrono::Utc::now().to_rfc2822();
    append(
        sent,
        &mbox_message(
            "s1",
            "me@example.invalid",
            "Re: Field trip",
            "Yes, count us in.",
            &now,
            Some("q1"),
        ),
    );
    append(
        sent,
        &mbox_message(
            "s2",
            "me@example.invalid",
            "Re: Water bill",
            "Is autopay available?",
            &now,
            Some("b1"),
        ),
    );

    let started = Instant::now();
    loop {
        let t = proxy.get_task(reply_task).await.unwrap();
        if t.status == "done" {
            eprintln!("reply task closed after {:?}", started.elapsed());
            break;
        }
        assert!(
            started.elapsed() < ONE_SWEEP,
            "the reply-needed task was not closed within one sweep"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(proxy.get_task(bill_task).await.unwrap().status, "open");
    {
        let db = Db::open(&db_path).unwrap();
        assert!(db
            .task_reminders(reply_task)
            .unwrap()
            .iter()
            .all(|r| r.fired_at.is_some()));
        assert!(db
            .task_reminders(bill_task)
            .unwrap()
            .iter()
            .all(|r| r.fired_at.is_none()));
    }
    stop(service).await;
}

/// With watching off, a new message still becomes a task within one
/// sweep; sweeps with nothing new cost almost no CPU and never load the
/// model. Needs `BELVEDERE_TEST_EXTRACT_MODEL` (or `BELVEDERE_TEST_MODEL`).
#[tokio::test]
async fn the_sweep_alone_turns_new_mail_into_a_task_and_is_cheap_when_idle() {
    let Some(model) = std::env::var_os("BELVEDERE_TEST_EXTRACT_MODEL")
        .or_else(|| std::env::var_os("BELVEDERE_TEST_MODEL"))
    else {
        eprintln!("BELVEDERE_TEST_MODEL not set; skipping");
        return;
    };
    let helper =
        std::path::Path::new(env!("CARGO_BIN_EXE_belvedered")).with_file_name("belvedere-model");
    if !helper.is_file() {
        eprintln!("{} not built; skipping", helper.display());
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (profile, files) = fake_profile(dir.path(), &["INBOX"]);
    let inbox = &files[0];
    let models = dir.path().join("belvedere").join("models");
    std::fs::create_dir_all(&models).unwrap();
    std::os::unix::fs::symlink(&model, models.join("test-model.gguf")).unwrap();
    let db_path = dir.path().join("belvedere.db");

    let bus = Bus::start().await;
    let mut cmd = bus.service_command(&db_path);
    cmd.env("BELVEDERE_TB_PROFILE", &profile)
        .env("XDG_DATA_HOME", dir.path())
        .env("HOME", dir.path())
        .env("BELVEDERE_NO_MAIL_WATCH", "1")
        .env("BELVEDERE_SWEEP_SECS", SWEEP_SECS.to_string())
        .env("BELVEDERE_MODEL_HELPER", &helper);
    let service = cmd.spawn().unwrap();
    let pid = service.id().unwrap();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;

    // Several sweeps with nothing new: under 2 s of CPU in total, and the
    // model is never loaded.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let before = cpu_time(pid);
    tokio::time::sleep(Duration::from_secs(SWEEP_SECS * 3)).await;
    let spent = cpu_time(pid) - before;
    eprintln!("three idle sweeps used {spent:?} of CPU");
    assert!(spent < Duration::from_secs(2), "idle sweeps used {spent:?}");
    let (state, _) = proxy.model_status().await.unwrap();
    assert_eq!(state, "unloaded", "idle sweeps must not load the model");
    assert!(proxy.list_tasks().await.unwrap().is_empty());

    // A bill arrives; nobody is watching the file, so only the sweep
    // can find it.
    let due = (chrono::Local::now() + chrono::Days::new(20)).date_naive();
    append(
        inbox,
        &mbox_message(
            "bill-1",
            "City Power <billing@citypower.invalid>",
            "Your statement is ready",
            &format!(
                "Amount due: $84.12\nDue date: {}\n\nPay online or by mail.",
                due.format("%B %-d, %Y")
            ),
            &chrono::Utc::now().to_rfc2822(),
            None,
        ),
    );
    let started = Instant::now();
    let task = loop {
        let tasks = proxy.list_tasks().await.unwrap();
        if let Some(t) = tasks.first() {
            break t.clone();
        }
        // One sweep to notice it, then the model's time to read it.
        assert!(
            started.elapsed() < ONE_SWEEP + Duration::from_secs(90),
            "no task appeared from the sweep"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    eprintln!("task after {:?}: {}", started.elapsed(), task.title);
    assert!(task.notes.contains("$84.12"), "notes: {}", task.notes);
    assert_eq!(task.source_kind, "email");
    stop(service).await;
}

/// More than one batch of mail in skipped folders: all of it is handled at
/// startup without waiting for new mail (no model needed for these).
#[tokio::test]
async fn a_backlog_bigger_than_one_batch_is_worked_through_without_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let (profile, files) = fake_profile(dir.path(), &["Trash"]);
    let now = chrono::Utc::now().to_rfc2822();
    let mut text = String::new();
    for i in 0..130 {
        text.push_str(&mbox_message(
            &format!("old-{i}"),
            "someone@example.invalid",
            &format!("Deleted note {i}"),
            "Nothing to do.",
            &now,
            None,
        ));
    }
    append(&files[0], &text);
    let db_path = dir.path().join("belvedere.db");
    let bus = Bus::start().await;
    let mut cmd = bus.service_command(&db_path);
    cmd.env("BELVEDERE_TB_PROFILE", &profile)
        .env("XDG_DATA_HOME", dir.path())
        .env("HOME", dir.path())
        .env("BELVEDERE_NO_MAIL_WATCH", "1");
    let service = cmd.spawn().unwrap();
    let conn = bus.connect().await;
    let _proxy = wait_ready(&conn).await;
    let started = Instant::now();
    loop {
        let (seen, waiting) = {
            let db = Db::open(&db_path).unwrap();
            (
                db.mail_count().unwrap(),
                db.unprocessed_mail_count().unwrap(),
            )
        };
        if seen == 130 && waiting == 0 {
            eprintln!("130 messages handled after {:?}", started.elapsed());
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "backlog stalled: {seen} seen, {waiting} still waiting"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    stop(service).await;
}
