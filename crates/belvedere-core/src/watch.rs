//! Keeps a client (the window, the panel applet) in touch with the
//! background service.
//!
//! One long-running loop: it connects to the session bus, pings the
//! service once a second, reloads the task lists whenever the service
//! says tasks changed, and reports when the service goes away or comes
//! back. It also hands the client a proxy to make its own calls with.

use std::time::Duration;

use futures_channel::mpsc::Sender;
use futures_util::{SinkExt, StreamExt};

use crate::ipc::{ServiceProxy, TaskDto};

/// Live tasks and soft-deleted tasks, fetched together.
#[derive(Debug, Clone, Default)]
pub struct Lists {
    pub tasks: Vec<TaskDto>,
    pub deleted: Vec<TaskDto>,
}

/// What the loop tells the client.
#[derive(Debug, Clone)]
pub enum Event {
    /// The service answered; here is a proxy to talk to it and the
    /// current lists.
    Connected(ServiceProxy<'static>, Lists),
    /// Tasks changed; here are the new lists.
    Tasks(Lists),
    /// The service stopped answering.
    Disconnected,
}

/// How often to ping while connected, and how often to retry while not.
const PING_EVERY: Duration = Duration::from_secs(1);
const RETRY_EVERY: Duration = Duration::from_millis(500);

async fn fetch(proxy: &ServiceProxy<'static>) -> zbus::Result<Lists> {
    Ok(Lists {
        tasks: proxy.list_tasks().await?,
        deleted: proxy.list_deleted_tasks().await?,
    })
}

/// Runs forever, sending events into `out`. Returns only if the receiver
/// is dropped.
pub async fn watch(mut out: Sender<Event>) {
    loop {
        let conn = match zbus::Connection::session().await {
            Ok(c) => c,
            Err(_) => {
                // No session bus at all; nothing to do but wait.
                tokio::time::sleep(PING_EVERY).await;
                continue;
            }
        };
        let proxy = match ServiceProxy::new(&conn).await {
            Ok(p) => p,
            Err(_) => {
                tokio::time::sleep(PING_EVERY).await;
                continue;
            }
        };
        if run_until_disconnected(proxy, &mut out).await.is_err() {
            return;
        }
    }
}

/// Drives one connected session, returning when the service stops
/// answering (`Ok`) or the receiver went away (`Err`).
async fn run_until_disconnected(
    proxy: ServiceProxy<'static>,
    out: &mut Sender<Event>,
) -> Result<(), ()> {
    // Wait for the service to show up.
    loop {
        if proxy.ping().await.is_ok() {
            break;
        }
        tokio::time::sleep(RETRY_EVERY).await;
    }

    let lists = fetch(&proxy).await.unwrap_or_default();
    out.send(Event::Connected(proxy.clone(), lists))
        .await
        .map_err(|_| ())?;

    let mut changes = match proxy.receive_tasks_changed().await {
        Ok(stream) => stream,
        Err(_) => {
            out.send(Event::Disconnected).await.map_err(|_| ())?;
            return Ok(());
        }
    };
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;

    loop {
        tokio::select! {
            change = changes.next() => {
                if change.is_none() {
                    // Signal stream closed: the bus connection is gone.
                    out.send(Event::Disconnected).await.map_err(|_| ())?;
                    return Ok(());
                }
                match fetch(&proxy).await {
                    Ok(lists) => out.send(Event::Tasks(lists)).await.map_err(|_| ())?,
                    Err(_) => {
                        out.send(Event::Disconnected).await.map_err(|_| ())?;
                        return Ok(());
                    }
                }
            }
            _ = ping.tick() => {
                if proxy.ping().await.is_err() {
                    out.send(Event::Disconnected).await.map_err(|_| ())?;
                    return Ok(());
                }
            }
        }
    }
}
