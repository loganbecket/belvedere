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
use belvedere_core::matter::{self, Candidate, Incoming};
use belvedere_core::schedule;
use belvedere_core::thunderbird::{self, FolderRole};
use belvedere_core::tools::due_from_parts;
use chrono::{Local, NaiveDate};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::chat::pick_chat_model;
use crate::dbus::{Service, SharedDb};
use crate::notify::{SharedNotifier, Subject};

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
        .set_task_kind(
            task.id,
            e.kind.as_str(),
            e.reference.as_deref().unwrap_or(""),
        )
        .map_err(|e| e.to_string())?;
    if let Some(due) = &due {
        let plan = schedule::plan_reminders_with_lead(due, now, LEAD_DAYS);
        db.replace_task_reminders(task.id, &plan)
            .map_err(|e| e.to_string())?;
    }
    Ok(task)
}

/// How many open mail-born tasks a follow-up is checked against.
const CANDIDATE_LIMIT: usize = 200;

fn day_of(rfc3339: &str) -> Option<NaiveDate> {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|d| d.with_timezone(&Local).date_naive())
}

/// The open mail-born tasks, with the facts about their emails that the
/// matcher needs. Newest first.
pub fn candidates(db: &Db) -> Vec<Candidate> {
    let tasks = match db.mail_tasks(CANDIDATE_LIMIT) {
        Ok(t) => t,
        Err(err) => {
            warn!("could not list tasks for matching: {err}");
            return Vec::new();
        }
    };
    let mut out = Vec::with_capacity(tasks.len());
    for t in tasks {
        let mut c = Candidate {
            task_id: t.id,
            kind: t.kind.clone(),
            title: t.title.clone(),
            due_date: t.due_at.as_deref().and_then(day_of),
            reference: t.reference.clone(),
            amount: amount_in_notes(&t.notes),
            closed: t.status != TaskStatus::Open,
            ..Default::default()
        };
        for s in db.task_sources(t.id).unwrap_or_default() {
            if s.kind != SourceKind::Email {
                continue;
            }
            c.message_ids.push(s.reference.clone());
            c.subjects.push(s.label.clone());
            if let Ok(Some(m)) = db.mail_by_message_id(&s.reference) {
                if !m.from_addr.is_empty() {
                    c.senders.push(m.from_addr.to_lowercase());
                }
                let date = day_of(&m.date);
                if date > c.last_mail_date {
                    c.last_mail_date = date;
                }
            }
        }
        out.push(c);
    }
    out
}

/// The new email in the matcher's terms.
pub fn incoming(m: &MailMessage, e: &Extraction) -> Incoming {
    Incoming {
        kind: e.kind.as_str().to_string(),
        title: e.title.clone(),
        due_date: e
            .due_date
            .as_deref()
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()),
        reference: e.reference.clone(),
        sender: m.from_addr.to_lowercase(),
        subject: m.subject.clone(),
        replies_to: m.replies_to_ids().into_iter().map(str::to_string).collect(),
        mail_date: day_of(&m.date),
        amount: e.amount,
    }
}

/// The latest amount a task's notes record ("Amount: $84.12", then
/// "Amount now: $94.12" after an update).
pub fn amount_in_notes(notes: &str) -> Option<f64> {
    notes
        .lines()
        .filter_map(|l| {
            l.strip_prefix("Amount: $")
                .or_else(|| l.strip_prefix("Amount now: $"))
        })
        .filter_map(|v| v.trim().parse::<f64>().ok())
        .next_back()
}

/// Proof arrived (a payment confirmation, say) for an open task: mark it
/// done, cancel its reminders, link the email, and note it in the history.
/// The caller announces it with an Undo button.
pub fn close_on_proof(db: &Db, task: &Task, m: &MailMessage) -> Result<Task, String> {
    db.add_task_source(task.id, SourceKind::Email, &m.message_id, &m.subject)
        .map_err(|e| e.to_string())?;
    let when = day_of(&m.date)
        .map(|d| d.format("%b %-d").to_string())
        .unwrap_or_else(|| "later".into());
    let line = format!("Closed {when}: {}", m.subject.trim());
    let notes = if task.notes.trim().is_empty() {
        line
    } else {
        format!("{}\n{line}", task.notes.trim_end())
    };
    db.update_task(
        task.id,
        &NewTask {
            title: task.title.clone(),
            notes,
            due_at: task.due_at.clone(),
        },
    )
    .map_err(|e| e.to_string())?;
    let done = db.complete_task(task.id).map_err(|e| e.to_string())?;
    db.cancel_task_reminders(task.id)
        .map_err(|e| e.to_string())?;
    Ok(done)
}

/// What a follow-up changed about its task.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Followed {
    pub due_changed: bool,
    pub amount_changed: bool,
}

/// Records a follow-up email on an existing task: the email becomes
/// another source, the notes gain a history line, and a new due date
/// moves the task (and its reminders). The title is kept. A task already
/// done or dismissed only gets the source and the history line: it is
/// not reopened, its date does not move, and nothing is announced.
pub fn attach_to_task(
    db: &Db,
    task: &Task,
    m: &MailMessage,
    e: &Extraction,
) -> Result<(Task, Followed), String> {
    let now = Local::now();
    let mut followed = Followed::default();
    db.add_task_source(task.id, SourceKind::Email, &m.message_id, &m.subject)
        .map_err(|err| err.to_string())?;

    let when = day_of(&m.date)
        .map(|d| d.format("%b %-d").to_string())
        .unwrap_or_else(|| "later".into());
    let mut line = format!("Update {when}: {}", m.subject.trim());
    if let Some(a) = e.amount {
        let previous = task
            .notes
            .lines()
            .filter_map(|l| {
                l.strip_prefix("Amount: $")
                    .or_else(|| l.strip_prefix("Amount now: $"))
            })
            .next_back()
            .and_then(|v| v.trim().parse::<f64>().ok());
        if previous.is_none_or(|p| (p - a).abs() >= 0.005) {
            line.push_str(&format!(
                "
Amount now: ${a:.2}"
            ));
            followed.amount_changed = previous.is_some();
        }
    }
    let notes = if task.notes.trim().is_empty() {
        line
    } else {
        format!(
            "{}
{line}",
            task.notes.trim_end()
        )
    };

    let closed = task.status != TaskStatus::Open;
    let new_due = if closed {
        None
    } else {
        due_from_parts(e.due_date.as_deref(), e.due_time.as_deref(), now)?
    };
    if closed {
        followed.amount_changed = false;
    }
    let due = match (&new_due, &task.due_at) {
        (Some(n), Some(old)) if day_of(n) != day_of(old) => {
            followed.due_changed = true;
            Some(n.clone())
        }
        (Some(n), None) => {
            followed.due_changed = true;
            Some(n.clone())
        }
        _ => task.due_at.clone(),
    };
    let updated = db
        .update_task(
            task.id,
            &NewTask {
                title: task.title.clone(),
                notes,
                due_at: due.clone(),
            },
        )
        .map_err(|err| err.to_string())?;
    if followed.due_changed {
        if let Some(due) = &due {
            let plan = schedule::plan_reminders_with_lead(due, now, LEAD_DAYS);
            db.replace_task_reminders(task.id, &plan)
                .map_err(|err| err.to_string())?;
        }
    }
    if task.reference.is_empty() {
        if let Some(r) = &e.reference {
            db.set_task_kind(task.id, &task.kind, r)
                .map_err(|err| err.to_string())?;
        }
    }
    Ok((updated, followed))
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
    notifier: Option<SharedNotifier>,
) {
    loop {
        process_pending(&db, &engine, &bus, notifier.as_ref()).await;
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
    notifier: Option<&SharedNotifier>,
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
        // Proof that something got done closes the task it settles.
        if done.result.confirms_done {
            let settled = {
                let db = lock(db);
                let found = matter::find_completed(&incoming(&m, &done.result), &candidates(&db));
                found.and_then(|(id, reason)| db.get_task(id).ok().map(|t| (t, reason)))
            };
            if let Some((task, reason)) = settled {
                let closed = close_on_proof(&lock(db), &task, &m);
                match closed {
                    Ok(task) => {
                        info!(task = task.id, mail = m.id, reason = ?reason, "task closed on proof from mail");
                        announce_tasks(bus).await;
                        if let Some(n) = notifier {
                            let body = format!("Marked done from your mail: {}", m.subject.trim());
                            let subject = Subject {
                                task_id: task.id,
                                reminder_id: 0,
                            };
                            let shown = n
                                .lock()
                                .await
                                .announce_closed(subject, &task.title, &body)
                                .await;
                            if let Err(err) = shown {
                                warn!("could not announce the closed task: {err}");
                            }
                        }
                    }
                    Err(err) => warn!(
                        task = task.id,
                        mail = m.id,
                        "could not close on proof: {err}"
                    ),
                }
            } else {
                info!(
                    mail = m.id,
                    "confirmation matched no open task; nothing closed"
                );
            }
            let _ = lock(db).mark_mail_processed(m.id);
            continue;
        }

        // An email about something already on the list updates that task
        // instead of making another, however sure the model was.
        let matched = if done.result.action_needed {
            let db = lock(db);
            let found = matter::find_matter(&incoming(&m, &done.result), &candidates(&db));
            found.and_then(|(id, reason)| db.get_task(id).ok().map(|t| (t, reason)))
        } else {
            None
        };
        if let Some((task, reason)) = matched {
            let attached = attach_to_task(&lock(db), &task, &m, &done.result);
            match attached {
                Ok((task, followed)) => {
                    info!(
                        task = task.id,
                        mail = m.id,
                        reason = ?reason,
                        due_changed = followed.due_changed,
                        "follow-up attached to task"
                    );
                    announce_tasks(bus).await;
                    if followed.due_changed || followed.amount_changed {
                        if let Some(n) = notifier {
                            let body = match task.due_at.as_deref() {
                                Some(due) if followed.due_changed => format!(
                                    "Updated from your mail. Now due {}.",
                                    schedule::due_label(due, Local::now())
                                ),
                                _ => "Updated from your mail.".to_string(),
                            };
                            let subject = Subject {
                                task_id: task.id,
                                reminder_id: 0,
                            };
                            let shown = n.lock().await.remind(subject, &task.title, &body).await;
                            if let Err(err) = shown {
                                warn!("could not announce the update: {err}");
                            }
                        }
                    }
                }
                Err(err) => warn!(
                    task = task.id,
                    mail = m.id,
                    "could not attach follow-up: {err}"
                ),
            }
            let _ = lock(db).mark_mail_processed(m.id);
            continue;
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
                        if let Some(n) = notifier {
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
                            let shown = n.lock().await.remind(subject, &task.title, &body).await;
                            if let Err(err) = shown {
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
            reference: Some("4471-02".into()),
            confirms_done: false,
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

    #[test]
    fn proof_closes_the_task_links_the_email_and_notes_it() {
        let db = Db::open_in_memory().unwrap();
        let from_power = |id: &str| NewMailMessage {
            from_addr: "billing@citypower.invalid".into(),
            from_name: "City Power".into(),
            subject: "Your October statement is ready".into(),
            ..new_mail(id)
        };
        let (first, _) = db
            .record_mail(&from_power("<stmt@citypower.invalid>"))
            .unwrap();
        let task = create_task_from_mail(&db, &first, &bill(0.9)).unwrap();
        let (receipt, _) = db
            .record_mail(&NewMailMessage {
                subject: "Thank you for your payment".into(),
                date: "2026-10-28T09:00:00-05:00".into(),
                ..from_power("<rcpt@citypower.invalid>")
            })
            .unwrap();
        let proof = Extraction {
            action_needed: false,
            kind: extract::Kind::None,
            title: String::new(),
            due_date: None,
            confirms_done: true,
            ..bill(0.9)
        };
        let found = matter::find_completed(&incoming(&receipt, &proof), &candidates(&db));
        assert_eq!(found.map(|(id, _)| id), Some(task.id));
        let done = close_on_proof(&db, &task, &receipt).unwrap();
        assert_eq!(done.status, TaskStatus::Done);
        assert!(done
            .notes
            .contains("Closed Oct 28: Thank you for your payment"));
        assert_eq!(db.task_sources(task.id).unwrap().len(), 2);
        assert!(db
            .task_reminders(task.id)
            .unwrap()
            .iter()
            .all(|r| r.fired_at.is_some()));
        // Closed, so a second receipt finds nothing to close.
        assert_eq!(
            matter::find_completed(&incoming(&receipt, &proof), &candidates(&db)),
            None
        );
    }

    #[test]
    fn a_follow_up_to_a_dismissed_task_lands_quietly_without_reopening() {
        let db = Db::open_in_memory().unwrap();
        let from_power = |id: &str| NewMailMessage {
            from_addr: "billing@citypower.invalid".into(),
            from_name: "City Power".into(),
            subject: "Your October statement is ready".into(),
            ..new_mail(id)
        };
        let (first, _) = db
            .record_mail(&from_power("<stmt@citypower.invalid>"))
            .unwrap();
        let task = create_task_from_mail(&db, &first, &bill(0.9)).unwrap();
        let task = db.dismiss_task(task.id, "already paid").unwrap();
        db.cancel_task_reminders(task.id).unwrap();

        let (reminder, _) = db
            .record_mail(&NewMailMessage {
                subject: "Reminder: payment due".into(),
                date: "2026-10-26T09:00:00-05:00".into(),
                ..from_power("<rem@citypower.invalid>")
            })
            .unwrap();
        let followup = Extraction {
            due_date: Some("2099-11-07".into()),
            amount: Some(99.0),
            ..bill(0.9)
        };
        // Still matched, so it does not become a new task...
        let found = matter::find_matter(&incoming(&reminder, &followup), &candidates(&db));
        assert_eq!(found.map(|(id, _)| id), Some(task.id));
        let (after, followed) = attach_to_task(&db, &task, &reminder, &followup).unwrap();
        // ...but nothing that would reopen it or make noise.
        assert_eq!(followed, Followed::default());
        assert_eq!(after.status, TaskStatus::Dismissed);
        assert_eq!(after.dismiss_reason.as_deref(), Some("already paid"));
        assert_eq!(after.due_at, task.due_at);
        assert!(after.notes.contains("Update Oct 26: Reminder: payment due"));
        assert!(db
            .task_reminders(task.id)
            .unwrap()
            .iter()
            .all(|r| r.fired_at.is_some()));
        assert_eq!(db.list_tasks().unwrap().len(), 1);
    }

    #[test]
    fn a_follow_up_updates_the_task_instead_of_making_another() {
        let db = Db::open_in_memory().unwrap();
        let from_power = |id: &str| NewMailMessage {
            from_addr: "billing@citypower.invalid".into(),
            from_name: "City Power".into(),
            subject: "Your October statement is ready".into(),
            ..new_mail(id)
        };
        let (first, _) = db
            .record_mail(&from_power("<stmt@citypower.invalid>"))
            .unwrap();
        let task = create_task_from_mail(&db, &first, &bill(0.9)).unwrap();
        assert_eq!(task.reference, "4471-02");

        // A reminder: same account, due date pushed two weeks, higher amount.
        let (reminder, _) = db
            .record_mail(&NewMailMessage {
                subject: "FINAL NOTICE: payment past due".into(),
                date: "2026-11-03T09:00:00-05:00".into(),
                ..from_power("<final@citypower.invalid>")
            })
            .unwrap();
        let followup = Extraction {
            due_date: Some("2099-11-14".into()),
            amount: Some(94.12),
            ..bill(0.6)
        };
        let found = matter::find_matter(&incoming(&reminder, &followup), &candidates(&db));
        assert_eq!(found.map(|(id, _)| id), Some(task.id));
        let (updated, followed) = attach_to_task(&db, &task, &reminder, &followup).unwrap();
        assert!(followed.due_changed && followed.amount_changed);
        assert_eq!(updated.title, task.title, "the title is kept");
        assert!(updated.due_at.unwrap().starts_with("2099-11-14"));
        assert!(updated
            .notes
            .contains("Update Nov 3: FINAL NOTICE: payment past due"));
        assert!(updated.notes.contains("Amount now: $94.12"));
        assert_eq!(db.task_sources(task.id).unwrap().len(), 2);
        assert_eq!(db.list_tasks().unwrap().len(), 1);
        // Reminders follow the new date: due day and three days before.
        let fire: Vec<_> = db
            .task_reminders(task.id)
            .unwrap()
            .into_iter()
            .filter(|r| r.fired_at.is_none())
            .map(|r| r.fire_at)
            .collect();
        assert_eq!(fire.len(), 2);
        assert!(fire.iter().all(|f| f.starts_with("2099-11-1")));

        // Next month's statement is a new matter.
        let (november, _) = db
            .record_mail(&NewMailMessage {
                subject: "Your November statement is ready".into(),
                date: "2026-11-15T09:00:00-05:00".into(),
                ..from_power("<stmt-nov@citypower.invalid>")
            })
            .unwrap();
        let next = Extraction {
            due_date: Some("2099-12-14".into()),
            ..bill(0.9)
        };
        assert_eq!(
            matter::find_matter(&incoming(&november, &next), &candidates(&db)),
            None
        );
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
        // Completing a task happens in exactly two places: a sent reply
        // closing a "reply needed" task, and proof from the mail closing
        // the task it settles.
        let span = |name: &str| {
            let start = src.find(name).unwrap_or_else(|| panic!("{name} exists"));
            (start, start + src[start..].find("\n}\n").unwrap())
        };
        let allowed = [span("pub fn close_replied"), span("pub fn close_on_proof")];
        let uses: Vec<usize> = src.match_indices("complete_task").map(|(i, _)| i).collect();
        assert_eq!(uses.len(), 2, "complete_task used somewhere new");
        for u in uses {
            assert!(
                allowed.iter().any(|(a, b)| u > *a && u < *b),
                "complete_task outside the two closers"
            );
        }
    }
}
