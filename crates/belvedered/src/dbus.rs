//! The service side of `org.belvedere.Service`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::chat::{self, Replies};
use belvedere_core::engine::{self, Chunk, Engine};

use belvedere_core::db::{Db, DbError, NewTask, Role};
use belvedere_core::ipc::{
    ConversationDto, MailAccountDto, MailFolderDto, MailMessageDto, MessageDto, ModelDto, TaskDto,
    BUS_NAME, OBJECT_PATH,
};
use belvedere_core::schedule;
use chrono::Local;
use tracing::info;
use zbus::object_server::SignalEmitter;
use zbus::{fdo, interface};

/// Shared handle to the database. SQLite connections are not `Sync`, so
/// every call takes the lock for the duration of one query.
pub type SharedDb = Arc<Mutex<Db>>;

pub struct Service {
    db: SharedDb,
    engine: Engine,
    next_request: AtomicU64,
    replies: Arc<Replies>,
}

impl Service {
    pub fn new(db: SharedDb, engine: Engine) -> Self {
        Self {
            db,
            engine,
            next_request: AtomicU64::new(1),
            replies: Arc::new(Replies::default()),
        }
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Db> {
        // A poisoned lock means another call panicked mid-query. The
        // connection itself is still fine; carry on.
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Service {
    /// Sets a task's default reminders from its due date (none if undated).
    fn plan_reminders(&self, task: &belvedere_core::db::Task) -> Result<(), DbError> {
        let plan = match &task.due_at {
            Some(due) => schedule::plan_reminders(due, Local::now()),
            None => Vec::new(),
        };
        self.db().replace_task_reminders(task.id, &plan)?;
        Ok(())
    }
}

fn to_fdo(err: DbError) -> fdo::Error {
    match err {
        DbError::NotFound(id) => fdo::Error::UnknownObject(format!("no task with id {id}")),
        other => fdo::Error::Failed(other.to_string()),
    }
}

fn opt(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

#[interface(name = "org.belvedere.Service")]
impl Service {
    fn ping(&self) -> String {
        "pong".to_string()
    }

    fn version(&self) -> String {
        belvedere_core::VERSION.to_string()
    }

    fn list_tasks(&self) -> fdo::Result<Vec<TaskDto>> {
        let tasks = self.db().list_tasks().map_err(to_fdo)?;
        Ok(tasks.into_iter().map(TaskDto::from).collect())
    }

    fn get_task(&self, id: i64) -> fdo::Result<TaskDto> {
        self.db().get_task(id).map(TaskDto::from).map_err(to_fdo)
    }

    async fn create_task(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        title: &str,
        notes: &str,
        due_at: &str,
    ) -> fdo::Result<TaskDto> {
        let new = NewTask {
            title: title.to_string(),
            notes: notes.to_string(),
            due_at: opt(due_at),
        };
        let task = self.db().create_task(&new).map_err(to_fdo)?;
        self.plan_reminders(&task).map_err(to_fdo)?;
        info!(id = task.id, "task created over D-Bus");
        Self::tasks_changed(&emitter).await?;
        Ok(task.into())
    }

    async fn update_task(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
        title: &str,
        notes: &str,
        due_at: &str,
    ) -> fdo::Result<TaskDto> {
        let new = NewTask {
            title: title.to_string(),
            notes: notes.to_string(),
            due_at: opt(due_at),
        };
        let before = self.db().get_task(id).map_err(to_fdo)?;
        let task = self.db().update_task(id, &new).map_err(to_fdo)?;
        if before.due_at != task.due_at {
            self.plan_reminders(&task).map_err(to_fdo)?;
        }
        Self::tasks_changed(&emitter).await?;
        Ok(task.into())
    }

    /// Soft delete: the task is hidden, kept, and restorable.
    async fn delete_task(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
    ) -> fdo::Result<TaskDto> {
        let task = self.db().delete_task(id).map_err(to_fdo)?;
        info!(id, "task soft-deleted over D-Bus");
        Self::tasks_changed(&emitter).await?;
        Ok(task.into())
    }

    async fn restore_task(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
    ) -> fdo::Result<TaskDto> {
        let task = self.db().restore_task(id).map_err(to_fdo)?;
        info!(id, "task restored over D-Bus");
        Self::tasks_changed(&emitter).await?;
        Ok(task.into())
    }

    fn list_deleted_tasks(&self) -> fdo::Result<Vec<TaskDto>> {
        let tasks = self.db().list_deleted_tasks().map_err(to_fdo)?;
        Ok(tasks.into_iter().map(TaskDto::from).collect())
    }

    async fn complete_task(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
    ) -> fdo::Result<TaskDto> {
        let task = self.db().complete_task(id).map_err(to_fdo)?;
        Self::tasks_changed(&emitter).await?;
        Ok(task.into())
    }

    async fn reopen_task(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
    ) -> fdo::Result<TaskDto> {
        let task = self.db().reopen_task(id).map_err(to_fdo)?;
        Self::tasks_changed(&emitter).await?;
        Ok(task.into())
    }

    fn list_models(&self) -> fdo::Result<Vec<ModelDto>> {
        let models = self.db().list_models().map_err(to_fdo)?;
        Ok(models.into_iter().map(ModelDto::from).collect())
    }

    async fn load_model(&self, id: i64) -> fdo::Result<(u32, u32, u64)> {
        let model = self.db().get_model(id).map_err(to_fdo)?;
        let loaded = self
            .engine
            .load(model.path.into(), model.name, engine::gpu_enabled())
            .await
            .map_err(fdo::Error::Failed)?;
        Ok((
            loaded.gpu_layers,
            loaded.context,
            loaded.load_time.as_millis() as u64,
        ))
    }

    async fn unload_model(&self) {
        self.engine.unload().await;
    }

    fn model_status(&self) -> (String, String) {
        let state = self.engine.state();
        (state.label().to_string(), state.model_name().to_string())
    }

    async fn generate(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        prompt: &str,
        max_tokens: u32,
    ) -> fdo::Result<u64> {
        let request = self.next_request.fetch_add(1, Ordering::Relaxed);
        let mut chunks = self
            .engine
            .generate(prompt.to_string(), max_tokens.clamp(1, 4096))
            .await;
        let emitter = emitter.to_owned();
        tokio::spawn(async move {
            while let Some(chunk) = chunks.recv().await {
                let sent = match chunk {
                    Chunk::Text(text) => Self::generation_text(&emitter, request, &text).await,
                    Chunk::Done { tokens, seconds } => {
                        Self::generation_done(&emitter, request, tokens, seconds as f64).await
                    }
                    Chunk::Failed(message) => {
                        Self::generation_failed(&emitter, request, &message).await
                    }
                };
                if sent.is_err() {
                    break;
                }
            }
        });
        Ok(request)
    }

    fn list_mail_accounts(&self) -> fdo::Result<Vec<MailAccountDto>> {
        let profile = belvedere_core::thunderbird::find_profile()
            .ok_or_else(|| fdo::Error::Failed("no Thunderbird profile found".into()))?;
        let accounts = belvedere_core::thunderbird::accounts(&profile.dir)
            .map_err(|e| fdo::Error::Failed(format!("reading Thunderbird accounts: {e}")))?;
        Ok(accounts
            .into_iter()
            .map(|a| MailAccountDto {
                key: a.key,
                name: a.name,
                kind: a.kind,
                folders: a
                    .folders
                    .into_iter()
                    .map(|f| MailFolderDto {
                        path: f.path,
                        role: f.role.as_str().to_string(),
                        has_local_mail: f.mbox.is_some(),
                    })
                    .collect(),
            })
            .collect())
    }

    fn list_recent_mail(&self, limit: u32) -> fdo::Result<Vec<MailMessageDto>> {
        let mail = self
            .db()
            .recent_mail(limit.clamp(1, 500) as usize)
            .map_err(to_fdo)?;
        Ok(mail
            .into_iter()
            .map(|m| {
                let attachment_count = serde_json::from_str::<Vec<String>>(&m.attachments)
                    .map(|v| v.len() as u32)
                    .unwrap_or(0);
                MailMessageDto {
                    id: m.id,
                    account: m.account,
                    folder: m.folder,
                    from_name: m.from_name,
                    from_addr: m.from_addr,
                    subject: m.subject,
                    date: m.date,
                    snippet: m
                        .body_text
                        .chars()
                        .take(160)
                        .collect::<String>()
                        .replace('\n', " "),
                    attachment_count,
                    seen_at: m.seen_at,
                }
            })
            .collect())
    }

    fn list_conversations(&self) -> fdo::Result<Vec<ConversationDto>> {
        let list = self.db().list_conversations().map_err(to_fdo)?;
        Ok(list.into_iter().map(ConversationDto::from).collect())
    }

    fn new_conversation(&self) -> fdo::Result<ConversationDto> {
        self.db()
            .create_conversation("")
            .map(ConversationDto::from)
            .map_err(to_fdo)
    }

    fn get_messages(&self, conversation_id: i64) -> fdo::Result<Vec<MessageDto>> {
        let list = self
            .db()
            .conversation_messages(conversation_id)
            .map_err(to_fdo)?;
        Ok(list.into_iter().map(MessageDto::from).collect())
    }

    async fn send_message(
        &self,
        #[zbus(connection)] bus: &zbus::Connection,
        conversation_id: i64,
        text: &str,
    ) -> fdo::Result<u64> {
        let text = text.trim();
        if text.is_empty() {
            return Err(fdo::Error::InvalidArgs("message is empty".into()));
        }
        {
            let db = self.db();
            let conversation = db.get_conversation(conversation_id).map_err(to_fdo)?;
            db.add_message(conversation_id, Role::User, text)
                .map_err(to_fdo)?;
            if conversation.title.is_empty() {
                let title: String = text.chars().take(60).collect();
                let _ = db.rename_conversation(conversation_id, title.trim());
            }
        }
        let request = self.next_request.fetch_add(1, Ordering::Relaxed);
        self.replies.start(request, conversation_id);
        tokio::spawn(chat::reply(
            self.db.clone(),
            self.engine.clone(),
            self.replies.clone(),
            bus.clone(),
            request,
            conversation_id,
        ));
        Ok(request)
    }

    async fn stop_generation(&self, request: u64) {
        if self.replies.is_active(request) {
            self.replies.finish(request);
            self.engine.cancel().await;
        }
    }

    #[zbus(signal)]
    pub async fn chat_status(
        emitter: &SignalEmitter<'_>,
        request: u64,
        conversation_id: i64,
        status: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn chat_text(
        emitter: &SignalEmitter<'_>,
        request: u64,
        conversation_id: i64,
        text: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn chat_done(
        emitter: &SignalEmitter<'_>,
        request: u64,
        conversation_id: i64,
        message_id: i64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn chat_failed(
        emitter: &SignalEmitter<'_>,
        request: u64,
        conversation_id: i64,
        message: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn generation_text(
        emitter: &SignalEmitter<'_>,
        request: u64,
        text: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn generation_done(
        emitter: &SignalEmitter<'_>,
        request: u64,
        tokens: u32,
        seconds: f64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn generation_failed(
        emitter: &SignalEmitter<'_>,
        request: u64,
        message: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn tasks_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    /// Asks any open window to show one task.
    #[zbus(signal)]
    pub async fn show_task(emitter: &SignalEmitter<'_>, id: i64) -> zbus::Result<()>;
}

/// Connects to the session bus, exports the service object, and claims
/// the bus name. The returned connection must be kept alive.
///
/// The name is requested so that no other process can take it over: a
/// second instance fails to start instead of knocking this one off the
/// bus.
pub async fn serve(db: SharedDb, engine: Engine) -> zbus::Result<zbus::Connection> {
    let conn = zbus::connection::Builder::session()?
        .allow_name_replacements(false)
        .replace_existing_names(false)
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, Service::new(db, engine))?
        .build()
        .await?;
    info!(name = BUS_NAME, path = OBJECT_PATH, "D-Bus service ready");
    Ok(conn)
}

/// Resolves if the bus ever tells us we no longer own our name. That
/// should be impossible given the flags above, but if it happens the
/// service is unreachable and must restart rather than run on silently.
pub async fn name_lost(conn: &zbus::Connection) -> zbus::Result<()> {
    use futures_util::StreamExt;
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    let mut lost = dbus.receive_name_lost().await?;
    while let Some(signal) = lost.next().await {
        if let Ok(args) = signal.args() {
            if args.name.as_str() == BUS_NAME {
                return Ok(());
            }
        }
    }
    Ok(())
}
