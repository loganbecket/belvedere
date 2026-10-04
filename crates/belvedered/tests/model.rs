//! Loads a real model and generates text through the bus. Needs a model
//! file, so it only runs when `BELVEDERE_TEST_MODEL` points at a GGUF;
//! otherwise it passes without doing anything (CI has no models).

mod common;

use std::time::Duration;

use common::{stop, wait_ready, Bus};
use futures_util::StreamExt;
use tokio::time::timeout;

#[tokio::test]
async fn load_generate_unload_through_the_bus() {
    let Some(model_path) = std::env::var_os("BELVEDERE_TEST_MODEL") else {
        eprintln!("BELVEDERE_TEST_MODEL not set; skipping");
        return;
    };
    let model_path = std::path::PathBuf::from(model_path);

    // Put the model where the service's scan will find it: Belvedere's own
    // model folder, under a private XDG_DATA_HOME.
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("belvedere").join("models");
    std::fs::create_dir_all(&models).unwrap();
    std::os::unix::fs::symlink(&model_path, models.join("test-model.gguf")).unwrap();

    // The helper is a separate package, so cargo doesn't hand us its path;
    // it sits next to the service binary in the same target directory.
    let helper =
        std::path::Path::new(env!("CARGO_BIN_EXE_belvedered")).with_file_name("belvedere-model");
    if !helper.is_file() {
        eprintln!(
            "{} not built (run cargo build --workspace); skipping",
            helper.display()
        );
        return;
    }

    let bus = Bus::start().await;
    let mut cmd = bus.service_command(&dir.path().join("belvedere.db"));
    cmd.env("XDG_DATA_HOME", dir.path())
        .env("BELVEDERE_FORCE_CPU", "1")
        .env("BELVEDERE_MODEL_HELPER", &helper);
    let service = cmd.spawn().unwrap();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;

    let models = proxy.list_models().await.unwrap();
    let ours = models
        .iter()
        .find(|m| m.path.ends_with("test-model.gguf"))
        .expect("scan found the test model");

    assert_eq!(proxy.model_status().await.unwrap().0, "unloaded");
    let (gpu_layers, context, ms) = proxy.load_model(ours.id).await.unwrap();
    assert_eq!(gpu_layers, 0, "CPU was forced");
    assert!(context >= 512);
    assert!(ms > 0);
    let (state, name) = proxy.model_status().await.unwrap();
    assert_eq!(state, "ready");
    assert_eq!(name, ours.name);

    let mut text = proxy.receive_generation_text().await.unwrap();
    let mut done = proxy.receive_generation_done().await.unwrap();
    let request = proxy.generate("The capital of France is", 8).await.unwrap();

    let mut reply = String::new();
    let finished = loop {
        tokio::select! {
            Some(t) = text.next() => {
                let args = t.args().unwrap();
                assert_eq!(args.request, request);
                reply.push_str(args.text);
            }
            d = timeout(Duration::from_secs(120), done.next()) => {
                break d.expect("generation did not finish in time").unwrap();
            }
        }
    };
    let args = finished.args().unwrap();
    assert_eq!(args.request, request);
    assert!(args.tokens > 0 && args.tokens <= 8);
    assert!(!reply.trim().is_empty(), "no text was streamed");

    proxy.unload_model().await.unwrap();
    assert_eq!(proxy.model_status().await.unwrap().0, "unloaded");

    stop(service).await;
}

#[tokio::test]
async fn chat_streams_persists_and_stops_quickly() {
    let Some(model_path) = std::env::var_os("BELVEDERE_TEST_MODEL") else {
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
    let models = dir.path().join("belvedere").join("models");
    std::fs::create_dir_all(&models).unwrap();
    std::os::unix::fs::symlink(
        std::path::PathBuf::from(model_path),
        models.join("test-model.gguf"),
    )
    .unwrap();
    let db_path = dir.path().join("belvedere.db");

    let bus = Bus::start().await;
    let spawn = || {
        let mut cmd = bus.service_command(&db_path);
        cmd.env("XDG_DATA_HOME", dir.path())
            .env("HOME", dir.path())
            .env("BELVEDERE_FORCE_CPU", "1")
            .env("BELVEDERE_MODEL_HELPER", &helper);
        cmd.spawn().unwrap()
    };
    let service = spawn();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;

    let conversation = proxy.new_conversation().await.unwrap();
    let mut text = proxy.receive_chat_text().await.unwrap();
    let mut done = proxy.receive_chat_done().await.unwrap();
    let mut status = proxy.receive_chat_status().await.unwrap();
    let request = proxy
        .send_message(conversation.id, "Reply with the single word: hello")
        .await
        .unwrap();

    // The model had to be loaded first; the window is told so.
    let first_status = timeout(Duration::from_secs(10), status.next())
        .await
        .expect("no ChatStatus")
        .unwrap();
    assert_eq!(first_status.args().unwrap().status, "loading model");

    let mut streamed = String::new();
    let mut pieces = 0;
    let finished = loop {
        tokio::select! {
            Some(t) = text.next() => {
                let a = t.args().unwrap();
                assert_eq!(a.request, request);
                streamed.push_str(a.text);
                pieces += 1;
            }
            d = timeout(Duration::from_secs(120), done.next()) => {
                break d.expect("reply did not finish").unwrap();
            }
        }
    };
    let message_id = finished.args().unwrap().message_id;
    assert!(pieces >= 1, "reply did not stream");
    assert!(!streamed.trim().is_empty());
    assert!(!streamed.contains("<think>"), "thinking leaked: {streamed}");

    // Saved: user question and assistant answer.
    let messages = proxy.get_messages(conversation.id).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[1].id, message_id);
    assert_eq!(messages[1].content, streamed.trim());

    // Stop: ask for a long reply, stop it, and expect Done within a second.
    let request = proxy
        .send_message(
            conversation.id,
            "Count slowly from one to five hundred, one number per line.",
        )
        .await
        .unwrap();
    // Wait for the first piece so generation is really underway.
    loop {
        let t = timeout(Duration::from_secs(60), text.next())
            .await
            .expect("no text for the long reply")
            .unwrap();
        if t.args().unwrap().request == request {
            break;
        }
    }
    let stopped_at = tokio::time::Instant::now();
    proxy.stop_generation(request).await.unwrap();
    let finished = loop {
        let d = timeout(Duration::from_secs(5), done.next())
            .await
            .expect("no ChatDone after Stop")
            .unwrap();
        if d.args().unwrap().request == request {
            break d;
        }
    };
    let latency = stopped_at.elapsed();
    assert!(latency < Duration::from_secs(1), "Stop took {latency:?}");
    assert_eq!(finished.args().unwrap().conversation_id, conversation.id);
    // The partial reply was kept.
    let messages = proxy.get_messages(conversation.id).await.unwrap();
    assert_eq!(messages.len(), 4);

    // Survives a service restart.
    stop(service).await;
    let service = spawn();
    let proxy = wait_ready(&conn).await;
    let listed = proxy.list_conversations().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "Reply with the single word: hello");
    assert_eq!(proxy.get_messages(conversation.id).await.unwrap().len(), 4);
    stop(service).await;
}
