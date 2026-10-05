//! Chat: turns a conversation into a reply from the model, lets the model
//! use the task tools, streams the answer to the bus, and saves it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use belvedere_core::agent::{self, Progress};
use belvedere_core::db::{Db, Model, NewTask, Role, TaskStatus};
use belvedere_core::engine::{self, Engine, State};
use belvedere_core::ipc::OBJECT_PATH;
use belvedere_core::model_ipc::ChatMessage;
use belvedere_core::tools::{TaskStore, ToolEvent, ToolMail, ToolRule, ToolTask};
use chrono::Local;
use tracing::{info, warn};
use zbus::object_server::SignalEmitter;

use crate::dbus::{Service, SharedDb};

/// How much of a reply to allow per model round.
const MAX_REPLY_TOKENS: u32 = 1024;

/// Which model to chat with: the configured one, else the best guess
/// among what's on disk (the 4B Qwen if present, then anything that can
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
        .find(|m| lower(m).contains("qwen3.5") && lower(m).contains("4b"))
        .or_else(|| models.iter().find(|m| m.supports_tools == Some(true)))
        .or_else(|| models.first())
        .cloned()
}

/// The model for background work (mail reading): the `background_model_id`
/// setting, else the chat model.
pub fn pick_background_model(db: &Db) -> Option<Model> {
    if let Some(id) = db
        .get_setting("background_model_id")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
    {
        if let Ok(m) = db.get_model(id) {
            return Some(m);
        }
    }
    pick_chat_model(db)
}

/// Whether `picked` must be loaded: nothing is loaded, or something else
/// is. A switch in the Models page takes effect on the next use this way.
pub fn needs_load(state: &State, picked: &Model) -> bool {
    match state {
        State::Ready { .. } | State::Generating { .. } => state.model_name() != picked.name,
        _ => true,
    }
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

/// The database as the tools see it. Each call takes the lock briefly.
struct DbStore(SharedDb, crate::calendar::SharedCalendar);

fn mail_to_tool(m: belvedere_core::db::MailMessage) -> ToolMail {
    let from = if m.from_name.is_empty() {
        m.from_addr.clone()
    } else {
        format!("{} <{}>", m.from_name, m.from_addr)
    };
    let snippet: String = m.body_text.split_whitespace().collect::<Vec<_>>().join(" ");
    ToolMail {
        message_id: m.message_id,
        from,
        subject: m.subject,
        date: m.date,
        snippet: snippet.chars().take(240).collect(),
    }
}

fn to_tool_task(t: belvedere_core::db::Task) -> ToolTask {
    ToolTask {
        id: t.id,
        title: t.title,
        notes: t.notes,
        due_at: t.due_at.unwrap_or_default(),
        status: match t.status {
            TaskStatus::Open => "open",
            TaskStatus::Done => "done",
            TaskStatus::Dismissed => "dismissed",
        }
        .into(),
        dismiss_reason: t.dismiss_reason.unwrap_or_default(),
    }
}

impl TaskStore for DbStore {
    fn read_file(&mut self, path: &str) -> Result<belvedere_core::tools::ToolDocument, String> {
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .ok_or("no home folder")?;
        let extra: Vec<String> = lock(&self.0)
            .get_setting("file_search_excludes")
            .ok()
            .flatten()
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let doc = belvedere_core::readfile::read(&home, path, &extra)?;
        tracing::info!(
            path = doc.path,
            kind = doc.kind,
            chars = doc.text.chars().count(),
            "file read for chat"
        );
        Ok(belvedere_core::tools::ToolDocument {
            path: doc.path,
            kind: doc.kind,
            text: doc.text,
            pages: doc.pages,
        })
    }

    fn set_file_source(&mut self, task_id: i64, path: &str) -> Result<(), String> {
        let db = lock(&self.0);
        let label = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        db.add_task_source(task_id, belvedere_core::db::SourceKind::File, path, &label)
            .map_err(|e| e.to_string())?;
        let _ = db.set_task_kind(task_id, "file", "");
        Ok(())
    }

    fn find_files(
        &mut self,
        query: &belvedere_core::files::Query,
        limit: usize,
    ) -> Result<Vec<belvedere_core::tools::ToolFile>, String> {
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .ok_or("no home folder")?;
        let extra: Vec<String> = lock(&self.0)
            .get_setting("file_search_excludes")
            .ok()
            .flatten()
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let (found, stats) = belvedere_core::files::search(&home, query, &extra, limit);
        tracing::info!(
            files = stats.files_seen,
            folders = stats.folders_entered,
            skipped = stats.folders_skipped,
            millis = stats.elapsed_ms as u64,
            found = found.len(),
            "file search"
        );
        Ok(found
            .into_iter()
            .map(|f| belvedere_core::tools::ToolFile {
                path: f.path,
                size: f.size,
                modified: f.modified,
            })
            .collect())
    }

    fn events(
        &mut self,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) -> Result<Vec<ToolEvent>, String> {
        let st = self.1.lock().unwrap_or_else(|e| e.into_inner());
        let f = |d: chrono::DateTime<chrono::Utc>| {
            d.with_timezone(&Local)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
        };
        Ok(st
            .reading
            .events
            .iter()
            .filter(|e| {
                let s = e.start.with_timezone(&Local).date_naive();
                let last = if e.all_day {
                    (e.end - chrono::Duration::seconds(1))
                        .with_timezone(&Local)
                        .date_naive()
                } else {
                    e.end.with_timezone(&Local).date_naive()
                };
                s <= to && last >= from
            })
            .map(|e| ToolEvent {
                title: e.title.clone(),
                start: f(e.start),
                end: f(e.end),
                all_day: e.all_day,
                location: e.location.clone(),
            })
            .collect())
    }

    fn search_mail(&mut self, query: &str, limit: usize) -> Result<Vec<ToolMail>, String> {
        let mut hits: Vec<ToolMail> = lock(&self.0)
            .search_mail(query, limit)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(mail_to_tool)
            .collect();
        // Thunderbird's own index reaches further back than Belvedere's
        // own copy of recent mail.
        if let Some(index) =
            crate::mail::profile().and_then(|p| belvedere_core::mail::gloda::index_file(&p.dir))
        {
            match belvedere_core::mail::gloda::search(&index, query, limit) {
                Ok(more) => {
                    for h in more {
                        if !hits.iter().any(|m| m.message_id == h.message_id) {
                            hits.push(ToolMail {
                                message_id: h.message_id,
                                from: h.author,
                                subject: h.subject,
                                date: h.date,
                                snippet: h.snippet,
                            });
                        }
                    }
                }
                Err(err) => warn!("Thunderbird's search index could not be read: {err}"),
            }
        }
        hits.sort_by(|a, b| b.date.cmp(&a.date));
        hits.truncate(limit);
        Ok(hits)
    }

    fn task_source(&mut self, task_id: i64) -> Result<Option<ToolMail>, String> {
        let db = lock(&self.0);
        let sources = db.task_sources(task_id).map_err(|e| e.to_string())?;
        let Some(source) = sources
            .iter()
            .rfind(|s| s.kind == belvedere_core::db::SourceKind::Email)
        else {
            return Ok(None);
        };
        Ok(db
            .mail_by_message_id(&source.reference)
            .map_err(|e| e.to_string())?
            .map(mail_to_tool))
    }

    fn list_rules(&mut self) -> Result<Vec<ToolRule>, String> {
        lock(&self.0)
            .list_rules()
            .map(|v| {
                v.into_iter()
                    .filter(|r| r.enabled)
                    .map(|r| ToolRule {
                        id: r.id,
                        text: r.text,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    }

    fn add_rule(&mut self, text: &str) -> Result<ToolRule, String> {
        lock(&self.0)
            .create_rule(text)
            .map(|r| ToolRule {
                id: r.id,
                text: r.text,
            })
            .map_err(|e| e.to_string())
    }

    fn delete_rule(&mut self, id: i64) -> Result<ToolRule, String> {
        lock(&self.0)
            .delete_rule(id)
            .map(|r| ToolRule {
                id: r.id,
                text: r.text,
            })
            .map_err(|e| e.to_string())
    }

    fn list(&mut self) -> Result<Vec<ToolTask>, String> {
        lock(&self.0)
            .list_tasks()
            .map(|v| v.into_iter().map(to_tool_task).collect())
            .map_err(|e| e.to_string())
    }

    fn create(
        &mut self,
        title: &str,
        notes: &str,
        due_at: Option<&str>,
    ) -> Result<ToolTask, String> {
        let db = lock(&self.0);
        let task = db
            .create_task(&NewTask {
                title: title.to_string(),
                notes: notes.to_string(),
                due_at: due_at.map(str::to_string),
            })
            .map_err(|e| e.to_string())?;
        if let Some(due) = &task.due_at {
            let plan = belvedere_core::schedule::plan_reminders(due, Local::now());
            let _ = db.replace_task_reminders(task.id, &plan);
        }
        Ok(to_tool_task(task))
    }

    fn update(
        &mut self,
        id: i64,
        title: Option<&str>,
        notes: Option<&str>,
        due_at: Option<Option<&str>>,
    ) -> Result<ToolTask, String> {
        let db = lock(&self.0);
        let current = db.get_task(id).map_err(|e| e.to_string())?;
        let new = NewTask {
            title: title.map(str::to_string).unwrap_or(current.title),
            notes: notes.map(str::to_string).unwrap_or(current.notes),
            due_at: match due_at {
                Some(d) => d.map(str::to_string),
                None => current.due_at.clone(),
            },
        };
        let task = db.update_task(id, &new).map_err(|e| e.to_string())?;
        if due_at.is_some() {
            let plan = task
                .due_at
                .as_deref()
                .map(|d| belvedere_core::schedule::plan_reminders(d, Local::now()))
                .unwrap_or_default();
            let _ = db.replace_task_reminders(task.id, &plan);
        }
        Ok(to_tool_task(task))
    }

    fn reopen(&mut self, id: i64) -> Result<ToolTask, String> {
        lock(&self.0)
            .reopen_task(id)
            .map(to_tool_task)
            .map_err(|e| e.to_string())
    }

    fn delete(&mut self, id: i64) -> Result<ToolTask, String> {
        let db = lock(&self.0);
        let task = db.delete_task(id).map_err(|e| e.to_string())?;
        let _ = db.cancel_task_reminders(id);
        Ok(to_tool_task(task))
    }
}

/// Forwards the agent's progress to the bus.
struct BusProgress {
    emitter: SignalEmitter<'static>,
    request: u64,
    conversation_id: i64,
    answer: String,
    handle: tokio::runtime::Handle,
}

impl Progress for BusProgress {
    fn text(&mut self, piece: &str) {
        self.answer.push_str(piece);
        let emitter = self.emitter.clone();
        let (request, conversation_id, piece) =
            (self.request, self.conversation_id, piece.to_string());
        self.handle.spawn(async move {
            let _ = Service::chat_text(&emitter, request, conversation_id, &piece).await;
        });
    }

    fn status(&mut self, status: &str) {
        let emitter = self.emitter.clone();
        let (request, conversation_id, status) =
            (self.request, self.conversation_id, status.to_string());
        self.handle.spawn(async move {
            let _ = Service::chat_status(&emitter, request, conversation_id, &status).await;
        });
    }
}

/// Runs one reply end to end. Spawned by `SendMessage`; everything it
/// learns goes out as signals on `bus`.
pub async fn reply(
    db: SharedDb,
    calendar: crate::calendar::SharedCalendar,
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

    // Make sure the chosen model is the one loaded.
    // Hold the database lock only for the lookup, never across an await.
    let picked = pick_chat_model(&lock(&db));
    let Some(model) = picked else {
        fail("No model is available. Belvedere found no model files on this machine.".into()).await;
        replies.finish(request);
        return;
    };
    if needs_load(&engine.state(), &model) {
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

    // The conversation for the model: user and assistant turns only.
    let history = lock(&db).conversation_messages(conversation_id);
    let history = match history {
        Ok(h) => h,
        Err(err) => {
            fail(format!("Could not read the conversation: {err}")).await;
            replies.finish(request);
            return;
        }
    };
    let history: Vec<ChatMessage> = history
        .iter()
        .filter(|m| !m.content.is_empty() && matches!(m.role, Role::User | Role::Assistant))
        .map(|m| ChatMessage {
            role: if m.role == Role::User {
                "user"
            } else {
                "assistant"
            }
            .into(),
            content: m.content.clone(),
        })
        .collect();

    let mut store = DbStore(db.clone(), calendar.clone());
    let mut progress = BusProgress {
        emitter: emitter.clone(),
        request,
        conversation_id,
        answer: String::new(),
        handle: tokio::runtime::Handle::current(),
    };
    let turn = tokio::select! {
        turn = agent::run_turn(&engine, &mut store, &history, Local::now(), MAX_REPLY_TOKENS, &mut progress) => Some(turn),
        _ = wait_for_stop(&replies, request) => None,
    };
    replies.finish(request);

    let (answer, changed, error) = match turn {
        Some(t) => (t.reply, t.changed_tasks, t.error),
        None => {
            // Stopped: keep what streamed so far.
            engine.cancel().await;
            (progress.answer.trim().to_string(), false, None)
        }
    };
    if changed {
        let _ = Service::tasks_changed(&emitter).await;
    }
    if let Some(message) = error {
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
            info!(request, "chat reply saved");
            let _ = Service::chat_done(&emitter, request, conversation_id, m.id).await;
        }
        Err(err) => fail(format!("Could not save the reply: {err}")).await,
    }
}

/// Resolves when Stop has been pressed for `request`.
async fn wait_for_stop(replies: &Replies, request: u64) {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if !replies.is_active(request) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belvedere_core::db::{ModelSource, NewModel};

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
            .upsert_model(&model("Qwen3.5-4B", "/m/qwen.gguf", Some(true)))
            .unwrap();
        assert_eq!(pick_chat_model(&db).unwrap().id, qwen.id);

        db.set_setting("chat_model_id", &plain.id.to_string())
            .unwrap();
        assert_eq!(pick_chat_model(&db).unwrap().id, plain.id);

        db.set_setting("chat_model_id", "999").unwrap();
        assert_eq!(pick_chat_model(&db).unwrap().id, qwen.id);
    }

    #[test]
    fn a_switched_model_is_loaded_on_next_use() {
        let db = Db::open_in_memory().unwrap();
        let a = db
            .upsert_model(&NewModel {
                name: "Qwen3.5-4B",
                path: "/m/a.gguf",
                source: belvedere_core::db::ModelSource::LmStudio,
                size_bytes: 1,
                quantization: "Q4",
                supports_tools: Some(true),
            })
            .unwrap();
        let b = db
            .upsert_model(&NewModel {
                name: "Other-3B",
                path: "/m/b.gguf",
                source: belvedere_core::db::ModelSource::LmStudio,
                size_bytes: 1,
                quantization: "Q4",
                supports_tools: Some(false),
            })
            .unwrap();
        let ready = State::Ready {
            name: a.name.clone(),
        };
        assert!(!needs_load(&ready, &a), "same model: keep it");
        assert!(
            needs_load(&ready, &b),
            "a different model is picked: load it"
        );
        assert!(needs_load(&State::Unloaded, &a));
        // The background role falls back to the chat model until set.
        db.set_setting("chat_model_id", &a.id.to_string()).unwrap();
        assert_eq!(pick_background_model(&db).unwrap().id, a.id);
        db.set_setting("background_model_id", &b.id.to_string())
            .unwrap();
        assert_eq!(pick_background_model(&db).unwrap().id, b.id);
        assert_eq!(pick_chat_model(&db).unwrap().id, a.id);
    }

    #[test]
    fn db_store_round_trips_tasks_and_reminders() {
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let mut store = DbStore(db.clone(), Default::default());
        let due =
            belvedere_core::tools::due_from_parts(Some("2099-10-23"), Some("14:00"), Local::now())
                .unwrap()
                .unwrap();
        let t = store.create("Call the dentist", "", Some(&due)).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(
            lock(&db).task_reminders(t.id).unwrap().len(),
            2,
            "9:00 and 14:00 reminders planned"
        );

        let t = store
            .update(t.id, Some("Call Dr. Patel"), None, Some(None))
            .unwrap();
        assert_eq!(t.title, "Call Dr. Patel");
        assert_eq!(t.due_at, "");
        assert!(
            lock(&db).task_reminders(t.id).unwrap().is_empty(),
            "cleared date clears reminders"
        );

        // Closing is done through the buttons, not the chat store.
        lock(&db).dismiss_task(t.id, "not needed").unwrap();
        assert_eq!(store.list().unwrap()[0].status, "dismissed");
        assert_eq!(store.list().unwrap()[0].dismiss_reason, "not needed");
        let t = store.reopen(t.id).unwrap();
        assert_eq!(t.status, "open");
        let t = store.delete(t.id).unwrap();
        assert!(store.list().unwrap().is_empty());
        assert_eq!(lock(&db).list_deleted_tasks().unwrap()[0].id, t.id);
    }
}
