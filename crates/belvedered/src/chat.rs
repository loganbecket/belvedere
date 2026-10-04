//! Chat: turns a conversation into a reply from the model, streams it to
//! the bus, and saves it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use belvedere_core::db::{Db, Model, Role};
use belvedere_core::ipc::OBJECT_PATH;
use belvedere_core::model_ipc::ChatMessage;
use belvedere_core::think::ThinkFilter;
use chrono::Local;
use tracing::{info, warn};

use crate::dbus::{Service, SharedDb};
use crate::engine::{self, Chunk, Engine, State};

/// How much of a reply to allow.
const MAX_REPLY_TOKENS: u32 = 1024;

/// The persona and the facts every conversation starts with.
pub fn system_prompt(now: chrono::DateTime<Local>) -> String {
    format!(
        "You are Belvedere, a discreet and capable personal butler who runs on the user's own computer. \
You help with tasks, reminders, mail, and the calendar. Be brief, plain, and useful; answer directly \
without preamble. Use American English. Today is {}.",
        now.format("%A, %B %-d, %Y")
    )
}

/// Which model to chat with: the configured one, else the best guess
/// among what's on disk (the 9B Qwen if present, then anything that can
/// use tools, then anything at all).
pub fn pick_chat_model(db: &Db) -> Option<Model> {
    let models = db.list_models().ok()?;
    if let Some(id) = db
        .get_setting("chat_model_id")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
    {
        if let Some(m) = models.iter().find(|m| m.id == id) {
            return Some(m.clone());
        }
    }
    let lower = |m: &Model| m.name.to_ascii_lowercase();
    models
        .iter()
        .find(|m| lower(m).contains("qwen3.5") && lower(m).contains("9b"))
        .or_else(|| models.iter().find(|m| m.supports_tools == Some(true)))
        .or_else(|| models.first())
        .cloned()
}

/// A reply in progress, so Stop can find it.
#[derive(Default)]
pub struct Replies {
    active: Mutex<HashMap<u64, i64>>,
}

impl Replies {
    pub fn start(&self, request: u64, conversation_id: i64) {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(request, conversation_id);
    }

    pub fn finish(&self, request: u64) {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request);
    }

    pub fn is_active(&self, request: u64) -> bool {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&request)
    }
}

fn lock(db: &SharedDb) -> std::sync::MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs one reply end to end. Spawned by `SendMessage`; everything it
/// learns goes out as signals on `bus`.
pub async fn reply(
    db: SharedDb,
    engine: Engine,
    replies: Arc<Replies>,
    bus: zbus::Connection,
    request: u64,
    conversation_id: i64,
) {
    let emitter = match bus
        .object_server()
        .interface::<_, Service>(OBJECT_PATH)
        .await
    {
        Ok(iface) => iface.signal_emitter().to_owned(),
        Err(err) => {
            warn!("chat reply has no bus interface to signal on: {err}");
            replies.finish(request);
            return;
        }
    };
    let fail = |message: String| {
        let emitter = emitter.clone();
        async move {
            let _ = Service::chat_failed(&emitter, request, conversation_id, &message).await;
        }
    };

    // Make sure a model is loaded.
    if !matches!(
        engine.state(),
        State::Ready { .. } | State::Generating { .. }
    ) {
        // Hold the database lock only for the lookup, never across an await.
        let picked = pick_chat_model(&lock(&db));
        let Some(model) = picked else {
            fail("No model is available. Belvedere found no model files on this machine.".into())
                .await;
            replies.finish(request);
            return;
        };
        let _ = Service::chat_status(&emitter, request, conversation_id, "loading model").await;
        if let Err(err) = engine
            .load(
                model.path.clone().into(),
                model.name.clone(),
                engine::gpu_enabled(),
            )
            .await
        {
            fail(format!("Could not load {}: {err}", model.name)).await;
            replies.finish(request);
            return;
        }
    }

    // Build the conversation for the model.
    let history = lock(&db).conversation_messages(conversation_id);
    let history = match history {
        Ok(h) => h,
        Err(err) => {
            fail(format!("Could not read the conversation: {err}")).await;
            replies.finish(request);
            return;
        }
    };
    let mut messages = vec![ChatMessage {
        role: "system".into(),
        content: system_prompt(Local::now()),
    }];
    messages.extend(history.iter().filter(|m| !m.content.is_empty()).map(|m| {
        ChatMessage {
            role: match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
                Role::Tool => "user",
            }
            .into(),
            content: m.content.clone(),
        }
    }));

    let _ = Service::chat_status(&emitter, request, conversation_id, "thinking").await;
    let mut chunks = engine.chat(messages, MAX_REPLY_TOKENS).await;
    let mut filter = ThinkFilter::new();
    let mut answer = String::new();
    let mut was_thinking = false;
    let mut failed: Option<String> = None;
    while let Some(chunk) = chunks.recv().await {
        if !replies.is_active(request) {
            // Stopped: drop the receiver, which cancels the helper.
            break;
        }
        match chunk {
            Chunk::Text(piece) => {
                let visible = filter.push(&piece);
                if filter.thinking() != was_thinking {
                    was_thinking = filter.thinking();
                    let status = if was_thinking { "thinking" } else { "writing" };
                    let _ = Service::chat_status(&emitter, request, conversation_id, status).await;
                }
                if !visible.is_empty() {
                    answer.push_str(&visible);
                    let _ = Service::chat_text(&emitter, request, conversation_id, &visible).await;
                }
            }
            Chunk::Done { tokens, seconds } => {
                info!(request, tokens, seconds, "chat reply finished");
                break;
            }
            Chunk::Failed(message) => {
                failed = Some(message);
                break;
            }
        }
    }
    drop(chunks);
    let tail = filter.finish();
    if !tail.is_empty() {
        answer.push_str(&tail);
        let _ = Service::chat_text(&emitter, request, conversation_id, &tail).await;
    }
    replies.finish(request);

    if let Some(message) = failed {
        if answer.is_empty() {
            fail(message).await;
            return;
        }
        warn!(request, "reply ended early: {message}");
    }

    // Save what was said, even if it was cut short.
    let saved = lock(&db).add_message(conversation_id, Role::Assistant, answer.trim());
    match saved {
        Ok(m) => {
            let _ = Service::chat_done(&emitter, request, conversation_id, m.id).await;
        }
        Err(err) => fail(format!("Could not save the reply: {err}")).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belvedere_core::db::{ModelSource, NewModel};
    use chrono::TimeZone;

    #[test]
    fn system_prompt_names_belvedere_and_the_date() {
        let day = Local.with_ymd_and_hms(2026, 10, 20, 9, 0, 0).unwrap();
        let p = system_prompt(day);
        assert!(p.starts_with("You are Belvedere"));
        assert!(p.contains("Tuesday, October 20, 2026"));
    }

    fn model<'a>(name: &'a str, path: &'a str, tools: Option<bool>) -> NewModel<'a> {
        NewModel {
            name,
            path,
            source: ModelSource::LmStudio,
            size_bytes: 1,
            quantization: "Q4_K_M",
            supports_tools: tools,
        }
    }

    #[test]
    fn picks_configured_then_qwen_then_tools_then_anything() {
        let db = Db::open_in_memory().unwrap();
        assert!(pick_chat_model(&db).is_none());

        let plain = db
            .upsert_model(&model("Tiny", "/m/tiny.gguf", Some(false)))
            .unwrap();
        assert_eq!(pick_chat_model(&db).unwrap().id, plain.id);

        let tools = db
            .upsert_model(&model("Toolish 4B", "/m/tools.gguf", Some(true)))
            .unwrap();
        assert_eq!(pick_chat_model(&db).unwrap().id, tools.id);

        let qwen = db
            .upsert_model(&model("Qwen_Qwen3.5 9B", "/m/qwen.gguf", Some(true)))
            .unwrap();
        assert_eq!(pick_chat_model(&db).unwrap().id, qwen.id);

        db.set_setting("chat_model_id", &plain.id.to_string())
            .unwrap();
        assert_eq!(pick_chat_model(&db).unwrap().id, plain.id);

        // A setting pointing at a model that no longer exists falls back.
        db.set_setting("chat_model_id", "999").unwrap();
        assert_eq!(pick_chat_model(&db).unwrap().id, qwen.id);
    }
}
