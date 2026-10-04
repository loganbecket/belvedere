//! The window itself.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use belvedere_core::ipc::{ServiceProxy, TaskDto};
use belvedere_core::schedule::{self, DueInput, Section};
use chrono::Local;
use cosmic::app::{Core, Task};
use cosmic::iced::{Alignment, Length, Subscription};
use cosmic::widget::{self, button, container, text};
use cosmic::{Application, ApplicationExt, Element};

use crate::service::{self, Lists};

/// How long the Undo offer stays up after a delete.
pub const UNDO_WINDOW: Duration = Duration::from_secs(10);

pub struct Belvedere {
    core: Core,
    /// What's typed in the chat box but not yet sent.
    draft: String,
    /// Lines sent so far. Nothing answers yet; that arrives with the model.
    transcript: Vec<String>,
    /// Live connection to the service, once it has answered.
    service: Option<ServiceProxy<'static>>,
    lists: Lists,
    /// Whether the background service is answering.
    connected: bool,
    /// Title typed into the "new task" box.
    new_title: String,
    /// Sections the user has folded shut.
    collapsed: HashSet<Section>,
    /// The task open in the editor, if any.
    editor: Option<Editor>,
    /// A delete waiting for the user to say yes.
    confirm_delete: Option<TaskDto>,
    /// A recent delete that can still be undone.
    undo: Option<Undo>,
    /// Something went wrong talking to the service; shown briefly.
    error: Option<String>,
}

/// The edit form for one task.
#[derive(Debug, Clone)]
pub struct Editor {
    pub id: i64,
    pub title: String,
    pub notes: String,
    pub due: DueInput,
    pub problem: Option<String>,
}

impl Editor {
    fn for_task(task: &TaskDto) -> Self {
        Editor {
            id: task.id,
            title: task.title.clone(),
            notes: task.notes.clone(),
            due: task
                .due_at()
                .map(DueInput::from_rfc3339)
                .unwrap_or(DueInput {
                    date: String::new(),
                    time: String::new(),
                }),
            problem: None,
        }
    }
}

/// A delete the user can still take back.
#[derive(Debug, Clone)]
pub struct Undo {
    pub task: TaskDto,
    pub expires: Instant,
}

impl Undo {
    pub fn new(task: TaskDto, now: Instant) -> Self {
        Undo {
            task,
            expires: now + UNDO_WINDOW,
        }
    }

    pub fn expired(&self, now: Instant) -> bool {
        now >= self.expires
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    // Chat
    Draft(String),
    Send,
    // Service
    Service(service::Event),
    /// A call to the service finished; `Err` holds a message to show.
    Done(Result<(), String>),
    // Tasks
    NewTitle(String),
    CreateTask,
    SetDone(i64, bool),
    ToggleSection(Section),
    Open(i64),
    EditTitle(String),
    EditNotes(String),
    EditDate(String),
    EditTime(String),
    SaveEdit,
    CloseEditor,
    AskDelete(i64),
    CancelDelete,
    ConfirmDelete,
    UndoDelete,
    Restore(i64),
    Tick,
}

impl Application for Belvedere {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = "org.belvedere.Belvedere";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: ()) -> (Self, Task<Message>) {
        let mut app = Belvedere {
            core,
            draft: String::new(),
            transcript: Vec::new(),
            service: None,
            lists: Lists::default(),
            connected: false,
            new_title: String::new(),
            collapsed: Section::ALL
                .into_iter()
                .filter(|s| s.collapsed_by_default())
                .collect(),
            editor: None,
            confirm_delete: None,
            undo: None,
            error: None,
        };
        let title = match app.core.main_window_id() {
            Some(id) => app.set_window_title("Belvedere".to_string(), id),
            None => Task::none(),
        };
        (app, title)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Draft(text) => self.draft = text,
            Message::Send => {
                let line = self.draft.trim().to_string();
                if !line.is_empty() {
                    self.transcript.push(line);
                    self.draft.clear();
                }
            }

            Message::Service(event) => match event {
                service::Event::Connected(proxy, lists) => {
                    self.connected = true;
                    self.service = Some(proxy);
                    self.lists = lists;
                }
                service::Event::Tasks(lists) => self.lists = lists,
                service::Event::Disconnected => {
                    self.connected = false;
                    self.service = None;
                }
            },
            Message::Done(Ok(())) => {}
            Message::Done(Err(problem)) => self.error = Some(problem),

            Message::NewTitle(title) => self.new_title = title,
            Message::CreateTask => {
                let title = self.new_title.trim().to_string();
                if title.is_empty() {
                    return Task::none();
                }
                self.new_title.clear();
                return self
                    .call(move |p| async move { p.create_task(&title, "", "").await.map(|_| ()) });
            }
            Message::SetDone(id, done) => {
                return self.call(move |p| async move {
                    if done {
                        p.complete_task(id).await.map(|_| ())
                    } else {
                        p.reopen_task(id).await.map(|_| ())
                    }
                });
            }
            Message::ToggleSection(section) => {
                if !self.collapsed.remove(&section) {
                    self.collapsed.insert(section);
                }
            }

            Message::Open(id) => {
                if let Some(task) = self.find(id) {
                    self.editor = Some(Editor::for_task(task));
                }
            }
            Message::EditTitle(v) => self.edit(|e| e.title = v),
            Message::EditNotes(v) => self.edit(|e| e.notes = v),
            Message::EditDate(v) => self.edit(|e| e.due.date = v),
            Message::EditTime(v) => self.edit(|e| e.due.time = v),
            Message::SaveEdit => {
                let Some(editor) = self.editor.as_mut() else {
                    return Task::none();
                };
                let title = editor.title.trim().to_string();
                if title.is_empty() {
                    editor.problem = Some("A task needs a title".to_string());
                    return Task::none();
                }
                let due = match editor.due.to_rfc3339() {
                    Ok(due) => due.unwrap_or_default(),
                    Err(problem) => {
                        editor.problem = Some(problem);
                        return Task::none();
                    }
                };
                let id = editor.id;
                let notes = editor.notes.clone();
                self.editor = None;
                return self.call(move |p| async move {
                    p.update_task(id, &title, &notes, &due).await.map(|_| ())
                });
            }
            Message::CloseEditor => self.editor = None,

            Message::AskDelete(id) => {
                if let Some(task) = self.find(id) {
                    self.confirm_delete = Some(task.clone());
                }
            }
            Message::CancelDelete => self.confirm_delete = None,
            Message::ConfirmDelete => {
                let Some(task) = self.confirm_delete.take() else {
                    return Task::none();
                };
                let id = task.id;
                if self.editor.as_ref().is_some_and(|e| e.id == id) {
                    self.editor = None;
                }
                self.undo = Some(Undo::new(task, Instant::now()));
                return self.call(move |p| async move { p.delete_task(id).await.map(|_| ()) });
            }
            Message::UndoDelete => {
                if let Some(undo) = self.undo.take() {
                    let id = undo.task.id;
                    return self.call(move |p| async move { p.restore_task(id).await.map(|_| ()) });
                }
            }
            Message::Restore(id) => {
                return self.call(move |p| async move { p.restore_task(id).await.map(|_| ()) });
            }
            Message::Tick => {
                let now = Instant::now();
                if self.undo.as_ref().is_some_and(|u| u.expired(now)) {
                    self.undo = None;
                }
                self.error = None;
            }
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![service::subscription().map(Message::Service)];
        // The undo bar and error line time themselves out; also lets the
        // section headings roll over at midnight without a restart.
        if self.undo.is_some() || self.error.is_some() {
            subs.push(cosmic::iced::time::every(Duration::from_millis(500)).map(|_| Message::Tick));
        } else {
            subs.push(cosmic::iced::time::every(Duration::from_secs(60)).map(|_| Message::Tick));
        }
        Subscription::batch(subs)
    }

    fn dialog(&self) -> Option<Element<'_, Message>> {
        let task = self.confirm_delete.as_ref()?;
        Some(
            widget::dialog()
                .title("Delete this task?")
                .body(format!(
                    "\"{}\" will move to Deleted, where you can restore it later.",
                    task.title
                ))
                .primary_action(button::destructive("Delete").on_press(Message::ConfirmDelete))
                .secondary_action(button::standard("Cancel").on_press(Message::CancelDelete))
                .into(),
        )
    }

    fn view(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        let panes = widget::row::with_capacity(2)
            .spacing(spacing.space_s)
            .push(self.chat_pane())
            .push(self.task_pane())
            .height(Length::Fill);

        let mut page = widget::column::with_capacity(4).spacing(spacing.space_s);
        if !self.connected {
            page = page.push(widget::warning(
                "Belvedere's background service isn't running. Tasks will appear once it's back.",
            ));
        }
        if let Some(error) = &self.error {
            page = page.push(widget::warning(error.as_str()));
        }
        if let Some(undo) = &self.undo {
            page = page.push(
                container(
                    widget::row::with_capacity(3)
                        .align_y(Alignment::Center)
                        .spacing(spacing.space_s)
                        .push(text::body(format!("Deleted \"{}\"", undo.task.title)))
                        .push(cosmic::iced::widget::space().width(Length::Fill))
                        .push(button::text("Undo").on_press(Message::UndoDelete)),
                )
                .padding(spacing.space_xs)
                .width(Length::Fill)
                .class(cosmic::theme::Container::Card),
            );
        }
        page = page.push(panes);

        container(page)
            .padding(spacing.space_s)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

impl Belvedere {
    /// Runs one call against the service, reporting failure as a message.
    fn call<F, Fut>(&self, f: F) -> Task<Message>
    where
        F: FnOnce(ServiceProxy<'static>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = zbus::Result<()>> + Send + 'static,
    {
        let Some(proxy) = self.service.clone() else {
            return Task::done(cosmic::Action::App(Message::Done(Err(
                "The background service isn't running.".to_string(),
            ))));
        };
        Task::perform(f(proxy), |result| {
            cosmic::Action::App(Message::Done(result.map_err(|e| e.to_string())))
        })
    }

    fn find(&self, id: i64) -> Option<&TaskDto> {
        self.lists
            .tasks
            .iter()
            .chain(self.lists.deleted.iter())
            .find(|t| t.id == id)
    }

    fn edit(&mut self, f: impl FnOnce(&mut Editor)) {
        if let Some(editor) = self.editor.as_mut() {
            editor.problem = None;
            f(editor);
        }
    }

    fn chat_pane(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        let mut lines =
            widget::column::with_capacity(self.transcript.len().max(1)).spacing(spacing.space_xs);
        if self.transcript.is_empty() {
            lines = lines.push(text::body(
                "Say something to Belvedere. Replies arrive once a model is connected.",
            ));
        }
        for line in &self.transcript {
            lines = lines.push(
                container(text::body(line.as_str()))
                    .padding(spacing.space_xs)
                    .class(cosmic::theme::Container::Card),
            );
        }

        let input = widget::text_input("Ask Belvedere…", &self.draft)
            .on_input(Message::Draft)
            .on_submit(|_| Message::Send);

        widget::column::with_capacity(3)
            .spacing(spacing.space_s)
            .push(text::title4("Chat"))
            .push(widget::scrollable(lines).height(Length::Fill))
            .push(input)
            .width(Length::FillPortion(3))
            .into()
    }

    fn task_pane(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        let body: Element<'_, Message> = match &self.editor {
            Some(editor) => self.editor_view(editor),
            None => self.list_view(),
        };

        widget::column::with_capacity(3)
            .spacing(spacing.space_s)
            .push(
                widget::row::with_capacity(3)
                    .align_y(Alignment::Center)
                    .push(text::title4("Tasks"))
                    .push(cosmic::iced::widget::space().width(Length::Fill))
                    .push(text::caption(format!("{}", self.lists.tasks.len()))),
            )
            .push(body)
            .width(Length::FillPortion(2))
            .into()
    }

    fn list_view(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let now = Local::now();

        let new_task = widget::text_input("New task…", &self.new_title)
            .on_input(Message::NewTitle)
            .on_submit(|_| Message::CreateTask);

        let groups = schedule::group(
            self.lists.tasks.iter().chain(self.lists.deleted.iter()),
            now,
        );

        let mut list = widget::column::with_capacity(16).spacing(spacing.space_xs);
        let mut any = false;
        for (section, tasks) in groups {
            if tasks.is_empty() {
                continue;
            }
            any = true;
            let collapsed = self.collapsed.contains(&section);
            let arrow = if collapsed { "▸" } else { "▾" };
            list = list.push(
                button::text(format!("{arrow} {}  ·  {}", section.title(), tasks.len()))
                    .on_press(Message::ToggleSection(section)),
            );
            if collapsed {
                continue;
            }
            for task in tasks {
                list = list.push(task_row(task, section, now));
            }
        }
        if !any {
            let note = if self.connected {
                "No tasks yet. Type one above."
            } else {
                "Waiting for the service…"
            };
            list = list.push(text::body(note));
        }

        widget::column::with_capacity(2)
            .spacing(spacing.space_s)
            .push(new_task)
            .push(widget::scrollable(list).height(Length::Fill))
            .into()
    }

    fn editor_view<'a>(&'a self, editor: &'a Editor) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();

        let mut form = widget::column::with_capacity(8)
            .spacing(spacing.space_s)
            .push(
                widget::text_input("Title", &editor.title)
                    .label("Title")
                    .on_input(Message::EditTitle)
                    .on_submit(|_| Message::SaveEdit),
            )
            .push(
                widget::text_input("Notes", &editor.notes)
                    .label("Notes")
                    .on_input(Message::EditNotes),
            )
            .push(
                widget::row::with_capacity(2)
                    .spacing(spacing.space_s)
                    .push(
                        widget::text_input("2026-10-20", &editor.due.date)
                            .label("Due date")
                            .on_input(Message::EditDate)
                            .on_submit(|_| Message::SaveEdit),
                    )
                    .push(
                        widget::text_input("09:00", &editor.due.time)
                            .label("Time")
                            .on_input(Message::EditTime)
                            .on_submit(|_| Message::SaveEdit),
                    ),
            );
        if let Some(problem) = &editor.problem {
            form = form.push(widget::warning(problem.as_str()));
        }

        let is_done = self.find(editor.id).is_some_and(|t| t.status != "open");
        let is_deleted = self.find(editor.id).is_some_and(|t| t.is_deleted());

        let mut actions = widget::row::with_capacity(5)
            .spacing(spacing.space_s)
            .push(button::suggested("Save").on_press(Message::SaveEdit))
            .push(button::standard("Cancel").on_press(Message::CloseEditor))
            .push(cosmic::iced::widget::space().width(Length::Fill));
        if is_deleted {
            actions =
                actions.push(button::standard("Restore").on_press(Message::Restore(editor.id)));
        } else {
            actions = actions
                .push(
                    button::standard(if is_done { "Reopen" } else { "Mark done" })
                        .on_press(Message::SetDone(editor.id, !is_done)),
                )
                .push(button::destructive("Delete").on_press(Message::AskDelete(editor.id)));
        }
        form = form.push(actions);

        container(form)
            .padding(spacing.space_s)
            .width(Length::Fill)
            .class(cosmic::theme::Container::Card)
            .into()
    }
}

fn task_row<'a>(
    task: &'a TaskDto,
    section: Section,
    now: chrono::DateTime<Local>,
) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();

    let mut words = widget::column::with_capacity(2).spacing(spacing.space_xxs);
    words = words.push(text::body(task.title.as_str()));
    if let Some(due) = task.due_at() {
        let label = schedule::due_label(due, now);
        words = words.push(match section {
            Section::Overdue => text::caption(label).class(cosmic::theme::Text::Color(
                cosmic::theme::active().cosmic().destructive_color().into(),
            )),
            _ => text::caption(label),
        });
    }

    let mut row = widget::row::with_capacity(3)
        .align_y(Alignment::Center)
        .spacing(spacing.space_xs);
    if section == Section::Deleted {
        row = row
            .push(button::text("Restore").on_press(Message::Restore(task.id)))
            .push(
                button::custom(words)
                    .on_press(Message::Open(task.id))
                    .width(Length::Fill),
            );
    } else {
        let done = task.status != "open";
        row = row
            .push(
                widget::checkbox(done).on_toggle(move |checked| Message::SetDone(task.id, checked)),
            )
            .push(
                button::custom(words)
                    .on_press(Message::Open(task.id))
                    .width(Length::Fill),
            );
    }

    container(row)
        .padding(spacing.space_xxs)
        .width(Length::Fill)
        .class(cosmic::theme::Container::Card)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_expires_after_the_window() {
        let task = TaskDto {
            id: 1,
            title: "t".into(),
            notes: String::new(),
            due_at: String::new(),
            status: "open".into(),
            dismiss_reason: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            completed_at: String::new(),
            deleted_at: String::new(),
        };
        let start = Instant::now();
        let undo = Undo::new(task, start);
        assert!(!undo.expired(start));
        assert!(!undo.expired(start + UNDO_WINDOW - Duration::from_millis(1)));
        assert!(undo.expired(start + UNDO_WINDOW));
    }

    #[test]
    fn editor_loads_local_date_and_time() {
        let task = TaskDto {
            id: 4,
            title: "Renew the passport".into(),
            notes: "Form DS-82".into(),
            due_at: "2026-11-01T14:00:00.000Z".into(),
            status: "open".into(),
            dismiss_reason: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            completed_at: String::new(),
            deleted_at: String::new(),
        };
        let editor = Editor::for_task(&task);
        assert_eq!(editor.id, 4);
        assert_eq!(editor.title, "Renew the passport");
        assert_eq!(editor.notes, "Form DS-82");
        assert_eq!(
            editor.due,
            DueInput::from_rfc3339("2026-11-01T14:00:00.000Z")
        );
        assert_eq!(editor.problem, None);
    }
}
