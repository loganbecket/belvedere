//! How the window and the panel applet talk to the background service:
//! a session D-Bus interface, `org.belvedere.Service`.
//!
//! This module holds the shared wire types and the client side. The
//! service side lives in `belvedered`.

use serde::{Deserialize, Serialize};
use zbus::zvariant::Type;

use crate::db::{Conversation, Message, Model, ModelSource, Role, Task, TaskStatus};

/// The bus name the service claims.
pub const BUS_NAME: &str = "org.belvedere.Service";
/// The object path the service is at.
pub const OBJECT_PATH: &str = "/org/belvedere/Service";
/// The interface name.
pub const INTERFACE: &str = "org.belvedere.Service";

/// A task as it crosses the bus. Optional fields travel as empty strings,
/// which keeps the D-Bus signature simple (`(xsssssssss)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct TaskDto {
    pub id: i64,
    pub title: String,
    pub notes: String,
    /// RFC 3339 or empty.
    pub due_at: String,
    /// `open`, `done`, or `dismissed`.
    pub status: String,
    pub dismiss_reason: String,
    pub created_at: String,
    pub updated_at: String,
    /// RFC 3339 or empty.
    pub completed_at: String,
    /// RFC 3339 or empty. Non-empty means soft-deleted.
    pub deleted_at: String,
    /// `email`, `event`, `file`, or empty when the task was typed in.
    pub source_kind: String,
    /// A label for the source, e.g. the email's subject.
    pub source_label: String,
}

impl From<Task> for TaskDto {
    fn from(t: Task) -> Self {
        TaskDto {
            id: t.id,
            title: t.title,
            notes: t.notes,
            due_at: t.due_at.unwrap_or_default(),
            status: match t.status {
                TaskStatus::Open => "open",
                TaskStatus::Done => "done",
                TaskStatus::Dismissed => "dismissed",
            }
            .to_string(),
            dismiss_reason: t.dismiss_reason.unwrap_or_default(),
            created_at: t.created_at,
            updated_at: t.updated_at,
            completed_at: t.completed_at.unwrap_or_default(),
            deleted_at: t.deleted_at.unwrap_or_default(),
            source_kind: String::new(),
            source_label: String::new(),
        }
    }
}

impl TaskDto {
    pub fn due_at(&self) -> Option<&str> {
        Some(self.due_at.as_str()).filter(|s| !s.is_empty())
    }

    pub fn is_deleted(&self) -> bool {
        !self.deleted_at.is_empty()
    }
}

/// A known model file as it crosses the bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ModelDto {
    pub id: i64,
    pub name: String,
    pub path: String,
    /// `lmstudio`, `ollama`, `belvedere`, or `import`.
    pub source: String,
    pub size_bytes: i64,
    pub quantization: String,
    /// `yes`, `no`, or `unknown`.
    pub supports_tools: String,
}

impl From<Model> for ModelDto {
    fn from(m: Model) -> Self {
        ModelDto {
            id: m.id,
            name: m.name,
            path: m.path,
            source: match m.source {
                ModelSource::LmStudio => "lmstudio",
                ModelSource::Ollama => "ollama",
                ModelSource::Belvedere => "belvedere",
                ModelSource::Import => "import",
            }
            .to_string(),
            size_bytes: m.size_bytes,
            quantization: m.quantization,
            supports_tools: match m.supports_tools {
                Some(true) => "yes",
                Some(false) => "no",
                None => "unknown",
            }
            .to_string(),
        }
    }
}

/// A suggested task awaiting a yes or no, as it crosses the bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SuggestionDto {
    pub id: i64,
    pub title: String,
    pub notes: String,
    /// RFC 3339 or empty.
    pub due_at: String,
    pub kind: String,
    /// 0 when there is no amount.
    pub amount: f64,
    pub confidence: f64,
    /// The email's subject, for context.
    pub source_label: String,
    pub created_at: String,
}

/// A chat conversation as it crosses the bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ConversationDto {
    pub id: i64,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
}

impl From<Conversation> for ConversationDto {
    fn from(c: Conversation) -> Self {
        ConversationDto {
            id: c.id,
            title: c.title,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }
    }
}

/// A chat message as it crosses the bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MessageDto {
    pub id: i64,
    pub conversation_id: i64,
    /// `user`, `assistant`, `system`, or `tool`.
    pub role: String,
    pub content: String,
    pub created_at: String,
}

impl From<Message> for MessageDto {
    fn from(m: Message) -> Self {
        MessageDto {
            id: m.id,
            conversation_id: m.conversation_id,
            role: match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
                Role::Tool => "tool",
            }
            .to_string(),
            content: m.content,
            created_at: m.created_at,
        }
    }
}

/// A mail folder as it crosses the bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MailFolderDto {
    /// Path within the account, e.g. `INBOX` or `[Gmail]/Sent Mail`.
    pub path: String,
    /// `inbox`, `sent`, `drafts`, `junk`, `trash`, `archive`, `templates`,
    /// `outbox`, or `other`.
    pub role: String,
    /// Whether any messages are stored locally for it.
    pub has_local_mail: bool,
}

/// A mail account as it crosses the bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MailAccountDto {
    pub key: String,
    pub name: String,
    /// `imap`, `pop3`, `none` (Local Folders), ...
    pub kind: String,
    pub folders: Vec<MailFolderDto>,
}

/// A seen email as it crosses the bus (essentials only, never the raw
/// message).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MailMessageDto {
    pub id: i64,
    pub account: String,
    pub folder: String,
    pub from_name: String,
    pub from_addr: String,
    pub subject: String,
    /// RFC 3339 or empty.
    pub date: String,
    /// The first words of the body.
    pub snippet: String,
    pub attachment_count: u32,
    pub seen_at: String,
}

/// Client proxy. `ServiceProxy::new(&connection).await?` connects to the
/// running service on whatever bus `connection` is on.
#[zbus::proxy(
    interface = "org.belvedere.Service",
    default_service = "org.belvedere.Service",
    default_path = "/org/belvedere/Service"
)]
pub trait Service {
    /// Returns "pong". Proves the service is up.
    fn ping(&self) -> zbus::Result<String>;

    /// The service's version string.
    fn version(&self) -> zbus::Result<String>;

    /// Every task that is not soft-deleted.
    fn list_tasks(&self) -> zbus::Result<Vec<TaskDto>>;

    /// One task by id, deleted or not. Errors if there is no such task.
    fn get_task(&self, id: i64) -> zbus::Result<TaskDto>;

    /// Creates an open task. `due_at` is RFC 3339 or empty for none.
    fn create_task(&self, title: &str, notes: &str, due_at: &str) -> zbus::Result<TaskDto>;

    /// Replaces title, notes, and due date.
    fn update_task(&self, id: i64, title: &str, notes: &str, due_at: &str)
        -> zbus::Result<TaskDto>;

    /// Soft-deletes a task. It can be restored later; nothing is lost.
    fn delete_task(&self, id: i64) -> zbus::Result<TaskDto>;

    /// Brings a soft-deleted task back exactly as it was.
    fn restore_task(&self, id: i64) -> zbus::Result<TaskDto>;

    /// Soft-deleted tasks, most recently deleted first.
    fn list_deleted_tasks(&self) -> zbus::Result<Vec<TaskDto>>;

    /// Marks a task done.
    fn complete_task(&self, id: i64) -> zbus::Result<TaskDto>;

    /// Closes a task as not needed or already handled, with a reason.
    fn dismiss_task(&self, id: i64, reason: &str) -> zbus::Result<TaskDto>;

    /// Marks a done or dismissed task open again.
    fn reopen_task(&self, id: i64) -> zbus::Result<TaskDto>;

    /// Every model file Belvedere knows about, by name.
    fn list_models(&self) -> zbus::Result<Vec<ModelDto>>;

    /// Loads a model by its id from `ListModels`, replacing any loaded one.
    /// Returns (gpu layers, context size, load milliseconds).
    fn load_model(&self, id: i64) -> zbus::Result<(u32, u32, u64)>;

    /// Unloads whatever model is loaded, freeing its memory.
    fn unload_model(&self) -> zbus::Result<()>;

    /// (state, model name). State is `unloaded`, `loading`, `ready`, or
    /// `generating`.
    fn model_status(&self) -> zbus::Result<(String, String)>;

    /// Starts generating a reply to `prompt`. Returns a request id; the
    /// text arrives through `GenerationText` signals and ends with
    /// `GenerationDone` or `GenerationFailed`. Debug use for now.
    fn generate(&self, prompt: &str, max_tokens: u32) -> zbus::Result<u64>;

    /// Thunderbird's mail accounts and their folders, read from the
    /// default profile. Debug use for now.
    fn list_mail_accounts(&self) -> zbus::Result<Vec<MailAccountDto>>;

    /// Mail Belvedere has seen, most recent first. Debug use for now.
    fn list_recent_mail(&self, limit: u32) -> zbus::Result<Vec<MailMessageDto>>;

    /// Suggested tasks awaiting a yes or no, newest first.
    fn list_suggestions(&self) -> zbus::Result<Vec<SuggestionDto>>;

    /// Turns a suggestion into a task.
    fn accept_suggestion(&self, id: i64) -> zbus::Result<TaskDto>;

    /// Drops a suggestion for good.
    fn reject_suggestion(&self, id: i64) -> zbus::Result<()>;

    /// Opens the email a task came from in Thunderbird. Errors if the
    /// task has no email source.
    fn open_email(&self, task_id: i64) -> zbus::Result<()>;

    /// Fired when suggestions are added or resolved.
    #[zbus(signal)]
    fn suggestions_changed(&self) -> zbus::Result<()>;

    /// Conversations, most recently active first.
    fn list_conversations(&self) -> zbus::Result<Vec<ConversationDto>>;

    /// Starts an empty conversation.
    fn new_conversation(&self) -> zbus::Result<ConversationDto>;

    /// Messages of one conversation, oldest first.
    fn get_messages(&self, conversation_id: i64) -> zbus::Result<Vec<MessageDto>>;

    /// Stores the user's message and starts Belvedere's reply. Returns a
    /// request id; the reply streams in `ChatText` signals and ends with
    /// `ChatDone` or `ChatFailed`. `ChatStatus` reports progress such as
    /// a model being loaded first.
    fn send_message(&self, conversation_id: i64, text: &str) -> zbus::Result<u64>;

    /// Stops the reply in progress; what was produced so far is kept.
    fn stop_generation(&self, request: u64) -> zbus::Result<()>;

    #[zbus(signal)]
    fn chat_status(&self, request: u64, conversation_id: i64, status: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn chat_text(&self, request: u64, conversation_id: i64, text: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn chat_done(&self, request: u64, conversation_id: i64, message_id: i64) -> zbus::Result<()>;

    #[zbus(signal)]
    fn chat_failed(&self, request: u64, conversation_id: i64, message: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn generation_text(&self, request: u64, text: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn generation_done(&self, request: u64, tokens: u32, seconds: f64) -> zbus::Result<()>;

    #[zbus(signal)]
    fn generation_failed(&self, request: u64, message: &str) -> zbus::Result<()>;

    /// Fired after any change to any task.
    #[zbus(signal)]
    fn tasks_changed(&self) -> zbus::Result<()>;

    /// Asks the window to show one task, e.g. after a notification was
    /// clicked.
    #[zbus(signal)]
    fn show_task(&self, id: i64) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dto_signature_is_stable() {
        // Changing this breaks every client; do it on purpose or not at all.
        assert_eq!(TaskDto::SIGNATURE.to_string(), "(xsssssssssss)");
    }

    #[test]
    fn optional_fields_become_empty_strings() {
        let task = Task {
            id: 7,
            title: "Pay the electric bill".into(),
            notes: String::new(),
            due_at: None,
            status: TaskStatus::Dismissed,
            dismiss_reason: Some("already paid".into()),
            created_at: "2026-10-03T00:00:00.000Z".into(),
            updated_at: "2026-10-03T00:00:00.000Z".into(),
            completed_at: None,
            deleted_at: None,
            kind: String::new(),
            reference: String::new(),
        };
        let dto = TaskDto::from(task);
        assert_eq!(dto.source_kind, "");
        assert_eq!(dto.due_at, "");
        assert_eq!(dto.due_at(), None);
        assert_eq!(dto.status, "dismissed");
        assert_eq!(dto.dismiss_reason, "already paid");
        assert!(!dto.is_deleted());
    }
}
