//! The service side of `org.belvedere.Service`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::chat::{self, Replies};
use belvedere_core::engine::{self, Chunk, Engine};

use belvedere_core::db::{Db, DbError, NewTask, Role, SourceKind};
use belvedere_core::ipc::{
    CalendarDto, CalendarTaskDto, ConversationDto, DownloadDto, EventDto, MailAccountDto,
    MailFolderDto, MailMessageDto, MessageDto, ModelDto, RepoDto, RepoFileDto, RuleDto, SettingDto,
    SuggestionDto, TaskDto, BUS_NAME, OBJECT_PATH,
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
    calendar: crate::calendar::SharedCalendar,
    downloads: crate::downloads::SharedManager,
    next_request: AtomicU64,
    replies: Arc<Replies>,
}

impl Service {
    pub fn new(db: SharedDb, engine: Engine, calendar: crate::calendar::SharedCalendar) -> Self {
        Self {
            db,
            engine,
            calendar,
            downloads: Default::default(),
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

/// A task as the bus sees it: with its first source (the email it came
/// from), if any.
fn task_dto(db: &Db, task: belvedere_core::db::Task) -> TaskDto {
    let mut dto = TaskDto::from(task);
    if let Ok(sources) = db.task_sources(dto.id) {
        if let Some(src) = sources.first() {
            dto.source_kind = match src.kind {
                SourceKind::Email => "email",
                SourceKind::Event => "event",
                SourceKind::File => "file",
            }
            .into();
            dto.source_label = src.label.clone();
        }
    }
    dto
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
        let db = self.db();
        let tasks = db.list_tasks().map_err(to_fdo)?;
        Ok(tasks.into_iter().map(|t| task_dto(&db, t)).collect())
    }

    fn get_task(&self, id: i64) -> fdo::Result<TaskDto> {
        let db = self.db();
        db.get_task(id).map(|t| task_dto(&db, t)).map_err(to_fdo)
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
        Ok(task_dto(&self.db(), task))
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
        Ok(task_dto(&self.db(), task))
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
        Ok(task_dto(&self.db(), task))
    }

    async fn restore_task(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
    ) -> fdo::Result<TaskDto> {
        let task = self.db().restore_task(id).map_err(to_fdo)?;
        info!(id, "task restored over D-Bus");
        Self::tasks_changed(&emitter).await?;
        Ok(task_dto(&self.db(), task))
    }

    fn list_deleted_tasks(&self) -> fdo::Result<Vec<TaskDto>> {
        let db = self.db();
        let tasks = db.list_deleted_tasks().map_err(to_fdo)?;
        Ok(tasks.into_iter().map(|t| task_dto(&db, t)).collect())
    }

    async fn complete_task(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
    ) -> fdo::Result<TaskDto> {
        let task = self.db().complete_task(id).map_err(to_fdo)?;
        Self::tasks_changed(&emitter).await?;
        Ok(task_dto(&self.db(), task))
    }

    /// Closes a task as not needed (or already handled), with the reason,
    /// and cancels its reminders. The task stays, marked dismissed.
    async fn dismiss_task(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
        reason: &str,
    ) -> fdo::Result<TaskDto> {
        let reason = reason.trim();
        let reason = if reason.is_empty() {
            "not needed"
        } else {
            reason
        };
        let task = {
            let db = self.db();
            let task = db.dismiss_task(id, reason).map_err(to_fdo)?;
            let _ = db.cancel_task_reminders(id);
            task
        };
        info!(task = id, reason, "task dismissed");
        Self::tasks_changed(&emitter).await?;
        Ok(task_dto(&self.db(), task))
    }

    async fn reopen_task(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
    ) -> fdo::Result<TaskDto> {
        let task = self.db().reopen_task(id).map_err(to_fdo)?;
        Self::tasks_changed(&emitter).await?;
        Ok(task_dto(&self.db(), task))
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

    /// The models chosen for chat and for background work (ids; 0 when
    /// none is available).
    fn model_roles(&self) -> (i64, i64) {
        let db = self.db();
        (
            crate::chat::pick_chat_model(&db).map(|m| m.id).unwrap_or(0),
            crate::chat::pick_background_model(&db)
                .map(|m| m.id)
                .unwrap_or(0),
        )
    }

    /// Chooses the model for a role: `chat` or `background`. Takes effect
    /// on the next use, no restart.
    async fn set_model_role(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        role: &str,
        id: i64,
    ) -> fdo::Result<()> {
        let key = match role {
            "chat" => "chat_model_id",
            "background" => "background_model_id",
            other => return Err(fdo::Error::InvalidArgs(format!("unknown role {other:?}"))),
        };
        {
            let db = self.db();
            db.get_model(id).map_err(to_fdo)?;
            db.set_setting(key, &id.to_string()).map_err(to_fdo)?;
        }
        info!(role, model = id, "model role set");
        Self::models_changed(&emitter).await?;
        Ok(())
    }

    /// Removes a model from Belvedere; deletes its file only if Belvedere
    /// downloaded it. Returns whether a file was deleted.
    async fn forget_model(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
    ) -> fdo::Result<bool> {
        let own = belvedere_core::models::Locations::standard()
            .belvedere
            .first()
            .cloned()
            .unwrap_or_default();
        let deleted = crate::models::forget(&self.db(), id, &own).map_err(fdo::Error::Failed)?;
        Self::models_changed(&emitter).await?;
        Ok(deleted)
    }

    #[zbus(signal)]
    pub async fn models_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    /// Searches Hugging Face for GGUF model repositories (user-started).
    async fn search_models(&self, query: &str) -> fdo::Result<Vec<RepoDto>> {
        let q = query.trim().to_string();
        if q.is_empty() {
            return Err(fdo::Error::InvalidArgs("say what to search for".into()));
        }
        let repos = tokio::task::spawn_blocking(move || crate::downloads::search(&q))
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?
            .map_err(fdo::Error::Failed)?;
        Ok(repos
            .into_iter()
            .map(|r| RepoDto {
                id: r.id,
                downloads: r.downloads,
                likes: r.likes,
            })
            .collect())
    }

    /// The GGUF files in a repository with size, checksum, and whether
    /// each fits this machine's memory.
    async fn list_repo_files(&self, repo: &str) -> fdo::Result<Vec<RepoFileDto>> {
        let r = repo.trim().to_string();
        let files = tokio::task::spawn_blocking(move || crate::downloads::files(&r))
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?
            .map_err(fdo::Error::Failed)?;
        Ok(files
            .into_iter()
            .map(|f| RepoFileDto {
                name: f.name,
                size: f.size,
                sha256: f.sha256,
                fit: f.fit,
                quantization: f.quantization,
            })
            .collect())
    }

    /// Starts downloading a file into Belvedere's models folder.
    async fn start_download(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        repo: &str,
        file: RepoFileDto,
    ) -> fdo::Result<i64> {
        let f = belvedere_core::hf::RepoFile {
            name: file.name,
            size: file.size,
            sha256: file.sha256,
            fit: file.fit,
            quantization: file.quantization,
        };
        let d = crate::downloads::start(&self.db, &self.downloads, conn.clone(), repo.trim(), &f)
            .map_err(fdo::Error::Failed)?;
        Self::downloads_changed(&emitter).await?;
        Ok(d.id)
    }

    async fn pause_download(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
    ) -> fdo::Result<()> {
        crate::downloads::pause(&self.db, &self.downloads, id).map_err(fdo::Error::Failed)?;
        Self::downloads_changed(&emitter).await?;
        Ok(())
    }

    async fn resume_download(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        id: i64,
    ) -> fdo::Result<()> {
        crate::downloads::resume(&self.db, &self.downloads, conn.clone(), id)
            .map_err(fdo::Error::Failed)?;
        Self::downloads_changed(&emitter).await?;
        Ok(())
    }

    /// Stops a download and removes it with its partial file.
    async fn cancel_download(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
    ) -> fdo::Result<()> {
        let db = self.db.clone();
        let manager = self.downloads.clone();
        tokio::task::spawn_blocking(move || crate::downloads::cancel(&db, &manager, id))
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?
            .map_err(fdo::Error::Failed)?;
        Self::downloads_changed(&emitter).await?;
        Ok(())
    }

    fn list_downloads(&self) -> fdo::Result<Vec<DownloadDto>> {
        let list = self.db().list_downloads().map_err(to_fdo)?;
        Ok(list
            .into_iter()
            .map(|d| DownloadDto {
                id: d.id,
                repo: d.repo,
                file: d.file,
                size: d.size,
                received: d.received,
                status: d.status,
                error: d.error,
            })
            .collect())
    }

    #[zbus(signal)]
    pub async fn downloads_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    fn list_settings(&self) -> fdo::Result<Vec<SettingDto>> {
        let db = self.db();
        Ok(belvedere_core::settings::SPECS
            .iter()
            .map(|s| SettingDto {
                key: s.key.into(),
                label: s.label.into(),
                help: s.help.into(),
                kind: format!("{:?}", s.kind).to_lowercase(),
                value: db
                    .get_setting(s.key)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| s.default.to_string()),
                default: s.default.into(),
            })
            .collect())
    }

    async fn set_setting(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        key: &str,
        value: &str,
    ) -> fdo::Result<SettingDto> {
        let spec = belvedere_core::settings::spec(key)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("{key} is not a setting")))?;
        let stored = if value.trim().is_empty() {
            self.db().delete_setting(key).map_err(to_fdo)?;
            spec.default.to_string()
        } else {
            let v =
                belvedere_core::settings::validate(key, value).map_err(fdo::Error::InvalidArgs)?;
            self.db().set_setting(key, &v).map_err(to_fdo)?;
            v
        };
        info!(key, value = stored, "setting changed");
        Self::settings_changed(&emitter).await?;
        Ok(SettingDto {
            key: spec.key.into(),
            label: spec.label.into(),
            help: spec.help.into(),
            kind: format!("{:?}", spec.kind).to_lowercase(),
            value: stored,
            default: spec.default.into(),
        })
    }

    async fn regenerate_caldav_password(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        let p = belvedere_core::caldav::generate_password()
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        self.db()
            .set_setting("caldav_password", &p)
            .map_err(to_fdo)?;
        info!("Thunderbird calendar password regenerated");
        Self::settings_changed(&emitter).await?;
        Ok(p)
    }

    #[zbus(signal)]
    pub async fn settings_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    /// Imports a GGUF file or a folder of them, by reference or as a copy
    /// into Belvedere's models folder.
    async fn import_model(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        path: &str,
        copy: bool,
    ) -> fdo::Result<Vec<ModelDto>> {
        let path = path.trim();
        if path.is_empty() {
            return Err(fdo::Error::InvalidArgs("give a file or folder".into()));
        }
        let own = crate::downloads::own_models_dir();
        let imported = crate::models::import(&self.db(), std::path::Path::new(path), copy, &own)
            .map_err(fdo::Error::Failed)?;
        Self::models_changed(&emitter).await?;
        Ok(imported.into_iter().map(ModelDto::from).collect())
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

    fn list_suggestions(&self) -> fdo::Result<Vec<SuggestionDto>> {
        let db = self.db();
        let list = db.list_suggestions().map_err(to_fdo)?;
        Ok(list
            .into_iter()
            .map(|s| {
                let source_label = db
                    .get_mail(s.mail_message_id)
                    .map(|m| m.subject)
                    .unwrap_or_default();
                SuggestionDto {
                    id: s.id,
                    title: s.title,
                    notes: s.notes,
                    due_at: s.due_at.unwrap_or_default(),
                    kind: s.kind,
                    amount: s.amount.unwrap_or(0.0),
                    confidence: s.confidence,
                    source_label,
                    created_at: s.created_at,
                }
            })
            .collect())
    }

    async fn accept_suggestion(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
    ) -> fdo::Result<TaskDto> {
        let dto = {
            let db = self.db();
            let s = db.get_suggestion(id).map_err(to_fdo)?;
            if s.resolved_at.is_some() {
                return Err(fdo::Error::Failed(format!(
                    "suggestion {id} was already {}",
                    s.resolution.as_deref().unwrap_or("resolved")
                )));
            }
            let mail = db.get_mail(s.mail_message_id).map_err(to_fdo)?;
            let extraction = belvedere_core::extract::Extraction {
                action_needed: true,
                kind: match s.kind.as_str() {
                    "bill" => belvedere_core::extract::Kind::Bill,
                    "deadline" => belvedere_core::extract::Kind::Deadline,
                    "reply_needed" => belvedere_core::extract::Kind::ReplyNeeded,
                    "appointment" => belvedere_core::extract::Kind::Appointment,
                    "renewal" => belvedere_core::extract::Kind::Renewal,
                    _ => belvedere_core::extract::Kind::Other,
                },
                title: s.title.clone(),
                due_date: s.due_at.as_deref().and_then(|d| {
                    chrono::DateTime::parse_from_rfc3339(d)
                        .ok()
                        .map(|d| d.with_timezone(&Local).format("%Y-%m-%d").to_string())
                }),
                due_time: s.due_at.as_deref().and_then(|d| {
                    chrono::DateTime::parse_from_rfc3339(d)
                        .ok()
                        .map(|d| d.with_timezone(&Local).format("%H:%M").to_string())
                }),
                amount: s.amount,
                from_whom: String::new(),
                reference: None,
                confirms_done: false,
                rule_applied: None,
                heads_up: false,
                also: Vec::new(),
                confidence: s.confidence as f32,
            };
            let task = crate::pipeline::create_task_from_mail(&db, &mail, &extraction)
                .map_err(fdo::Error::Failed)?;
            db.accept_suggestion(id, task.id).map_err(to_fdo)?;
            info!(suggestion = id, task = task.id, "suggestion accepted");
            task_dto(&db, task)
        };
        Self::tasks_changed(&emitter).await?;
        Self::suggestions_changed(&emitter).await?;
        Ok(dto)
    }

    async fn reject_suggestion(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
    ) -> fdo::Result<()> {
        self.db().reject_suggestion(id).map_err(to_fdo)?;
        info!(suggestion = id, "suggestion rejected");
        Self::suggestions_changed(&emitter).await?;
        Ok(())
    }

    fn open_email(&self, task_id: i64) -> fdo::Result<()> {
        let message_id = {
            let db = self.db();
            db.task_sources(task_id)
                .map_err(to_fdo)?
                .into_iter()
                .rfind(|s| s.kind == SourceKind::Email)
                .map(|s| s.reference)
                .ok_or_else(|| fdo::Error::Failed("this task did not come from an email".into()))?
        };
        crate::pipeline::open_in_thunderbird(&message_id).map_err(fdo::Error::Failed)
    }

    #[zbus(signal)]
    pub async fn suggestions_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    fn list_rules(&self) -> fdo::Result<Vec<RuleDto>> {
        let list = self.db().list_rules().map_err(to_fdo)?;
        Ok(list.into_iter().map(RuleDto::from).collect())
    }

    async fn create_rule(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        text: &str,
    ) -> fdo::Result<RuleDto> {
        let text = text.trim();
        if text.is_empty() {
            return Err(fdo::Error::InvalidArgs("a rule needs some words".into()));
        }
        let rule = self.db().create_rule(text).map_err(to_fdo)?;
        info!(rule = rule.id, "rule added");
        Self::rules_changed(&emitter).await?;
        Ok(RuleDto::from(rule))
    }

    async fn update_rule(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
        text: &str,
        enabled: bool,
    ) -> fdo::Result<RuleDto> {
        let text = text.trim();
        if text.is_empty() {
            return Err(fdo::Error::InvalidArgs("a rule needs some words".into()));
        }
        let rule = self.db().update_rule(id, text, enabled).map_err(to_fdo)?;
        Self::rules_changed(&emitter).await?;
        Ok(RuleDto::from(rule))
    }

    async fn delete_rule(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: i64,
    ) -> fdo::Result<RuleDto> {
        let rule = self.db().delete_rule(id).map_err(to_fdo)?;
        info!(rule = id, "rule deleted");
        Self::rules_changed(&emitter).await?;
        Ok(RuleDto::from(rule))
    }

    #[zbus(signal)]
    pub async fn rules_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    fn list_calendars(&self) -> fdo::Result<Vec<CalendarDto>> {
        let st = self.calendar.lock().unwrap_or_else(|e| e.into_inner());
        Ok(st
            .reading
            .calendars
            .iter()
            .map(|c| CalendarDto {
                id: c.id.clone(),
                name: c.name.clone(),
                kind: c.kind.clone(),
                enabled: c.enabled,
                readable: c.kind == "storage" || st.reading.on_disk.contains(&c.id),
            })
            .collect())
    }

    fn list_events(&self, from: &str, to: &str) -> fdo::Result<Vec<EventDto>> {
        let now = chrono::Utc::now();
        let from = if from.is_empty() {
            now
        } else {
            belvedere_core::calendar::parse_time(from)
                .ok_or_else(|| fdo::Error::InvalidArgs("from must be RFC 3339".into()))?
        };
        let to = if to.is_empty() {
            now + chrono::Duration::days(belvedere_core::calendar::WINDOW_DAYS)
        } else {
            belvedere_core::calendar::parse_time(to)
                .ok_or_else(|| fdo::Error::InvalidArgs("to must be RFC 3339".into()))?
        };
        let st = self.calendar.lock().unwrap_or_else(|e| e.into_inner());
        Ok(st
            .reading
            .events
            .iter()
            .filter(|e| e.start < to && e.end > from)
            .cloned()
            .map(EventDto::from)
            .collect())
    }

    /// Thunderbird's own tasks: those in its other calendars, not its
    /// subscription to Belvedere's (those are Belvedere's tasks already).
    fn list_calendar_tasks(&self) -> fdo::Result<Vec<CalendarTaskDto>> {
        let st = self.calendar.lock().unwrap_or_else(|e| e.into_inner());
        let ours: Vec<&str> = st
            .reading
            .calendars
            .iter()
            .filter(|c| c.is_belvedere())
            .map(|c| c.id.as_str())
            .collect();
        Ok(st
            .reading
            .tasks
            .iter()
            .filter(|t| !ours.contains(&t.calendar_id.as_str()))
            .cloned()
            .map(CalendarTaskDto::from)
            .collect())
    }

    /// Opens Thunderbird on its calendar and tasks view.
    fn open_thunderbird_calendar(&self) -> fdo::Result<()> {
        crate::pipeline::open_thunderbird_with(&["-calendar"]).map_err(fdo::Error::Failed)
    }

    fn calendar_read_at(&self) -> fdo::Result<String> {
        let st = self.calendar.lock().unwrap_or_else(|e| e.into_inner());
        Ok(st
            .read_at
            .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or_default())
    }

    #[zbus(signal)]
    pub async fn calendar_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    fn briefing(&self) -> fdo::Result<(String, String)> {
        Ok(crate::scheduler::compose_briefing(
            &self.db(),
            &self.calendar,
            chrono::Local::now(),
        ))
    }

    #[zbus(signal)]
    pub async fn show_conversation(emitter: &SignalEmitter<'_>, id: i64) -> zbus::Result<()>;

    /// Address, user name, and password for subscribing Thunderbird to
    /// the Belvedere calendar.
    fn caldav_info(&self) -> fdo::Result<(String, String, String)> {
        Ok(crate::caldav::connection_info(&self.db()))
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
            self.calendar.clone(),
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
pub async fn serve(
    db: SharedDb,
    engine: Engine,
    calendar: crate::calendar::SharedCalendar,
) -> zbus::Result<zbus::Connection> {
    let conn = zbus::connection::Builder::session()?
        .allow_name_replacements(false)
        .replace_existing_names(false)
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, Service::new(db, engine, calendar))?
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
