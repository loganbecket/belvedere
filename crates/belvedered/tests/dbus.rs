//! Drives the real service binary over a private D-Bus session: every
//! method, the signal, and survival across a restart.

mod common;

use std::time::Duration;

use common::{stop, wait_ready, Bus};
use futures_util::StreamExt;
use tokio::time::timeout;

#[tokio::test]
async fn every_method_and_the_signal() {
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("belvedere.db");
    let service = bus.spawn_service(&db_path);
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

    let service = bus.spawn_service(&db_path);
    let proxy = wait_ready(&conn).await;
    let created = proxy
        .create_task("Renew the car registration", "", "")
        .await
        .unwrap();
    stop(service).await;

    // Gone from the bus...
    assert!(proxy.ping().await.is_err());

    // ...and back, with the same task.
    let service = bus.spawn_service(&db_path);
    let proxy = wait_ready(&conn).await;
    let listed = proxy.list_tasks().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.id);
    assert_eq!(listed[0].title, "Renew the car registration");
    stop(service).await;
}

#[tokio::test]
async fn a_second_instance_cannot_steal_the_name() {
    // Two services on one bus: the second must not take over the name and
    // knock the first off the air. This is what happened once when a test
    // ran against the real session bus.
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let conn = bus.connect().await;

    let first = bus.spawn_service(&dir.path().join("first.db"));
    let proxy = wait_ready(&conn).await;
    proxy
        .create_task("Only in the first", "", "")
        .await
        .unwrap();

    let mut second = bus.spawn_service(&dir.path().join("second.db"));
    // The second instance must give up rather than run without its name.
    let status = timeout(Duration::from_secs(10), second.wait())
        .await
        .expect("second instance kept running without owning the name")
        .unwrap();
    assert!(!status.success());

    // The first is still the one answering.
    let listed = proxy.list_tasks().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "Only in the first");
    stop(first).await;
}
