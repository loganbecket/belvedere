//! How the window and the panel applet talk to the background service:
//! a session D-Bus interface, `org.belvedere.Service`.
//!
//! This module holds the shared wire types and the client side. The
//! service side lives in `belvedered`.

use serde::{Deserialize, Serialize};
use zbus::zvariant::Type;

use crate::db::{Task, TaskStatus};

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

    /// Marks a done or dismissed task open again.
    fn reopen_task(&self, id: i64) -> zbus::Result<TaskDto>;

    /// Fired after any change to any task.
    #[zbus(signal)]
    fn tasks_changed(&self) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dto_signature_is_stable() {
        // Changing this breaks every client; do it on purpose or not at all.
        assert_eq!(TaskDto::SIGNATURE.to_string(), "(xsssssssss)");
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
        };
        let dto = TaskDto::from(task);
        assert_eq!(dto.due_at, "");
        assert_eq!(dto.due_at(), None);
        assert_eq!(dto.status, "dismissed");
        assert_eq!(dto.dismiss_reason, "already paid");
        assert!(!dto.is_deleted());
    }
}
