//! Desktop notifications through the standard `org.freedesktop.Notifications`
//! interface, which COSMIC's notification daemon implements.
//!
//! Every reminder notification carries three buttons (Done, Snooze an
//! hour, Not needed) and a default action for clicking the body (show the
//! task in the window). This module sends them and reports clicks back.

use std::collections::HashMap;

use futures_util::StreamExt;
use tracing::{info, warn};
use zbus::zvariant::Value;

/// Which button was pressed on a reminder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The body was clicked.
    Open,
    Done,
    SnoozeHour,
    NotNeeded,
    /// Reopen a task Belvedere closed on its own.
    Undo,
}

/// A notifier shared by the parts of the service that show notifications
/// and the one loop that hears the clicks.
pub type SharedNotifier = std::sync::Arc<tokio::sync::Mutex<Notifier>>;

impl Action {
    const KEYS: [(&'static str, Action); 5] = [
        ("default", Action::Open),
        ("done", Action::Done),
        ("snooze", Action::SnoozeHour),
        ("dismiss", Action::NotNeeded),
        ("undo", Action::Undo),
    ];

    fn from_key(key: &str) -> Option<Action> {
        Self::KEYS.iter().find(|(k, _)| *k == key).map(|(_, a)| *a)
    }

    /// The `actions` array the spec wants: key, label, key, label, ...
    fn spec_list() -> Vec<&'static str> {
        vec![
            "default",
            "Open",
            "done",
            "Done",
            "snooze",
            "Snooze 1 hour",
            "dismiss",
            "Not needed",
        ]
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<&str>,
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// Something a notification is about, so a click can be acted on.
#[derive(Debug, Clone, Copy)]
pub struct Subject {
    pub task_id: i64,
    pub reminder_id: i64,
}

/// What a click on a notification means for us.
#[derive(Debug, Clone, Copy)]
pub struct Clicked {
    pub subject: Subject,
    pub action: Action,
}

pub struct Notifier {
    proxy: NotificationsProxy<'static>,
    /// Notification id -> what it was about. Cleared when closed.
    open: HashMap<u32, Subject>,
}

impl Notifier {
    pub async fn new(conn: &zbus::Connection) -> zbus::Result<Self> {
        Ok(Notifier {
            proxy: NotificationsProxy::new(conn).await?,
            open: HashMap::new(),
        })
    }

    /// Sends a reminder. Never expires on its own; the user decides.
    pub async fn remind(&mut self, subject: Subject, title: &str, body: &str) -> zbus::Result<u32> {
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("desktop-entry", Value::from("org.belvedere.Belvedere"));
        hints.insert("urgency", Value::from(1u8));
        hints.insert("category", Value::from("reminder"));
        let id = self
            .proxy
            .notify(
                "Belvedere",
                0,
                "org.belvedere.Belvedere",
                title,
                body,
                Action::spec_list(),
                hints,
                0,
            )
            .await?;
        self.open.insert(id, subject);
        info!(notification = id, task = subject.task_id, "reminder shown");
        Ok(id)
    }

    /// Announces a task Belvedere closed on its own (proof arrived in the
    /// mail), with an Undo button. Low urgency: nothing is being asked.
    pub async fn announce_closed(
        &mut self,
        subject: Subject,
        title: &str,
        body: &str,
    ) -> zbus::Result<u32> {
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("desktop-entry", Value::from("org.belvedere.Belvedere"));
        hints.insert("urgency", Value::from(0u8));
        let id = self
            .proxy
            .notify(
                "Belvedere",
                0,
                "org.belvedere.Belvedere",
                title,
                body,
                vec!["default", "Open", "undo", "Undo"],
                hints,
                15_000,
            )
            .await?;
        self.open.insert(id, subject);
        info!(
            notification = id,
            task = subject.task_id,
            "closed task announced"
        );
        Ok(id)
    }

    /// A reminder about a calendar event. Normal urgency, no buttons (the
    /// event lives in Thunderbird); stays until dismissed.
    pub async fn event_reminder(&mut self, title: &str, body: &str) -> zbus::Result<u32> {
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("desktop-entry", Value::from("org.belvedere.Belvedere"));
        hints.insert("urgency", Value::from(1u8));
        hints.insert("category", Value::from("reminder"));
        let id = self
            .proxy
            .notify(
                "Belvedere",
                0,
                "org.belvedere.Belvedere",
                title,
                body,
                vec![],
                hints,
                0,
            )
            .await?;
        info!(notification = id, "event reminder shown");
        Ok(id)
    }

    /// A quiet mention with no task behind it (a bill a rule says is on
    /// autopay). Low urgency, goes away on its own, no buttons.
    pub async fn heads_up(
        &mut self,
        subject: Subject,
        title: &str,
        body: &str,
    ) -> zbus::Result<u32> {
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("desktop-entry", Value::from("org.belvedere.Belvedere"));
        hints.insert("urgency", Value::from(0u8));
        let id = self
            .proxy
            .notify(
                "Belvedere",
                0,
                "org.belvedere.Belvedere",
                title,
                body,
                vec![],
                hints,
                15_000,
            )
            .await?;
        self.open.insert(id, subject);
        info!(notification = id, "heads-up shown");
        Ok(id)
    }

    /// Streams clicks on our notifications until the bus goes away.
    pub async fn clicks(&mut self) -> zbus::Result<ClickStream> {
        Ok(ClickStream {
            actions: self.proxy.receive_action_invoked().await?,
            closed: self.proxy.receive_notification_closed().await?,
        })
    }

    /// Resolves a raw signal into a click on one of ours, if it was.
    pub fn resolve(&mut self, id: u32, key: &str) -> Option<Clicked> {
        let subject = self.open.remove(&id)?;
        match Action::from_key(key) {
            Some(action) => Some(Clicked { subject, action }),
            None => {
                warn!(key, "unknown notification action");
                None
            }
        }
    }

    pub fn forget(&mut self, id: u32) {
        self.open.remove(&id);
    }
}

/// The two signal streams, read together.
pub struct ClickStream {
    actions: ActionInvokedStream,
    closed: NotificationClosedStream,
}

/// A raw signal from the notification daemon.
#[derive(Debug)]
pub enum Signal {
    Action {
        id: u32,
        key: String,
    },
    Closed {
        id: u32,
    },
    /// A signal we couldn't read; skip it.
    Ignore,
    /// The daemon's streams ended.
    Gone,
}

impl ClickStream {
    pub async fn next(&mut self) -> Signal {
        tokio::select! {
            action = self.actions.next() => match action {
                Some(signal) => match signal.args() {
                    Ok(args) => Signal::Action { id: args.id, key: args.action_key.to_string() },
                    Err(_) => Signal::Ignore,
                },
                None => Signal::Gone,
            },
            closed = self.closed.next() => match closed {
                Some(signal) => match signal.args() {
                    Ok(args) => Signal::Closed { id: args.id },
                    Err(_) => Signal::Ignore,
                },
                None => Signal::Gone,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_keys_round_trip() {
        let list = Action::spec_list();
        assert_eq!(list.len() % 2, 0);
        for pair in list.chunks(2) {
            assert!(
                Action::from_key(pair[0]).is_some(),
                "{} has no action",
                pair[0]
            );
        }
        assert_eq!(Action::from_key("default"), Some(Action::Open));
        assert_eq!(Action::from_key("nope"), None);
    }
}
