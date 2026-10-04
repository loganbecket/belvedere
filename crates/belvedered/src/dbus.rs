//! The service side of `org.belvedere.Service`.

use std::sync::{Arc, Mutex};

use belvedere_core::db::{Db, DbError, NewTask};
use belvedere_core::ipc::{TaskDto, BUS_NAME, OBJECT_PATH};
use tracing::info;
use zbus::object_server::SignalEmitter;
use zbus::{fdo, interface};

/// Shared handle to the database. SQLite connections are not `Sync`, so
/// every call takes the lock for the duration of one query.
pub type SharedDb = Arc<Mutex<Db>>;

pub struct Service {
    db: SharedDb,
}

impl Service {
    pub fn new(db: SharedDb) -> Self {
        Self { db }
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Db> {
        // A poisoned lock means another call panicked mid-query. The
        // connection itself is still fine; carry on.
        self.db.lock().unwrap_or_else(|e| e.into_inner())
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
        let task = self.db().update_task(id, &new).map_err(to_fdo)?;
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

    #[zbus(signal)]
    async fn tasks_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Connects to the session bus, exports the service object, and claims
/// the bus name. The returned connection must be kept alive.
pub async fn serve(db: SharedDb) -> zbus::Result<zbus::Connection> {
    let conn = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, Service::new(db))?
        .build()
        .await?;
    info!(name = BUS_NAME, path = OBJECT_PATH, "D-Bus service ready");
    Ok(conn)
}
