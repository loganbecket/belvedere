//! Mail becomes tasks, end to end: the real service, a made-up profile, a
//! real (small) model, and a stand-in notification daemon. Needs
//! `BELVEDERE_TEST_MODEL`; skips otherwise.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{append, fake_profile as shared_profile, mbox_message, stop, wait_ready, Bus};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::Value;

#[derive(Debug, Clone)]
struct Shown {
    summary: String,
    body: String,
}

struct FakeDaemon {
    shown: Arc<Mutex<Vec<Shown>>>,
    next_id: Mutex<u32>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl FakeDaemon {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        _app_name: &str,
        _replaces_id: u32,
        _app_icon: &str,
        summary: &str,
        body: &str,
        _actions: Vec<&str>,
        _hints: HashMap<&str, Value<'_>>,
        _expire_timeout: i32,
    ) -> u32 {
        let mut next = self.next_id.lock().unwrap();
        *next += 1;
        self.shown.lock().unwrap().push(Shown {
            summary: summary.to_string(),
            body: body.to_string(),
        });
        *next
    }

    fn close_notification(&self, _id: u32) {}

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;
}

fn message(id: &str, from: &str, subject: &str, body: &str, date: &str) -> String {
    mbox_message(id, from, subject, body, date, None)
}

/// A profile with one IMAP account whose INBOX and Junk start empty.
fn fake_profile(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let (profile, files) = shared_profile(root, &["INBOX", "Junk"]);
    (profile, files[0].clone(), files[1].clone())
}

/// Reading mail well takes a better model than the tiny one the quick
/// tests use; `BELVEDERE_TEST_EXTRACT_MODEL` names it (the 4B default is
/// right), falling back to `BELVEDERE_TEST_MODEL`.
fn test_setup() -> Option<(PathBuf, PathBuf)> {
    let model = std::env::var_os("BELVEDERE_TEST_EXTRACT_MODEL")
        .or_else(|| std::env::var_os("BELVEDERE_TEST_MODEL"))?;
    let helper = Path::new(env!("CARGO_BIN_EXE_belvedered")).with_file_name("belvedere-model");
    if !helper.is_file() {
        eprintln!("{} not built; skipping", helper.display());
        return None;
    }
    Some((PathBuf::from(model), helper))
}

#[tokio::test]
async fn a_bill_email_becomes_a_task_with_a_notification() {
    let Some((model, helper)) = test_setup() else {
        eprintln!("BELVEDERE_TEST_MODEL not set; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (profile, inbox, junk) = fake_profile(dir.path());
    let models = dir.path().join("belvedere").join("models");
    std::fs::create_dir_all(&models).unwrap();
    std::os::unix::fs::symlink(&model, models.join("test-model.gguf")).unwrap();

    let bus = Bus::start().await;
    let shown = Arc::new(Mutex::new(Vec::new()));
    let _daemon = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .serve_at(
            "/org/freedesktop/Notifications",
            FakeDaemon {
                shown: shown.clone(),
                next_id: Mutex::new(0),
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let db_path = dir.path().join("belvedere.db");
    let mut cmd = bus.service_command(&db_path);
    cmd.env("BELVEDERE_TB_PROFILE", &profile)
        .env("XDG_DATA_HOME", dir.path())
        .env("HOME", dir.path())
        .env("BELVEDERE_MODEL_HELPER", &helper);
    let service = cmd.spawn().unwrap();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;

    // A bill lands in the inbox; junk gets a "bill" too, which must be ignored.
    let now = chrono::Utc::now();
    let due = (chrono::Local::now() + chrono::Days::new(27)).date_naive();
    append(
        &inbox,
        &message(
            "bill-1",
            "City Power <billing@citypower.invalid>",
            "Your October statement is ready",
            &format!(
                "Account 4471-02\n\nAmount due: $84.12\nDue date: {}\n\nPay online or by mail.",
                due.format("%B %-d, %Y")
            ),
            &now.to_rfc2822(),
        ),
    );
    append(
        &junk,
        &message(
            "spam-1",
            "Totally Real Bank <x@example.invalid>",
            "URGENT: pay $9,999 now",
            "Your account will be closed unless you pay $9,999 by tomorrow.",
            &now.to_rfc2822(),
        ),
    );

    // Within two minutes: one task, with the right amount and date, and a notification.
    let started = Instant::now();
    let task = loop {
        let tasks = proxy.list_tasks().await.unwrap();
        if let Some(t) = tasks.first() {
            break t.clone();
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "no task appeared within two minutes"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    eprintln!(
        "task after {:?}: {} / {}",
        started.elapsed(),
        task.title,
        task.notes
    );
    assert!(task.notes.contains("$84.12"), "notes: {}", task.notes);
    let due_day = due.format("%Y-%m-%d").to_string();
    let next_day = (due + chrono::Days::new(1)).format("%Y-%m-%d").to_string();
    assert!(
        task.due_at().unwrap_or("").starts_with(&due_day)
            || task.due_at().unwrap_or("").starts_with(&next_day),
        "due: {:?}, wanted {due_day}",
        task.due_at()
    );
    assert_eq!(task.source_kind, "email");
    assert_eq!(task.source_label, "Your October statement is ready");
    assert_eq!(
        proxy.list_tasks().await.unwrap().len(),
        1,
        "the junk mail must not become a task"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let shown = shown.lock().unwrap().clone();
        if let Some(n) = shown.first() {
            assert_eq!(n.summary, task.title);
            assert!(
                n.body.starts_with("New task from your mail"),
                "body: {}",
                n.body
            );
            break;
        }
        drop(shown);
        assert!(
            Instant::now() < deadline,
            "no notification for the new task"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Opening the email is wired (the test machine may have no Thunderbird;
    // either way the method must not fail for the wrong reason).
    match proxy.open_email(task.id).await {
        Ok(()) => {}
        Err(e) => assert!(
            e.to_string().contains("Thunderbird") || e.to_string().contains("profile"),
            "{e}"
        ),
    }
    assert!(
        proxy.open_email(task.id + 100).await.is_err(),
        "a task without an email source has nothing to open"
    );

    stop(service).await;
}

#[tokio::test]
async fn unsure_results_become_suggestions_that_accept_or_reject() {
    let Some((model, helper)) = test_setup() else {
        eprintln!("BELVEDERE_TEST_MODEL not set; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (profile, inbox, _junk) = fake_profile(dir.path());
    let models = dir.path().join("belvedere").join("models");
    std::fs::create_dir_all(&models).unwrap();
    std::os::unix::fs::symlink(&model, models.join("test-model.gguf")).unwrap();
    let db_path = dir.path().join("belvedere.db");
    // Make everything a suggestion: no confidence reaches 1.1.
    {
        let db = belvedere_core::db::Db::open(&db_path).unwrap();
        db.set_setting("extract_confidence_threshold", "1.1")
            .unwrap();
    }

    let bus = Bus::start().await;
    let spawn = || {
        let mut cmd = bus.service_command(&db_path);
        cmd.env("BELVEDERE_TB_PROFILE", &profile)
            .env("XDG_DATA_HOME", dir.path())
            .env("HOME", dir.path())
            .env("BELVEDERE_MODEL_HELPER", &helper);
        cmd.spawn().unwrap()
    };
    let service = spawn();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;

    let now = chrono::Utc::now();
    append(
        &inbox,
        &message(
            "a",
            "Metro Water <bills@metrowater.invalid>",
            "Water bill due",
            "Your water bill of $41.50 is due on 11/28/2099.",
            &now.to_rfc2822(),
        ),
    );
    append(
        &inbox,
        &message(
            "b",
            "Shield Auto <billing@shieldauto.invalid>",
            "Premium due",
            "Your premium of $612.40 is due by Dec 15, 2099.",
            &now.to_rfc2822(),
        ),
    );

    let started = Instant::now();
    let suggestions = loop {
        let s = proxy.list_suggestions().await.unwrap();
        if s.len() >= 2 {
            break s;
        }
        assert!(
            started.elapsed() < Duration::from_secs(180),
            "suggestions did not appear; have {}",
            s.len()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert!(
        proxy.list_tasks().await.unwrap().is_empty(),
        "nothing becomes a task outright at this threshold"
    );

    // Accept one: it becomes a task linked to its email; reject the other.
    let (a, b) = (&suggestions[0], &suggestions[1]);
    let task = proxy.accept_suggestion(a.id).await.unwrap();
    assert_eq!(task.title, a.title);
    assert_eq!(task.source_kind, "email");
    proxy.reject_suggestion(b.id).await.unwrap();
    assert!(proxy.list_suggestions().await.unwrap().is_empty());
    assert_eq!(proxy.list_tasks().await.unwrap().len(), 1);
    assert!(
        proxy.accept_suggestion(b.id).await.is_err(),
        "a rejected suggestion cannot be accepted later"
    );

    // Restart: the rejected one does not come back, nothing is reprocessed.
    stop(service).await;
    let service = spawn();
    let proxy = wait_ready(&conn).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(proxy.list_suggestions().await.unwrap().is_empty());
    let tasks = proxy.list_tasks().await.unwrap();
    assert_eq!(
        tasks.len(),
        1,
        "tasks after restart: {:?}",
        tasks
            .iter()
            .map(|t| (t.id, t.title.clone(), t.source_label.clone()))
            .collect::<Vec<_>>()
    );
    stop(service).await;
}
