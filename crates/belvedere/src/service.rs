//! Keeps the window in touch with the background service.
//!
//! One long-lived stream: it connects to the session bus, pings the
//! service once a second, reloads the task list whenever the service
//! says tasks changed, and reports when the service goes away or comes
//! back.

use std::time::Duration;

use belvedere_core::ipc::{ServiceProxy, TaskDto};
use cosmic::iced::futures::channel::mpsc::Sender;
use cosmic::iced::futures::{SinkExt, StreamExt};
use cosmic::iced::{stream, Subscription};

/// What the stream tells the app.
#[derive(Debug, Clone)]
pub enum Event {
    /// The service answered and here is the current task list.
    Connected(Vec<TaskDto>),
    /// Tasks changed; here is the new list.
    Tasks(Vec<TaskDto>),
    /// The service stopped answering.
    Disconnected,
}

/// How often to ping while connected, and how often to retry while not.
const PING_EVERY: Duration = Duration::from_secs(1);
const RETRY_EVERY: Duration = Duration::from_millis(500);

pub fn subscription() -> Subscription<Event> {
    Subscription::run(events)
}

fn events() -> impl cosmic::iced::futures::Stream<Item = Event> {
    stream::channel(16, |mut out: Sender<Event>| async move {
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
            run_until_disconnected(&proxy, &mut out).await;
        }
    })
}

/// Drives one connected session, returning when the service stops
/// answering. The caller reconnects.
async fn run_until_disconnected(proxy: &ServiceProxy<'_>, out: &mut Sender<Event>) {
    // Wait for the service to show up.
    loop {
        if proxy.ping().await.is_ok() {
            break;
        }
        tokio::time::sleep(RETRY_EVERY).await;
    }

    let tasks = proxy.list_tasks().await.unwrap_or_default();
    if out.send(Event::Connected(tasks)).await.is_err() {
        return;
    }

    let mut changes = match proxy.receive_tasks_changed().await {
        Ok(stream) => stream,
        Err(_) => {
            let _ = out.send(Event::Disconnected).await;
            return;
        }
    };
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;

    loop {
        tokio::select! {
            change = changes.next() => {
                if change.is_none() {
                    // Signal stream closed: the bus connection is gone.
                    let _ = out.send(Event::Disconnected).await;
                    return;
                }
                match proxy.list_tasks().await {
                    Ok(tasks) => {
                        if out.send(Event::Tasks(tasks)).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = out.send(Event::Disconnected).await;
                        return;
                    }
                }
            }
            _ = ping.tick() => {
                if proxy.ping().await.is_err() {
                    let _ = out.send(Event::Disconnected).await;
                    return;
                }
            }
        }
    }
}
