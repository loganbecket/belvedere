//! End to end: a task due in a moment produces a desktop notification,
//! and pressing its buttons changes the task. A fake notification daemon
//! stands in for COSMIC's on the private bus.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{stop, wait_ready, Bus};
use futures_util::StreamExt;
use tokio::time::timeout;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::Value;

/// One captured Notify call.
#[derive(Debug, Clone)]
struct Shown {
    id: u32,
    summary: String,
    body: String,
    actions: Vec<String>,
}

/// A stand-in for org.freedesktop.Notifications that records what it is
/// asked to show and lets the test press buttons.
struct FakeDaemon {
    shown: Arc<Mutex<Vec<Shown>>>,
    next_id: Mutex<u32>,
    notify_tx: tokio::sync::mpsc::UnboundedSender<Shown>,
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
        actions: Vec<&str>,
        _hints: HashMap<&str, Value<'_>>,
        _expire_timeout: i32,
    ) -> u32 {
        let mut next = self.next_id.lock().unwrap();
        *next += 1;
        let shown = Shown {
            id: *next,
            summary: summary.to_string(),
            body: body.to_string(),
            actions: actions.iter().map(|s| s.to_string()).collect(),
        };
        self.shown.lock().unwrap().push(shown.clone());
        let _ = self.notify_tx.send(shown);
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

async fn start_fake_daemon(
    bus: &Bus,
) -> (
    zbus::Connection,
    tokio::sync::mpsc::UnboundedReceiver<Shown>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let daemon = FakeDaemon {
        shown: Arc::new(Mutex::new(Vec::new())),
        next_id: Mutex::new(0),
        notify_tx: tx,
    };
    let conn = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .serve_at("/org/freedesktop/Notifications", daemon)
        .unwrap()
        .build()
        .await
        .unwrap();
    (conn, rx)
}

async fn press(daemon: &zbus::Connection, id: u32, key: &str) {
    let iface = daemon
        .object_server()
        .interface::<_, FakeDaemon>("/org/freedesktop/Notifications")
        .await
        .unwrap();
    FakeDaemon::action_invoked(iface.signal_emitter(), id, key)
        .await
        .unwrap();
}

fn in_seconds(secs: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[tokio::test]
async fn a_due_task_is_announced_and_its_buttons_work() {
    let bus = Bus::start().await;
    let (daemon, mut shown) = start_fake_daemon(&bus).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = bus.service_command(&dir.path().join("belvedere.db"));
    cmd.env("BELVEDERE_TICK_MS", "200");
    let service = cmd.spawn().unwrap();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;
    let mut changes = proxy.receive_tasks_changed().await.unwrap();

    // Due in two seconds, at a time that isn't 9:00, so a reminder is
    // planned for the due moment itself.
    let due = in_seconds(2);
    let task = proxy
        .create_task("Water the plants", "", &due)
        .await
        .unwrap();
    changes.next().await;

    // Fires within a few seconds of its time (the plan allows 10).
    let first = timeout(Duration::from_secs(8), shown.recv())
        .await
        .expect("no notification arrived")
        .unwrap();
    assert_eq!(first.summary, "Water the plants");
    assert!(first.body.starts_with("Due "), "body was {:?}", first.body);
    assert!(first.actions.contains(&"done".to_string()));
    assert!(first.actions.contains(&"snooze".to_string()));
    assert!(first.actions.contains(&"dismiss".to_string()));
    assert!(first.actions.contains(&"default".to_string()));

    // It does not fire again on later ticks.
    assert!(
        timeout(Duration::from_millis(800), shown.recv())
            .await
            .is_err(),
        "reminder fired twice"
    );

    // Snooze: the task stays open and nothing new shows now.
    press(&daemon, first.id, "snooze").await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(proxy.get_task(task.id).await.unwrap().status, "open");
    assert!(timeout(Duration::from_millis(500), shown.recv())
        .await
        .is_err());

    // A second task, and press Done on it.
    let other = proxy
        .create_task("Take out the trash", "", &in_seconds(1))
        .await
        .unwrap();
    let second = timeout(Duration::from_secs(8), shown.recv())
        .await
        .expect("second notification")
        .unwrap();
    assert_eq!(second.summary, "Take out the trash");
    press(&daemon, second.id, "done").await;
    let done = wait_status(&proxy, other.id, "done").await;
    assert!(!done.completed_at.is_empty());

    // A third, and press Not needed.
    let third_task = proxy
        .create_task("Return the library book", "", &in_seconds(1))
        .await
        .unwrap();
    let third = timeout(Duration::from_secs(8), shown.recv())
        .await
        .expect("third notification")
        .unwrap();
    press(&daemon, third.id, "dismiss").await;
    let dismissed = wait_status(&proxy, third_task.id, "dismissed").await;
    assert_eq!(dismissed.dismiss_reason, "not needed");

    // Clicking the body asks windows to show the task.
    let fourth_task = proxy
        .create_task("Call the plumber", "", &in_seconds(1))
        .await
        .unwrap();
    let fourth = timeout(Duration::from_secs(8), shown.recv())
        .await
        .expect("fourth notification")
        .unwrap();
    let mut show = proxy.receive_show_task().await.unwrap();
    press(&daemon, fourth.id, "default").await;
    let signal = timeout(Duration::from_secs(5), show.next())
        .await
        .expect("no ShowTask signal")
        .unwrap();
    assert_eq!(signal.args().unwrap().id, fourth_task.id);

    stop(service).await;
}

async fn wait_status(
    proxy: &belvedere_core::ipc::ServiceProxy<'_>,
    id: i64,
    status: &str,
) -> belvedere_core::ipc::TaskDto {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let task = proxy.get_task(id).await.unwrap();
        if task.status == status {
            return task;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "task {id} never became {status}, is {}",
            task.status
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_reminder_missed_while_down_fires_once_at_startup_marked_late() {
    let bus = Bus::start().await;
    let (_daemon, mut shown) = start_fake_daemon(&bus).await;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("belvedere.db");

    // Plant a reminder in the past directly in the database, as if the
    // machine had been off when it came due.
    {
        let db = belvedere_core::db::Db::open(&db_path).unwrap();
        let task = db
            .create_task(&belvedere_core::db::NewTask {
                title: "Pick up the dry cleaning".into(),
                notes: String::new(),
                due_at: Some("2026-01-05T15:00:00.000Z".into()),
            })
            .unwrap();
        db.create_reminder(task.id, "2026-01-05T15:00:00.000Z")
            .unwrap();
    }

    let mut cmd = bus.service_command(&db_path);
    cmd.env("BELVEDERE_TICK_MS", "200");
    let service = cmd.spawn().unwrap();
    let conn = bus.connect().await;
    wait_ready(&conn).await;

    let late = timeout(Duration::from_secs(8), shown.recv())
        .await
        .expect("missed reminder was not announced at startup")
        .unwrap();
    assert_eq!(late.summary, "Pick up the dry cleaning");
    assert!(
        late.body.starts_with("Missed reminder"),
        "body was {:?}",
        late.body
    );
    assert!(
        timeout(Duration::from_secs(1), shown.recv()).await.is_err(),
        "missed reminder fired twice"
    );

    // Restart: still not again.
    stop(service).await;
    let mut cmd = bus.service_command(&db_path);
    cmd.env("BELVEDERE_TICK_MS", "200");
    let service = cmd.spawn().unwrap();
    wait_ready(&conn).await;
    assert!(
        timeout(Duration::from_secs(1), shown.recv()).await.is_err(),
        "missed reminder fired again after restart"
    );
    stop(service).await;
}
