//! Drives the real service binary over a private D-Bus session: every
//! method, the signal, and survival across a restart.

use std::process::Stdio;
use std::time::Duration;

use belvedere_core::ipc::ServiceProxy;
use futures_util::StreamExt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// A throwaway session bus. Dies with the struct.
struct Bus {
    daemon: Child,
    address: String,
}

impl Bus {
    async fn start() -> Self {
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("dbus-daemon must be installed");
        let stdout = daemon.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let address = timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("dbus-daemon printed no address in time")
            .unwrap()
            .expect("dbus-daemon closed stdout");
        Bus { daemon, address }
    }

    async fn connect(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap()
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.start_kill();
    }
}

/// The service binary, pointed at a private bus and a private database.
fn spawn_service(bus: &Bus, db_path: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_belvedered"))
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env("BELVEDERE_DB", db_path)
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

/// Waits until the service answers Ping.
async fn wait_ready(conn: &zbus::Connection) -> ServiceProxy<'_> {
    let proxy = ServiceProxy::new(conn).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(reply) = proxy.ping().await {
            assert_eq!(reply, "pong");
            return proxy;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "service never answered Ping"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn stop(mut child: Child) {
    let pid = child.id().unwrap() as i32;
    unsafe { libc_kill(pid) };
    timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("service did not exit after SIGTERM")
        .unwrap();
}

// Avoid a `nix` dependency just for one call.
unsafe fn libc_kill(pid: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, 15);
}

#[tokio::test]
async fn every_method_and_the_signal() {
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("belvedere.db");
    let service = spawn_service(&bus, &db_path);
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;

    // Version
    assert_eq!(proxy.version().await.unwrap(), belvedere_core::VERSION);

    // Subscribe to the signal before making changes.
    let mut changes = proxy.receive_tasks_changed().await.unwrap();

    // ListTasks on an empty database
    assert!(proxy.list_tasks().await.unwrap().is_empty());

    // CreateTask
    let created = proxy
        .create_task(
            "Pay the electric bill",
            "$84.12",
            "2026-10-20T00:00:00.000Z",
        )
        .await
        .unwrap();
    assert_eq!(created.title, "Pay the electric bill");
    assert_eq!(created.status, "open");
    assert_eq!(created.due_at(), Some("2026-10-20T00:00:00.000Z"));
    timeout(Duration::from_secs(2), changes.next())
        .await
        .expect("no TasksChanged after CreateTask")
        .unwrap();

    // GetTask
    let fetched = proxy.get_task(created.id).await.unwrap();
    assert_eq!(fetched, created);

    // GetTask for a missing id is an error, not a panic or an empty task.
    assert!(proxy.get_task(created.id + 100).await.is_err());

    // UpdateTask
    let updated = proxy
        .update_task(created.id, "Pay the gas bill", "", "")
        .await
        .unwrap();
    assert_eq!(updated.title, "Pay the gas bill");
    assert_eq!(updated.notes, "");
    assert_eq!(updated.due_at(), None);
    timeout(Duration::from_secs(2), changes.next())
        .await
        .expect("no TasksChanged after UpdateTask")
        .unwrap();

    // ListTasks shows it
    let listed = proxy.list_tasks().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.id);

    // DeleteTask is soft: gone from the list, still fetchable, marked deleted.
    let deleted = proxy.delete_task(created.id).await.unwrap();
    assert!(deleted.is_deleted());
    timeout(Duration::from_secs(2), changes.next())
        .await
        .expect("no TasksChanged after DeleteTask")
        .unwrap();
    assert!(proxy.list_tasks().await.unwrap().is_empty());
    assert!(proxy.get_task(created.id).await.unwrap().is_deleted());

    stop(service).await;
}

#[tokio::test]
async fn tasks_survive_a_service_restart() {
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("belvedere.db");
    let conn = bus.connect().await;

    let service = spawn_service(&bus, &db_path);
    let proxy = wait_ready(&conn).await;
    let created = proxy
        .create_task("Renew the car registration", "", "")
        .await
        .unwrap();
    stop(service).await;

    // Gone from the bus...
    assert!(proxy.ping().await.is_err());

    // ...and back, with the same task.
    let service = spawn_service(&bus, &db_path);
    let proxy = wait_ready(&conn).await;
    let listed = proxy.list_tasks().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.id);
    assert_eq!(listed[0].title, "Renew the car registration");
    stop(service).await;
}
