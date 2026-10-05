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

use crate::ipc::{
    CalendarTaskDto, DownloadDto, ModelDto, RuleDto, ServiceProxy, SettingDto, SuggestionDto,
    TaskDto,
};

/// Live tasks and soft-deleted tasks, fetched together.
#[derive(Debug, Clone, Default)]
pub struct Lists {
    pub tasks: Vec<TaskDto>,
    pub deleted: Vec<TaskDto>,
    pub suggestions: Vec<SuggestionDto>,
    pub rules: Vec<RuleDto>,
    /// Thunderbird's own tasks (its other calendars), read-only here.
    pub calendar_tasks: Vec<CalendarTaskDto>,
    pub models: Vec<ModelDto>,
    /// Ids of the chat and background models (0 = none).
    pub model_roles: (i64, i64),
    pub downloads: Vec<DownloadDto>,
    pub settings: Vec<SettingDto>,
}

/// What the loop tells the client.
#[derive(Debug, Clone)]
pub enum Event {
    /// The service answered; here is a proxy to talk to it and the
    /// current lists.
    Connected(ServiceProxy<'static>, Lists),
    /// Tasks changed; here are the new lists.
    Tasks(Lists),
    /// The service wants one task shown (a notification was clicked).
    ShowTask(i64),
    /// The service wants a conversation shown (a briefing was clicked).
    ShowConversation(i64),
    /// Progress on a chat reply: "loading model", "thinking", ...
    ChatStatus {
        request: u64,
        conversation_id: i64,
        status: String,
    },
    /// A piece of a chat reply.
    ChatText {
        request: u64,
        conversation_id: i64,
        text: String,
    },
    /// A chat reply finished and was saved as `message_id`.
    ChatDone {
        request: u64,
        conversation_id: i64,
        message_id: i64,
    },
    /// A chat reply failed.
    ChatFailed {
        request: u64,
        conversation_id: i64,
        message: String,
    },
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
        suggestions: proxy.list_suggestions().await.unwrap_or_default(),
        rules: proxy.list_rules().await.unwrap_or_default(),
        calendar_tasks: proxy.list_calendar_tasks().await.unwrap_or_default(),
        models: proxy.list_models().await.unwrap_or_default(),
        model_roles: proxy.model_roles().await.unwrap_or((0, 0)),
        downloads: proxy.list_downloads().await.unwrap_or_default(),
        settings: proxy.list_settings().await.unwrap_or_default(),
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
    let mut suggestions_changed = match proxy.receive_suggestions_changed().await {
        Ok(stream) => stream,
        Err(_) => {
            out.send(Event::Disconnected).await.map_err(|_| ())?;
            return Ok(());
        }
    };
    let mut settings_changed = match proxy.receive_settings_changed().await {
        Ok(stream) => stream,
        Err(_) => {
            out.send(Event::Disconnected).await.map_err(|_| ())?;
            return Ok(());
        }
    };
    let mut downloads_changed = match proxy.receive_downloads_changed().await {
        Ok(stream) => stream,
        Err(_) => {
            out.send(Event::Disconnected).await.map_err(|_| ())?;
            return Ok(());
        }
    };
    let mut models_changed = match proxy.receive_models_changed().await {
        Ok(stream) => stream,
        Err(_) => {
            out.send(Event::Disconnected).await.map_err(|_| ())?;
            return Ok(());
        }
    };
    let mut calendar_changed = match proxy.receive_calendar_changed().await {
        Ok(stream) => stream,
        Err(_) => {
            out.send(Event::Disconnected).await.map_err(|_| ())?;
            return Ok(());
        }
    };
    let mut rules_changed = match proxy.receive_rules_changed().await {
        Ok(stream) => stream,
        Err(_) => {
            out.send(Event::Disconnected).await.map_err(|_| ())?;
            return Ok(());
        }
    };
    let mut show_conversation = match proxy.receive_show_conversation().await {
        Ok(stream) => stream,
        Err(_) => {
            out.send(Event::Disconnected).await.map_err(|_| ())?;
            return Ok(());
        }
    };
    let mut show = match proxy.receive_show_task().await {
        Ok(stream) => stream,
        Err(_) => {
            out.send(Event::Disconnected).await.map_err(|_| ())?;
            return Ok(());
        }
    };
    let (Ok(mut chat_status), Ok(mut chat_text), Ok(mut chat_done), Ok(mut chat_failed)) = (
        proxy.receive_chat_status().await,
        proxy.receive_chat_text().await,
        proxy.receive_chat_done().await,
        proxy.receive_chat_failed().await,
    ) else {
        out.send(Event::Disconnected).await.map_err(|_| ())?;
        return Ok(());
    };
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;

    loop {
        tokio::select! {
            Some(signal) = chat_status.next() => {
                if let Ok(a) = signal.args() {
                    out.send(Event::ChatStatus { request: a.request, conversation_id: a.conversation_id, status: a.status.to_string() }).await.map_err(|_| ())?;
                }
            }
            Some(signal) = chat_text.next() => {
                if let Ok(a) = signal.args() {
                    out.send(Event::ChatText { request: a.request, conversation_id: a.conversation_id, text: a.text.to_string() }).await.map_err(|_| ())?;
                }
            }
            Some(signal) = chat_done.next() => {
                if let Ok(a) = signal.args() {
                    out.send(Event::ChatDone { request: a.request, conversation_id: a.conversation_id, message_id: a.message_id }).await.map_err(|_| ())?;
                }
            }
            Some(signal) = chat_failed.next() => {
                if let Ok(a) = signal.args() {
                    out.send(Event::ChatFailed { request: a.request, conversation_id: a.conversation_id, message: a.message.to_string() }).await.map_err(|_| ())?;
                }
            }
            Some(_) = settings_changed.next() => {
                match fetch(&proxy).await {
                    Ok(lists) => out.send(Event::Tasks(lists)).await.map_err(|_| ())?,
                    Err(_) => {
                        out.send(Event::Disconnected).await.map_err(|_| ())?;
                        return Ok(());
                    }
                }
            }
            Some(_) = downloads_changed.next() => {
                match fetch(&proxy).await {
                    Ok(lists) => out.send(Event::Tasks(lists)).await.map_err(|_| ())?,
                    Err(_) => {
                        out.send(Event::Disconnected).await.map_err(|_| ())?;
                        return Ok(());
                    }
                }
            }
            Some(_) = models_changed.next() => {
                match fetch(&proxy).await {
                    Ok(lists) => out.send(Event::Tasks(lists)).await.map_err(|_| ())?,
                    Err(_) => {
                        out.send(Event::Disconnected).await.map_err(|_| ())?;
                        return Ok(());
                    }
                }
            }
            Some(_) = calendar_changed.next() => {
                match fetch(&proxy).await {
                    Ok(lists) => out.send(Event::Tasks(lists)).await.map_err(|_| ())?,
                    Err(_) => {
                        out.send(Event::Disconnected).await.map_err(|_| ())?;
                        return Ok(());
                    }
                }
            }
            Some(_) = rules_changed.next() => {
                match fetch(&proxy).await {
                    Ok(lists) => out.send(Event::Tasks(lists)).await.map_err(|_| ())?,
                    Err(_) => {
                        out.send(Event::Disconnected).await.map_err(|_| ())?;
                        return Ok(());
                    }
                }
            }
            Some(_) = suggestions_changed.next() => {
                match fetch(&proxy).await {
                    Ok(lists) => out.send(Event::Tasks(lists)).await.map_err(|_| ())?,
                    Err(_) => {
                        out.send(Event::Disconnected).await.map_err(|_| ())?;
                        return Ok(());
                    }
                }
            }
            signal = show.next() => {
                if let Some(signal) = signal {
                    if let Ok(args) = signal.args() {
                        out.send(Event::ShowTask(args.id)).await.map_err(|_| ())?;
                    }
                }
            }
            Some(signal) = show_conversation.next() => {
                if let Ok(args) = signal.args() {
                    out.send(Event::ShowConversation(args.id)).await.map_err(|_| ())?;
                }
            }
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
