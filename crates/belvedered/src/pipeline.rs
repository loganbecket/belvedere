//! Mail becomes tasks: each newly seen message is read by the model and,
//! depending on how sure it is, becomes a task (with a notification) or a
//! suggestion for the user to accept or reject.
//!
//! This path can create tasks and suggestions, and mark a "reply needed"
//! task done once a reply to its email shows up in a sent folder. Nothing
//! else: no tools, no chat, no access to anything but the message text,
//! the task store, and the notifier.

use std::collections::HashMap;
use std::time::Duration;

use belvedere_core::db::{Db, MailMessage, NewSuggestion, NewTask, SourceKind, Task, TaskStatus};
use belvedere_core::engine::{self, Engine, State};
use belvedere_core::extract::{self, EmailInput, Extraction};
use belvedere_core::ipc::OBJECT_PATH;
use belvedere_core::schedule;
use belvedere_core::thunderbird::{self, FolderRole};
use belvedere_core::tools::due_from_parts;
use chrono::Local;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::chat::pick_chat_model;
use crate::dbus::{Service, SharedDb};
use crate::notify::{Notifier, Subject};

/// Confidence at or above which an extraction becomes a task outright;
/// below it, a suggestion. Setting `extract_confidence_threshold`.
pub const DEFAULT_THRESHOLD: f64 = 0.75;

/// Days before the due date for the extra reminder on mail-born tasks.
pub const LEAD_DAYS: i64 = 3;

/// What to do with one message.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Confident: make the task.
    Task(Extraction),
    /// Possible: ask the user.
    Suggest(Extraction),
    /// Nothing to do.
    Nothing,
}

/// The decision rule, kept pure so it can be tested.
pub fn decide(extraction: &Extraction, threshold: f64) -> Decision {
    if !extraction.action_needed || extraction.title.trim().is_empty() {
        return Decision::Nothing;
    }
    if f64::from(extraction.confidence) >= threshold {
        Decision::Task(extraction.clone())
    } else {
        Decision::Suggest(extraction.clone())
    }
}

/// Folders whose mail never becomes a task.
pub fn skipped_role(role: FolderRole) -> bool {
    matches!(
        role,
        FolderRole::Junk
            | FolderRole::Trash
            | FolderRole::Drafts
            | FolderRole::Sent
            | FolderRole::Outbox
            | FolderRole::Templates
    )
}

/// The notes a mail-born task carries: who it is from and what it was about.
pub fn task_notes(m: &MailMessage, e: &Extraction) -> String {
    let mut notes = String::new();
    let who = if !e.from_whom.is_empty() {
        e.from_whom.clone()
    } else if !m.from_name.is_empty() {
        m.from_name.clone()
    } else {
        m.from_addr.clone()
    };
    notes.push_str(&format!("From {who}"));
    if !m.from_addr.is_empty() && who != m.from_addr {
        notes.push_str(&format!(" <{}>", m.from_addr));
    }
    notes.push_str(&format!(": {}", m.subject));
    if let Some(a) = e.amount {
        notes.push_str(&format!("\nAmount: ${a:.2}"));
    }
    notes
}

fn lock(db: &SharedDb) -> std::sync::MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(|e| e.into_inner())
}

/// Creates the task for a confident extraction (or an accepted
/// suggestion): task, link to the email, reminders including the lead.
pub fn create_task_from_mail(db: &Db, m: &MailMessage, e: &Extraction) -> Result<Task, String> {
    let now = Local::now();
    let due = due_from_parts(e.due_date.as_deref(), e.due_time.as_deref(), now)?;
    let task = db
        .create_task(&NewTask {
            title: e.title.clone(),
            notes: task_notes(m, e),
            due_at: due.clone(),
        })
        .map_err(|e| e.to_string())?;
    db.add_task_source(task.id, SourceKind::Email, &m.message_id, &m.subject)
        .map_err(|e| e.to_string())?;
    let task = db
        .set_task_kind(task.id, e.kind.as_str())
        .map_err(|e| e.to_string())?;
    if let Some(due) = &due {
        let plan = schedule::plan_reminders_with_lead(due, now, LEAD_DAYS);
        db.replace_task_reminders(task.id, &plan)
            .map_err(|e| e.to_string())?;
    }
    Ok(task)
}

/// A message the user sent answers the emails it replies to: any open
/// "reply needed" task made from one of those emails is done. Returns
/// the tasks closed. Other kinds of task (a bill, say) are left alone,
/// since answering a bill's email is not paying it.
pub fn close_replied(db: &Db, sent: &MailMessage) -> Vec<Task> {
    let mut closed = Vec::new();
    for answered in sent.replies_to_ids() {
        let ids = match db.tasks_for_source(SourceKind::Email, answered) {
            Ok(ids) => ids,
            Err(err) => {
                warn!("could not look up tasks for {answered}: {err}");
                continue;
            }
        };
        for id in ids {
            let Ok(task) = db.get_task(id) else { continue };
            if task.status != TaskStatus::Open
                || task.deleted_at.is_some()
                || task.kind != extract::Kind::ReplyNeeded.as_str()
            {
                continue;
            }
            let done = db
                .complete_task(id)
                .and_then(|t| db.cancel_task_reminders(id).map(|_| t));
            match done {
                Ok(t) => {
                    info!(task = id, reply = sent.message_id, "replied; task done");
                    closed.push(t);
                }
                Err(err) => warn!(task = id, "could not close replied task: {err}"),
            }
        }
    }
    closed
}

/// Runs forever: processes unhandled mail at startup and whenever the
/// watcher or the sweep reports new mail.
pub async fn run(
    db: SharedDb,
    engine: Engine,
    bus: zbus::Connection,
    mut new_mail: mpsc::UnboundedReceiver<usize>,
) {
    let mut notifier = match Notifier::new(&bus).await {
        Ok(n) => Some(n),
        Err(err) => {
            warn!("notifications unavailable for new tasks: {err}");
            None
        }
    };
    loop {
        process_pending(&db, &engine, &bus, notifier.as_mut()).await;
        // Wait for the watcher or the sweep; re-check hourly regardless.
        tokio::select! {
            _ = new_mail.recv() => {}
            _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
        }
    }
}

/// Role of each (account, folder) the mail could have come from.
fn folder_roles() -> HashMap<(String, String), FolderRole> {
    let mut roles = HashMap::new();
    let Some(profile) = crate::mail::profile() else {
        return roles;
    };
    if let Ok(accounts) = thunderbird::accounts(&profile.dir) {
        for a in accounts {
            for f in a.folders {
                roles.insert((a.name.clone(), f.path), f.role);
            }
        }
    }
    roles
}

async fn process_pending(
    db: &SharedDb,
    engine: &Engine,
    bus: &zbus::Connection,
    mut notifier: Option<&mut Notifier>,
) {
    let pending = match lock(db).unprocessed_mail(50) {
        Ok(p) => p,
        Err(err) => {
            warn!("could not list unprocessed mail: {err}");
            return;
        }
    };
    if pending.is_empty() {
        return;
    }
    let roles = folder_roles();
    let threshold = lock(db)
        .get_setting("extract_confidence_threshold")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(DEFAULT_THRESHOLD);

    for m in pending {
        // Mail in junk, trash, drafts, sent, or outbox is never a task.
        let role = roles
            .get(&(m.account.clone(), m.folder.clone()))
            .copied()
            .unwrap_or(FolderRole::Other);
        if skipped_role(role) {
            if role == FolderRole::Sent && !close_replied(&lock(db), &m).is_empty() {
                announce_tasks(bus).await;
            }
            let _ = lock(db).mark_mail_processed(m.id);
            continue;
        }

        // Never compete with the user's own chat.
        while matches!(engine.state(), State::Generating { .. }) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if !matches!(engine.state(), State::Ready { .. }) {
            let picked = pick_chat_model(&lock(db));
            let Some(model) = picked else {
                warn!("no model available; leaving mail unprocessed for now");
                return;
            };
            if let Err(err) = engine
                .load(
                    model.path.clone().into(),
                    model.name.clone(),
                    engine::gpu_enabled(),
                )
                .await
            {
                warn!(
                    "could not load {}: {err}; leaving mail unprocessed",
                    model.name
                );
                return;
            }
        }

        let email = EmailInput {
            from_name: m.from_name.clone(),
            from_addr: m.from_addr.clone(),
            subject: m.subject.clone(),
            date: m.date.clone(),
            body: m.body_text.clone(),
            attachments: serde_json::from_str(&m.attachments).unwrap_or_default(),
        };
        let done = extract::extract(engine, &email, Local::now(), false).await;
        if let Some(err) = &done.error {
            warn!(
                mail = m.id,
                "extraction failed: {err}; treating as nothing to do"
            );
        }
        match decide(&done.result, threshold) {
            Decision::Nothing => {}
            Decision::Task(e) => {
                let created = create_task_from_mail(&lock(db), &m, &e);
                match created {
                    Ok(task) => {
                        info!(
                            task = task.id,
                            mail = m.id,
                            kind = e.kind.as_str(),
                            "task created from mail"
                        );
                        announce_tasks(bus).await;
                        if let Some(n) = notifier.as_deref_mut() {
                            let body = match task.due_at.as_deref() {
                                Some(due) => format!(
                                    "New task from your mail. Due {}.",
                                    schedule::due_label(due, Local::now())
                                ),
                                None => "New task from your mail.".to_string(),
                            };
                            let subject = Subject {
                                task_id: task.id,
                                reminder_id: 0,
                            };
                            if let Err(err) = n.remind(subject, &task.title, &body).await {
                                warn!("could not announce the new task: {err}");
                            }
                        }
                    }
                    Err(err) => warn!(mail = m.id, "could not create task: {err}"),
                }
            }
            Decision::Suggest(e) => {
                let due =
                    due_from_parts(e.due_date.as_deref(), e.due_time.as_deref(), Local::now())
                        .unwrap_or(None);
                let created = lock(db).create_suggestion(&NewSuggestion {
                    mail_message_id: m.id,
                    title: e.title.clone(),
                    notes: task_notes(&m, &e),
                    due_at: due,
                    kind: e.kind.as_str().to_string(),
                    amount: e.amount,
                    confidence: f64::from(e.confidence),
                });
                match created {
                    Ok(s) => {
                        info!(suggestion = s.id, mail = m.id, "suggestion made from mail");
                        announce_suggestions(bus).await;
                    }
                    Err(err) => warn!(mail = m.id, "could not record suggestion: {err}"),
                }
            }
        }
        let _ = lock(db).mark_mail_processed(m.id);
    }
}

async fn announce_tasks(bus: &zbus::Connection) {
    if let Ok(iface) = bus
        .object_server()
        .interface::<_, Service>(OBJECT_PATH)
        .await
    {
        let _ = Service::tasks_changed(iface.signal_emitter()).await;
    }
}

pub async fn announce_suggestions(bus: &zbus::Connection) {
    if let Ok(iface) = bus
        .object_server()
        .interface::<_, Service>(OBJECT_PATH)
        .await
    {
        let _ = Service::suggestions_changed(iface.signal_emitter()).await;
    }
}

/// Opens a message in Thunderbird by its Message-ID. Thunderbird accepts
/// `mid:` links on its command line; the Flatpak needs `flatpak run`.
pub fn open_in_thunderbird(message_id: &str) -> Result<(), String> {
    let mid = format!("mid:{}", message_id.trim().trim_matches(['<', '>']));
    let profile = crate::mail::profile().ok_or("no Thunderbird profile")?;
    let attempts: Vec<(&str, Vec<String>)> = match profile.source.as_str() {
        s if s.contains("ESR") => vec![(
            "flatpak",
            vec![
                "run".into(),
                "org.mozilla.thunderbird_esr".into(),
                mid.clone(),
            ],
        )],
        "Flatpak" => vec![(
            "flatpak",
            vec!["run".into(), "org.mozilla.Thunderbird".into(), mid.clone()],
        )],
        _ => vec![("thunderbird", vec![mid.clone()])],
    };
    for (program, args) in attempts {
        if std::process::Command::new(program)
            .args(&args)
            .spawn()
            .is_ok()
        {
            return Ok(());
        }
    }
    Err("could not start Thunderbird".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use belvedere_core::db::NewMailMessage;
    use belvedere_core::extract::Kind;

    fn bill(conf: f32) -> Extraction {
        Extraction {
            action_needed: true,
            kind: Kind::Bill,
            title: "Pay the electric bill".into(),
            due_date: Some("2099-10-31".into()),
            due_time: None,
            amount: Some(84.12),
            from_whom: "City Power".into(),
            confidence: conf,
        }
    }

    fn mail() -> MailMessage {
        MailMessage {
            id: 1,
            message_id: "<bill-1@example.invalid>".into(),
            account: "acct".into(),
            folder: "INBOX".into(),
            from_addr: "billing@citypower.invalid".into(),
            from_name: "City Power Billing".into(),
            to_addrs: "me@example.invalid".into(),
            subject: "Your October statement".into(),
            date: "2026-10-15T09:00:00-05:00".into(),
            body_text: "Amount due $84.12".into(),
            attachments: "[]".into(),
            mbox_path: "/p/INBOX".into(),
            mbox_offset: 0,
            seen_at: "2026-10-15T14:00:00.000Z".into(),
            processed_at: None,
            replies_to: String::new(),
        }
    }

    #[test]
    fn decision_follows_confidence_and_action() {
        assert!(matches!(decide(&bill(0.9), 0.75), Decision::Task(_)));
        assert!(matches!(decide(&bill(0.75), 0.75), Decision::Task(_)));
        assert!(matches!(decide(&bill(0.6), 0.75), Decision::Suggest(_)));
        assert_eq!(decide(&Extraction::nothing(), 0.75), Decision::Nothing);
        let mut no_title = bill(0.9);
        no_title.title = "  ".into();
        assert_eq!(decide(&no_title, 0.75), Decision::Nothing);
    }

    #[test]
    fn skipped_roles_are_the_ones_the_plan_names() {
        for r in [
            FolderRole::Junk,
            FolderRole::Trash,
            FolderRole::Drafts,
            FolderRole::Sent,
        ] {
            assert!(skipped_role(r), "{r:?}");
        }
        for r in [FolderRole::Inbox, FolderRole::Archive, FolderRole::Other] {
            assert!(!skipped_role(r), "{r:?}");
        }
    }

    #[test]
    fn notes_name_sender_subject_and_amount() {
        let n = task_notes(&mail(), &bill(0.9));
        assert_eq!(
            n,
            "From City Power <billing@citypower.invalid>: Your October statement\nAmount: $84.12"
        );
    }

    #[test]
    fn task_from_mail_links_source_and_plans_reminders_with_lead() {
        let db = Db::open_in_memory().unwrap();
        let task = create_task_from_mail(&db, &mail(), &bill(0.9)).unwrap();
        assert_eq!(task.title, "Pay the electric bill");
        let sources = db.task_sources(task.id).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].kind, SourceKind::Email);
        assert_eq!(sources[0].reference, "<bill-1@example.invalid>");
        assert_eq!(sources[0].label, "Your October statement");
        let reminders = db.task_reminders(task.id).unwrap();
        // Due day 9:00 and three days before.
        assert_eq!(reminders.len(), 2);
        assert!(reminders[0].fire_at < reminders[1].fire_at);
    }

    #[test]
    fn a_reply_closes_only_open_reply_needed_tasks_for_that_email() {
        let db = Db::open_in_memory().unwrap();
        let (asked, _) = db.record_mail(&new_mail("<q1@example.invalid>")).unwrap();
        let (billed, _) = db.record_mail(&new_mail("<b1@example.invalid>")).unwrap();
        let reply_task = create_task_from_mail(
            &db,
            &asked,
            &Extraction {
                kind: extract::Kind::ReplyNeeded,
                title: "Reply to Pat about the field trip".into(),
                ..bill(0.9)
            },
        )
        .unwrap();
        let bill_task = create_task_from_mail(&db, &billed, &bill(0.9)).unwrap();
        assert_eq!(reply_task.kind, "reply_needed");
        assert_eq!(bill_task.kind, "bill");

        // A reply to both: only the reply-needed task closes.
        let (sent, _) = db
            .record_mail(&NewMailMessage {
                replies_to: vec!["<b1@example.invalid>".into(), "<q1@example.invalid>".into()],
                ..new_mail("<s1@example.invalid>")
            })
            .unwrap();
        let closed = close_replied(&db, &sent);
        assert_eq!(
            closed.iter().map(|t| t.id).collect::<Vec<_>>(),
            [reply_task.id]
        );
        assert_eq!(db.get_task(reply_task.id).unwrap().status, TaskStatus::Done);
        assert_eq!(db.get_task(bill_task.id).unwrap().status, TaskStatus::Open);
        assert!(db
            .task_reminders(reply_task.id)
            .unwrap()
            .iter()
            .all(|r| r.fired_at.is_some()));

        // Again: nothing left to close. A reply to something else: nothing.
        assert!(close_replied(&db, &sent).is_empty());
        let (other, _) = db
            .record_mail(&NewMailMessage {
                replies_to: vec!["<zzz@example.invalid>".into()],
                ..new_mail("<s2@example.invalid>")
            })
            .unwrap();
        assert!(close_replied(&db, &other).is_empty());
    }

    fn new_mail(message_id: &str) -> NewMailMessage {
        NewMailMessage {
            message_id: message_id.into(),
            account: "acct".into(),
            folder: "INBOX".into(),
            from_addr: "pat@example.invalid".into(),
            from_name: "Pat".into(),
            subject: "Field trip".into(),
            date: "2026-10-15T09:00:00-05:00".into(),
            mbox_path: "/p/INBOX".into(),
            ..Default::default()
        }
    }

    /// The background path's only ways to change anything are the
    /// functions above (a task, a suggestion, a reply closing a task); it
    /// never touches tools.
    #[test]
    fn background_path_has_no_tool_access() {
        // Only the real code counts; this test's own words don't.
        let src = include_str!("pipeline.rs");
        let src = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            !src.contains("agent::"),
            "pipeline must not use the chat agent"
        );
        assert!(
            !src.contains("tools::execute"),
            "pipeline must not call task tools"
        );
        assert!(!src.contains("delete_task"), "pipeline must never delete");
        assert!(!src.contains("dismiss_task"), "pipeline must never dismiss");
        // Completing a task happens in exactly one place: a sent reply
        // closing a "reply needed" task.
        let closer = src
            .find("pub fn close_replied")
            .expect("close_replied exists");
        let closer_end = closer + src[closer..].find("\n}\n").unwrap();
        let uses: Vec<usize> = src.match_indices("complete_task").map(|(i, _)| i).collect();
        assert_eq!(uses.len(), 1, "complete_task used outside close_replied");
        assert!(uses[0] > closer && uses[0] < closer_end);
    }
}
