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

#[tokio::test]
async fn complete_reopen_delete_restore_keep_everything() {
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let service = bus.spawn_service(&dir.path().join("belvedere.db"));
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;
    let mut changes = proxy.receive_tasks_changed().await.unwrap();

    let created = proxy
        .create_task(
            "Renew the passport",
            "Form DS-82",
            "2026-11-01T14:00:00.000Z",
        )
        .await
        .unwrap();
    changes.next().await;

    // Complete, then reopen.
    let done = proxy.complete_task(created.id).await.unwrap();
    assert_eq!(done.status, "done");
    assert!(!done.completed_at.is_empty());
    timeout(Duration::from_secs(2), changes.next())
        .await
        .expect("no TasksChanged after CompleteTask");
    let open = proxy.reopen_task(created.id).await.unwrap();
    assert_eq!(open.status, "open");
    assert!(open.completed_at.is_empty());
    timeout(Duration::from_secs(2), changes.next())
        .await
        .expect("no TasksChanged after ReopenTask");

    // Delete: hidden from the main list, shown in the deleted list.
    proxy.delete_task(created.id).await.unwrap();
    changes.next().await;
    assert!(proxy.list_tasks().await.unwrap().is_empty());
    let deleted = proxy.list_deleted_tasks().await.unwrap();
    assert_eq!(deleted.len(), 1);
    assert!(deleted[0].is_deleted());

    // Restore: back with title, notes, and due date intact.
    let restored = proxy.restore_task(created.id).await.unwrap();
    timeout(Duration::from_secs(2), changes.next())
        .await
        .expect("no TasksChanged after RestoreTask");
    assert!(!restored.is_deleted());
    assert_eq!(restored.title, "Renew the passport");
    assert_eq!(restored.notes, "Form DS-82");
    assert_eq!(restored.due_at, "2026-11-01T14:00:00.000Z");
    assert_eq!(restored.status, "open");
    assert_eq!(proxy.list_tasks().await.unwrap().len(), 1);
    assert!(proxy.list_deleted_tasks().await.unwrap().is_empty());

    // Restoring something that isn't deleted is an error, not a no-op.
    assert!(proxy.restore_task(created.id).await.is_err());

    stop(service).await;
}

#[tokio::test]
async fn chat_without_a_working_model_fails_cleanly_and_keeps_the_question() {
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    // The machine running this test may well have models installed, so
    // instead point the service at a helper that doesn't exist: any load
    // fails at once, which is the failure path we want to see handled.
    let mut cmd = bus.service_command(&dir.path().join("belvedere.db"));
    cmd.env("XDG_DATA_HOME", dir.path())
        .env("BELVEDERE_MODEL_HELPER", dir.path().join("no-such-helper"));
    let service = cmd.spawn().unwrap();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;

    assert!(proxy.list_conversations().await.unwrap().is_empty());
    let conversation = proxy.new_conversation().await.unwrap();
    assert_eq!(conversation.title, "");

    let mut failed = proxy.receive_chat_failed().await.unwrap();
    let request = proxy
        .send_message(conversation.id, "What's due today?")
        .await
        .unwrap();
    let signal = timeout(Duration::from_secs(30), failed.next())
        .await
        .expect("no ChatFailed")
        .unwrap();
    let args = signal.args().unwrap();
    assert_eq!(args.request, request);
    assert_eq!(args.conversation_id, conversation.id);
    assert!(
        args.message.contains("No model") || args.message.contains("Could not load"),
        "was: {}",
        args.message
    );

    // The question was saved and titled the conversation.
    let messages = proxy.get_messages(conversation.id).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].content, "What's due today?");
    let listed = proxy.list_conversations().await.unwrap();
    assert_eq!(listed[0].title, "What's due today?");

    // An empty message is refused.
    assert!(proxy.send_message(conversation.id, "   ").await.is_err());

    stop(service).await;
}

/// "Not needed" over the bus: the task is closed with the reason and a
/// closing date, its reminders are canceled, it stays on the list, and
/// Reopen brings it back.
#[tokio::test]
async fn dismiss_closes_with_a_reason_and_cancels_reminders() {
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("belvedere.db");
    let service = bus.spawn_service(&db_path);
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;
    let mut changes = proxy.receive_tasks_changed().await.unwrap();

    let task = proxy
        .create_task("Pay the water bill", "", "2099-11-28T14:00:00.000Z")
        .await
        .unwrap();
    let _ = timeout(Duration::from_secs(2), changes.next()).await;
    {
        let db = belvedere_core::db::Db::open(&db_path).unwrap();
        assert!(!db.task_reminders(task.id).unwrap().is_empty());
    }

    let dismissed = proxy.dismiss_task(task.id, "already paid").await.unwrap();
    assert_eq!(dismissed.status, "dismissed");
    assert_eq!(dismissed.dismiss_reason, "already paid");
    assert!(!dismissed.completed_at.is_empty(), "closing date recorded");
    assert!(
        timeout(Duration::from_secs(2), changes.next())
            .await
            .is_ok(),
        "TasksChanged after a dismissal"
    );
    {
        let db = belvedere_core::db::Db::open(&db_path).unwrap();
        assert!(db
            .task_reminders(task.id)
            .unwrap()
            .iter()
            .all(|r| r.fired_at.is_some()));
    }
    assert_eq!(
        proxy.list_tasks().await.unwrap().len(),
        1,
        "dismissed, not deleted"
    );

    // An empty reason still records one.
    let again = proxy.dismiss_task(task.id, "  ").await.unwrap();
    assert_eq!(again.dismiss_reason, "not needed");

    let reopened = proxy.reopen_task(task.id).await.unwrap();
    assert_eq!(reopened.status, "open");
    assert!(reopened.dismiss_reason.is_empty());
    stop(service).await;
}

/// Rules over the bus: add, edit, switch off, delete; the signal fires;
/// and what the mail reader would apply follows the list.
#[tokio::test]
async fn rules_round_trip_with_signal() {
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("belvedere.db");
    let service = bus.spawn_service(&db_path);
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;
    let mut changes = proxy.receive_rules_changed().await.unwrap();

    assert!(proxy.list_rules().await.unwrap().is_empty());
    let a = proxy
        .create_rule("  The car insurance is on autopay.  ")
        .await
        .unwrap();
    assert_eq!(a.text, "The car insurance is on autopay.");
    assert!(a.enabled);
    assert!(timeout(Duration::from_secs(2), changes.next())
        .await
        .is_ok());
    assert!(
        proxy.create_rule("   ").await.is_err(),
        "empty rule refused"
    );

    let b = proxy.create_rule("Ignore mail from Shoply.").await.unwrap();
    let b = proxy
        .update_rule(b.id, "Ignore all mail from Shoply.", false)
        .await
        .unwrap();
    assert!(!b.enabled);
    let listed = proxy.list_rules().await.unwrap();
    assert_eq!(listed.len(), 2, "disabled rules stay listed");
    assert_eq!(listed[1].text, "Ignore all mail from Shoply.");

    // What extraction would see: enabled, not deleted.
    {
        let db = belvedere_core::db::Db::open(&db_path).unwrap();
        let in_force: Vec<String> = db
            .list_rules()
            .unwrap()
            .into_iter()
            .filter(|r| r.enabled)
            .map(|r| r.text)
            .collect();
        assert_eq!(in_force, ["The car insurance is on autopay."]);
    }
    proxy.delete_rule(a.id).await.unwrap();
    assert_eq!(proxy.list_rules().await.unwrap().len(), 1);
    assert!(proxy.delete_rule(a.id).await.is_err(), "already deleted");
    {
        let db = belvedere_core::db::Db::open(&db_path).unwrap();
        assert!(
            db.list_rules().unwrap().iter().all(|r| !r.enabled),
            "nothing in force now"
        );
    }
    stop(service).await;
}

/// Model roles over the bus: pick the chat and background models, and
/// removing a model deletes its file only when Belvedere downloaded it.
#[tokio::test]
async fn model_roles_switch_and_removal_spares_other_apps_files() {
    use belvedere_core::db::{Db, ModelSource, NewModel};
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("belvedere.db");
    // Two model files: one Belvedere downloaded, one imported from elsewhere.
    let own_dir = dir.path().join("belvedere").join("models");
    std::fs::create_dir_all(&own_dir).unwrap();
    let ours = own_dir.join("ours.gguf");
    let theirs = dir.path().join("elsewhere.gguf");
    std::fs::write(&ours, b"gguf").unwrap();
    std::fs::write(&theirs, b"gguf").unwrap();
    let (a, b) = {
        let db = Db::open(&db_path).unwrap();
        let a = db
            .upsert_model(&NewModel {
                name: "Ours",
                path: &ours.to_string_lossy(),
                source: ModelSource::Belvedere,
                size_bytes: 4,
                quantization: "Q4",
                supports_tools: Some(true),
            })
            .unwrap();
        let b = db
            .upsert_model(&NewModel {
                name: "Theirs",
                path: &theirs.to_string_lossy(),
                source: ModelSource::Import,
                size_bytes: 4,
                quantization: "Q4",
                supports_tools: Some(false),
            })
            .unwrap();
        (a.id, b.id)
    };
    let mut cmd = bus.service_command(&db_path);
    cmd.env("XDG_DATA_HOME", dir.path()).env("HOME", dir.path());
    let service = cmd.spawn().unwrap();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;
    let mut changes = proxy.receive_models_changed().await.unwrap();

    // The machine may have system-wide Ollama models too; count only ours.
    let ours_listed = |models: Vec<belvedere_core::ipc::ModelDto>| {
        models
            .into_iter()
            .filter(|m| m.id == a || m.id == b)
            .count()
    };
    assert_eq!(ours_listed(proxy.list_models().await.unwrap()), 2);
    proxy.set_model_role("chat", b).await.unwrap();
    assert!(timeout(Duration::from_secs(2), changes.next())
        .await
        .is_ok());
    proxy.set_model_role("background", a).await.unwrap();
    assert_eq!(proxy.model_roles().await.unwrap(), (b, a));
    assert!(proxy.set_model_role("sidekick", a).await.is_err());

    // Removing the imported one leaves its file; removing ours deletes it.
    assert!(!proxy.forget_model(b).await.unwrap());
    assert!(theirs.is_file(), "another app's file stays on disk");
    assert!(proxy.forget_model(a).await.unwrap());
    assert!(!ours.is_file(), "Belvedere's own download is deleted");
    assert_eq!(ours_listed(proxy.list_models().await.unwrap()), 0);
    let roles = proxy.model_roles().await.unwrap();
    assert!(
        roles.0 != a && roles.0 != b && roles.1 != a && roles.1 != b,
        "roles no longer point at removed models: {roles:?}"
    );
    stop(service).await;
}

/// Imported models are usable and survive a restart; a non-model file is
/// refused with a clear message.
#[tokio::test]
async fn imported_models_survive_a_restart_and_junk_is_refused() {
    use belvedere_core::models::gguf::fixture::{gguf, V};
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("belvedere.db");
    let elsewhere = dir.path().join("Downloads");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let model = gguf(&[
        ("general.architecture", V::Str("qwen3")),
        ("general.name", V::Str("Tiny")),
    ]);
    std::fs::write(elsewhere.join("tiny-Q4_K_M.gguf"), &model).unwrap();
    std::fs::write(elsewhere.join("photo.jpg"), [0xff, 0xd8]).unwrap();
    let spawn = || {
        let mut cmd = bus.service_command(&db_path);
        cmd.env("XDG_DATA_HOME", dir.path()).env("HOME", dir.path());
        cmd.spawn().unwrap()
    };
    let service = spawn();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;

    let refused = proxy
        .import_model(&elsewhere.join("photo.jpg").to_string_lossy(), false)
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("not a GGUF model file"), "{refused}");

    let imported = proxy
        .import_model(&elsewhere.join("tiny-Q4_K_M.gguf").to_string_lossy(), false)
        .await
        .unwrap();
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0].source, "import");
    assert_eq!(imported[0].quantization, "Q4_K_M");
    proxy.set_model_role("chat", imported[0].id).await.unwrap();
    let copied = proxy
        .import_model(&elsewhere.to_string_lossy(), true)
        .await
        .unwrap();
    assert_eq!(copied.len(), 1);
    assert_eq!(copied[0].source, "belvedere");
    assert!(dir
        .path()
        .join("belvedere/models/tiny-Q4_K_M.gguf")
        .is_file());

    stop(service).await;
    let service = spawn();
    let proxy = wait_ready(&conn).await;
    let after: Vec<_> = proxy
        .list_models()
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.path.ends_with("tiny-Q4_K_M.gguf"))
        .collect();
    assert_eq!(
        after.len(),
        2,
        "both the reference and the copy survive: {after:?}"
    );
    assert_eq!(proxy.model_roles().await.unwrap().0, imported[0].id);
    stop(service).await;
}

/// Settings over the bus: listed with defaults, checked when set, back to
/// default when cleared, and kept across a restart.
#[tokio::test]
async fn settings_are_checked_take_effect_and_survive_a_restart() {
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("belvedere.db");
    let spawn = || bus.spawn_service(&db_path);
    let service = spawn();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;
    let mut changes = proxy.receive_settings_changed().await.unwrap();

    let all = proxy.list_settings().await.unwrap();
    let sweep = all.iter().find(|s| s.key == "sweep_minutes").unwrap();
    assert_eq!(
        (
            sweep.value.as_str(),
            sweep.default.as_str(),
            sweep.kind.as_str()
        ),
        ("5", "5", "int")
    );
    assert!(all
        .iter()
        .any(|s| s.key == "briefing_enabled" && s.value == "true"));

    let set = proxy.set_setting("sweep_minutes", " 2 ").await.unwrap();
    assert_eq!(set.value, "2");
    assert!(timeout(Duration::from_secs(2), changes.next())
        .await
        .is_ok());
    assert!(
        proxy.set_setting("sweep_minutes", "0").await.is_err(),
        "out of range"
    );
    assert!(proxy.set_setting("briefing_time", "25:61").await.is_err());
    assert!(proxy.set_setting("not_a_setting", "1").await.is_err());
    proxy.set_setting("briefing_enabled", "off").await.unwrap();
    proxy
        .set_setting("skipped_folder_roles", "Junk, Trash")
        .await
        .unwrap();
    let password_before = proxy.caldav_info().await.unwrap().2;
    let new_password = proxy.regenerate_caldav_password().await.unwrap();
    assert_ne!(new_password, password_before);
    assert_eq!(proxy.caldav_info().await.unwrap().2, new_password);

    // Takes effect without a restart: the stored values are what the
    // service reads on its next use.
    {
        let db = belvedere_core::db::Db::open(&db_path).unwrap();
        assert_eq!(
            db.get_setting("sweep_minutes").unwrap().as_deref(),
            Some("2")
        );
        assert_eq!(
            db.get_setting("skipped_folder_roles").unwrap().as_deref(),
            Some("junk,trash")
        );
        assert_eq!(
            db.get_setting("briefing_enabled").unwrap().as_deref(),
            Some("false")
        );
    }

    stop(service).await;
    let service = spawn();
    let proxy = wait_ready(&conn).await;
    let after = proxy.list_settings().await.unwrap();
    let get = |k: &str| after.iter().find(|s| s.key == k).unwrap().value.clone();
    assert_eq!(get("sweep_minutes"), "2");
    assert_eq!(get("briefing_enabled"), "false");
    assert_eq!(get("skipped_folder_roles"), "junk,trash");
    assert_eq!(proxy.caldav_info().await.unwrap().2, new_password);
    let reset = proxy.set_setting("sweep_minutes", "").await.unwrap();
    assert_eq!(reset.value, "5", "cleared means default");
    stop(service).await;
}

/// Conversations can be deleted one at a time or all at once, for good.
#[tokio::test]
async fn conversations_delete_one_or_all() {
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("belvedere.db");
    let service = bus.spawn_service(&db_path);
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;
    let a = proxy.new_conversation().await.unwrap();
    let b = proxy.new_conversation().await.unwrap();
    proxy.new_conversation().await.unwrap();
    assert_eq!(proxy.list_conversations().await.unwrap().len(), 3);

    proxy.delete_conversation(a.id).await.unwrap();
    let left: Vec<i64> = proxy
        .list_conversations()
        .await
        .unwrap()
        .into_iter()
        .map(|x| x.id)
        .collect();
    assert!(!left.contains(&a.id) && left.contains(&b.id));
    assert!(
        proxy.delete_conversation(a.id).await.is_err(),
        "already gone"
    );
    {
        let db = belvedere_core::db::Db::open(&db_path).unwrap();
        assert!(
            db.get_conversation(a.id).is_err(),
            "removed for good, not hidden"
        );
    }

    assert_eq!(proxy.delete_all_conversations().await.unwrap(), 2);
    assert!(proxy.list_conversations().await.unwrap().is_empty());
    stop(service).await;
}
