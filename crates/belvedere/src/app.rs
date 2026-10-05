//! The window itself.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use belvedere_core::ipc::{ConversationDto, MessageDto, RuleDto, ServiceProxy, TaskDto};
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
    /// Conversations, most recent first, and the one being viewed.
    conversations: Vec<ConversationDto>,
    current: Option<i64>,
    /// Messages of the current conversation.
    messages: Vec<MessageDto>,
    /// A reply being streamed right now.
    reply: Option<Reply>,
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
    /// The Rules form: a new rule being typed, and any rule being edited.
    new_rule: String,
    rule_edit: Option<(i64, String)>,
    /// A rule delete waiting for a yes.
    confirm_delete_rule: Option<RuleDto>,
}

/// A reply in progress.
#[derive(Debug, Clone)]
pub struct Reply {
    pub request: u64,
    pub conversation_id: i64,
    /// What has arrived so far.
    pub text: String,
    /// "loading model", "thinking", "writing".
    pub status: String,
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
    Stop,
    NewConversation,
    OpenConversation(i64),
    /// Fresh lists from the service after a chat action.
    ChatLoaded(Vec<ConversationDto>, Option<i64>, Vec<MessageDto>),
    // Service
    Service(service::Event),
    /// A call to the service finished; `Err` holds a message to show.
    Done(Result<(), String>),
    // Tasks
    NewTitle(String),
    CreateTask,
    SetDone(i64, bool),
    /// Close a task as not needed.
    Dismiss(i64),
    // Rules
    NewRule(String),
    AddRule,
    EditRule(i64, String),
    SaveRule,
    CancelRuleEdit,
    ToggleRule(i64, bool),
    AskDeleteRule(i64),
    ConfirmDeleteRule,
    CancelDeleteRule,
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
    AcceptSuggestion(i64),
    RejectSuggestion(i64),
    OpenEmail(i64),
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
            conversations: Vec::new(),
            current: None,
            messages: Vec::new(),
            reply: None,
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
            new_rule: String::new(),
            rule_edit: None,
            confirm_delete_rule: None,
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
                if line.is_empty() || self.reply.is_some() {
                    return Task::none();
                }
                self.draft.clear();
                let Some(proxy) = self.service.clone() else {
                    self.error = Some("The background service isn't running.".into());
                    return Task::none();
                };
                let current = self.current;
                return Task::perform(
                    async move {
                        let conversation = match current {
                            Some(id) => id,
                            None => proxy.new_conversation().await?.id,
                        };
                        proxy.send_message(conversation, &line).await?;
                        Ok::<_, zbus::Error>(conversation)
                    },
                    |result| match result {
                        Ok(_) => cosmic::Action::App(Message::Done(Ok(()))),
                        Err(e) => cosmic::Action::App(Message::Done(Err(e.to_string()))),
                    },
                )
                .chain(self.reload_chat());
            }
            Message::Stop => {
                if let (Some(reply), Some(proxy)) = (&self.reply, self.service.clone()) {
                    let request = reply.request;
                    return Task::perform(
                        async move { proxy.stop_generation(request).await },
                        |r| cosmic::Action::App(Message::Done(r.map_err(|e| e.to_string()))),
                    );
                }
            }
            Message::NewConversation => {
                if self.reply.is_none() {
                    self.current = None;
                    self.messages.clear();
                }
            }
            Message::OpenConversation(id) => {
                if self.reply.is_none() {
                    self.current = Some(id);
                    return self.reload_chat();
                }
            }
            Message::ChatLoaded(conversations, current, messages) => {
                self.conversations = conversations;
                if let Some(id) = current {
                    self.current = Some(id);
                }
                self.messages = messages;
            }

            Message::Service(event) => {
                match event {
                    service::Event::Connected(proxy, lists) => {
                        self.connected = true;
                        self.service = Some(proxy);
                        self.lists = lists;
                        return self.reload_chat();
                    }
                    service::Event::ChatStatus {
                        request,
                        conversation_id,
                        status,
                    } => {
                        if self.current.is_none() {
                            self.current = Some(conversation_id);
                        }
                        match &mut self.reply {
                            Some(r) if r.request == request => r.status = status,
                            _ => {
                                self.reply = Some(Reply {
                                    request,
                                    conversation_id,
                                    text: String::new(),
                                    status,
                                })
                            }
                        }
                    }
                    service::Event::ChatText {
                        request,
                        conversation_id,
                        text,
                    } => {
                        if let Some(r) = self.reply.as_mut().filter(|r| {
                            r.request == request && r.conversation_id == conversation_id
                        }) {
                            r.text.push_str(&text);
                            r.status = "writing".into();
                        }
                    }
                    service::Event::ChatDone { request, .. } => {
                        if self.reply.as_ref().is_some_and(|r| r.request == request) {
                            self.reply = None;
                            return self.reload_chat();
                        }
                    }
                    service::Event::ChatFailed {
                        request, message, ..
                    } => {
                        if self.reply.as_ref().is_some_and(|r| r.request == request) {
                            self.reply = None;
                            self.error = Some(message);
                            return self.reload_chat();
                        }
                    }
                    service::Event::Tasks(lists) => self.lists = lists,
                    service::Event::ShowTask(id) => {
                        if let Some(task) = self.find(id) {
                            self.editor = Some(Editor::for_task(task));
                        }
                    }
                    service::Event::Disconnected => {
                        self.connected = false;
                        self.service = None;
                    }
                }
            }
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
            Message::NewRule(v) => self.new_rule = v,
            Message::AddRule => {
                let text = self.new_rule.trim().to_string();
                if text.is_empty() {
                    return Task::none();
                }
                self.new_rule.clear();
                return self.call(move |p| async move { p.create_rule(&text).await.map(|_| ()) });
            }
            Message::EditRule(id, text) => self.rule_edit = Some((id, text)),
            Message::CancelRuleEdit => self.rule_edit = None,
            Message::SaveRule => {
                let Some((id, text)) = self.rule_edit.take() else {
                    return Task::none();
                };
                let enabled = self
                    .lists
                    .rules
                    .iter()
                    .find(|r| r.id == id)
                    .is_none_or(|r| r.enabled);
                let text = text.trim().to_string();
                if text.is_empty() {
                    return Task::none();
                }
                return self.call(move |p| async move {
                    p.update_rule(id, &text, enabled).await.map(|_| ())
                });
            }
            Message::ToggleRule(id, enabled) => {
                let Some(rule) = self.lists.rules.iter().find(|r| r.id == id).cloned() else {
                    return Task::none();
                };
                return self.call(move |p| async move {
                    p.update_rule(id, &rule.text, enabled).await.map(|_| ())
                });
            }
            Message::AskDeleteRule(id) => {
                self.confirm_delete_rule = self.lists.rules.iter().find(|r| r.id == id).cloned();
            }
            Message::CancelDeleteRule => self.confirm_delete_rule = None,
            Message::ConfirmDeleteRule => {
                let Some(rule) = self.confirm_delete_rule.take() else {
                    return Task::none();
                };
                return self.call(move |p| async move { p.delete_rule(rule.id).await.map(|_| ()) });
            }
            Message::Dismiss(id) => {
                self.editor = None;
                return self.call(move |p| async move {
                    p.dismiss_task(id, "not needed").await.map(|_| ())
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
            Message::AcceptSuggestion(id) => {
                return self
                    .call(move |p| async move { p.accept_suggestion(id).await.map(|_| ()) });
            }
            Message::RejectSuggestion(id) => {
                return self.call(move |p| async move { p.reject_suggestion(id).await });
            }
            Message::OpenEmail(id) => {
                return self.call(move |p| async move { p.open_email(id).await });
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
            page = page.push(notice(
                "Belvedere's background service isn't running. Tasks will appear once it's back.",
            ));
        }
        if let Some(error) = &self.error {
            page = page.push(notice(error.as_str()));
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

    /// Fetches conversations and the current conversation's messages.
    fn reload_chat(&self) -> Task<Message> {
        let Some(proxy) = self.service.clone() else {
            return Task::none();
        };
        let current = self.current;
        Task::perform(
            async move {
                let conversations = proxy.list_conversations().await?;
                let current = current.or_else(|| conversations.first().map(|c| c.id));
                let messages = match current {
                    Some(id) => proxy.get_messages(id).await?,
                    None => Vec::new(),
                };
                Ok::<_, zbus::Error>((conversations, current, messages))
            },
            |result| match result {
                Ok((c, cur, m)) => cosmic::Action::App(Message::ChatLoaded(c, cur, m)),
                Err(e) => cosmic::Action::App(Message::Done(Err(e.to_string()))),
            },
        )
    }

    fn chat_pane(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let busy = self.reply.is_some();

        // Conversation list down the left of the pane.
        let mut list = widget::column::with_capacity(self.conversations.len() + 1)
            .spacing(spacing.space_xxs)
            .push(button::standard("New conversation").on_press_maybe(
                (!busy && self.current.is_some()).then_some(Message::NewConversation),
            ));
        for c in &self.conversations {
            let title = if c.title.is_empty() {
                "Untitled"
            } else {
                c.title.as_str()
            };
            let mut b = button::text(title).width(Length::Fill);
            if !busy {
                b = b.on_press(Message::OpenConversation(c.id));
            }
            list = list.push(b);
        }

        // The transcript.
        let mut lines =
            widget::column::with_capacity(self.messages.len() + 2).spacing(spacing.space_xs);
        if self.messages.is_empty() && self.reply.is_none() {
            lines = lines.push(text::body("Say something to Belvedere."));
        }
        for m in &self.messages {
            if m.role == "user" || m.role == "assistant" {
                lines = lines.push(bubble(&m.content, m.role == "user"));
            }
        }
        if let Some(reply) = &self.reply {
            if reply.text.is_empty() {
                let label = match reply.status.as_str() {
                    "loading model" => "Belvedere is loading a model…",
                    "thinking" => "Belvedere is thinking…",
                    _ => "Belvedere is writing…",
                };
                lines = lines.push(text::caption(label));
            } else {
                lines = lines.push(bubble(&reply.text, false));
            }
        }

        let input = widget::text_input("Ask Belvedere…", &self.draft)
            .on_input(Message::Draft)
            .on_submit(|_| Message::Send);
        let mut controls = widget::row::with_capacity(2)
            .spacing(spacing.space_xs)
            .push(input);
        if busy {
            controls = controls.push(button::destructive("Stop").on_press(Message::Stop));
        }

        let transcript = widget::column::with_capacity(3)
            .spacing(spacing.space_s)
            .push(text::title4("Chat"))
            .push(widget::scrollable(lines).height(Length::Fill))
            .push(controls)
            .width(Length::Fill);

        widget::row::with_capacity(2)
            .spacing(spacing.space_s)
            .push(widget::scrollable(list).width(Length::Fixed(200.0)))
            .push(transcript)
            .width(Length::FillPortion(3))
            .into()
    }

    fn task_pane(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        let body: Element<'_, Message> = match &self.editor {
            Some(editor) => self.editor_view(editor),
            None => self.list_view(),
        };

        widget::column::with_capacity(4)
            .spacing(spacing.space_s)
            .push(
                widget::row::with_capacity(3)
                    .align_y(Alignment::Center)
                    .push(text::title4("Tasks"))
                    .push(cosmic::iced::widget::space().width(Length::Fill))
                    .push(text::caption(format!("{}", self.lists.tasks.len()))),
            )
            .push(body)
            .push(self.rules_view())
            .width(Length::FillPortion(2))
            .into()
    }

    /// The standing rules: add one, edit its wording, switch it off, delete.
    fn rules_view(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let mut col = widget::column::with_capacity(self.lists.rules.len() + 3)
            .spacing(spacing.space_xxs)
            .push(text::title4("Rules"))
            .push(
                widget::text_input(
                    "New rule, in your words: \"The car insurance is on autopay\"",
                    &self.new_rule,
                )
                .on_input(Message::NewRule)
                .on_submit(|_| Message::AddRule),
            );
        if let Some(rule) = &self.confirm_delete_rule {
            col = col.push(
                container(
                    widget::row::with_capacity(4)
                        .align_y(Alignment::Center)
                        .spacing(spacing.space_s)
                        .push(text::body(format!("Delete the rule \"{}\"?", rule.text)))
                        .push(cosmic::iced::widget::space().width(Length::Fill))
                        .push(button::destructive("Delete").on_press(Message::ConfirmDeleteRule))
                        .push(button::standard("Keep").on_press(Message::CancelDeleteRule)),
                )
                .padding(spacing.space_xs)
                .width(Length::Fill)
                .class(cosmic::theme::Container::Card),
            );
        }
        for r in &self.lists.rules {
            let id = r.id;
            let row: Element<'_, Message> = match &self.rule_edit {
                Some((edit_id, text)) if *edit_id == id => widget::row::with_capacity(3)
                    .align_y(Alignment::Center)
                    .spacing(spacing.space_xs)
                    .push(
                        widget::text_input("Rule", text)
                            .on_input(move |v| Message::EditRule(id, v))
                            .on_submit(|_| Message::SaveRule),
                    )
                    .push(button::suggested("Save").on_press(Message::SaveRule))
                    .push(button::standard("Cancel").on_press(Message::CancelRuleEdit)),
                _ => widget::row::with_capacity(4)
                    .align_y(Alignment::Center)
                    .spacing(spacing.space_xs)
                    .push(
                        widget::checkbox(r.enabled)
                            .on_toggle(move |on| Message::ToggleRule(id, on)),
                    )
                    .push(
                        button::custom(text::body(&r.text))
                            .on_press(Message::EditRule(id, r.text.clone()))
                            .width(Length::Fill),
                    )
                    .push(button::text("Delete").on_press(Message::AskDeleteRule(id))),
            }
            .into();
            col = col.push(
                container(row)
                    .padding(spacing.space_xxs)
                    .width(Length::Fill)
                    .class(cosmic::theme::Container::Card),
            );
        }
        if self.lists.rules.is_empty() {
            col = col.push(text::caption(
                "No rules yet. A rule tells Belvedere how to treat certain mail from now on.",
            ));
        }
        col.into()
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
        if !self.lists.suggestions.is_empty() {
            any = true;
            list = list.push(text::body(format!(
                "Suggested  ·  {}",
                self.lists.suggestions.len()
            )));
            for s in &self.lists.suggestions {
                list = list.push(suggestion_row(s, now));
            }
        }
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
        // What has happened since: the update lines a follow-up email adds.
        let history: Vec<&str> = editor
            .notes
            .lines()
            .filter(|l| l.starts_with("Update ") || l.starts_with("Amount now: "))
            .collect();
        if !history.is_empty() {
            let mut block = widget::column::with_capacity(history.len() + 1)
                .spacing(spacing.space_xxs)
                .push(widget::text::caption_heading("History"));
            for line in history {
                block = block.push(widget::text::caption(line));
            }
            form = form.push(block);
        }
        if let Some(problem) = &editor.problem {
            form = form.push(notice(problem.as_str()));
        }

        let is_done = self.find(editor.id).is_some_and(|t| t.status != "open");
        let is_deleted = self.find(editor.id).is_some_and(|t| t.is_deleted());

        let mut actions = widget::row::with_capacity(5)
            .spacing(spacing.space_s)
            .push(button::suggested("Save").on_press(Message::SaveEdit))
            .push(button::standard("Cancel").on_press(Message::CloseEditor))
            .push(cosmic::iced::widget::space().width(Length::Fill));
        if self
            .find(editor.id)
            .is_some_and(|t| t.source_kind == "email")
        {
            actions = actions
                .push(button::standard("Open email").on_press(Message::OpenEmail(editor.id)));
        }
        if is_deleted {
            actions =
                actions.push(button::standard("Restore").on_press(Message::Restore(editor.id)));
        } else {
            actions = actions.push(
                button::standard(if is_done { "Reopen" } else { "Mark done" })
                    .on_press(Message::SetDone(editor.id, !is_done)),
            );
            if !is_done {
                actions = actions
                    .push(button::standard("Not needed").on_press(Message::Dismiss(editor.id)));
            }
            actions =
                actions.push(button::destructive("Delete").on_press(Message::AskDelete(editor.id)));
        }
        form = form.push(actions);

        container(form)
            .padding(spacing.space_s)
            .width(Length::Fill)
            .class(cosmic::theme::Container::Card)
            .into()
    }
}

/// A warning bar that stays readable in light and dark themes: a soft
/// tint of the theme's warning color behind the theme's normal text, with
/// a solid warning-colored edge on the left.
fn notice<'a>(message: &'a str) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();
    container(text::body(message))
        .padding([spacing.space_xs, spacing.space_s])
        .width(Length::Fill)
        .class(cosmic::theme::Container::custom(|theme| {
            let cosmic = theme.cosmic();
            let warning = cosmic.warning_color();
            let mut tint = warning;
            tint.alpha = 0.18;
            cosmic::iced::widget::container::Style {
                icon_color: None,
                text_color: Some(cosmic.on_bg_color().into()),
                background: Some(cosmic::iced::Color::from(tint).into()),
                border: cosmic::iced::Border {
                    color: warning.into(),
                    width: 0.0,
                    radius: cosmic.corner_radii.radius_s.into(),
                },
                shadow: Default::default(),
                snap: false,
            }
        }))
        .into()
}

/// One chat message: the user's on the right in the accent color, Belvedere's
/// on the left.
fn bubble(content: &str, from_user: bool) -> Element<'_, Message> {
    let spacing = cosmic::theme::spacing();
    let card = container(text::body(content))
        .padding(spacing.space_xs)
        .max_width(560.0)
        .class(if from_user {
            cosmic::theme::Container::Primary
        } else {
            cosmic::theme::Container::Card
        });
    let mut row = widget::row::with_capacity(2).width(Length::Fill);
    if from_user {
        row = row.push(cosmic::iced::widget::space().width(Length::Fill));
    }
    row.push(card).into()
}

/// A suggested task: what Belvedere thinks you might need to do, with the
/// email it came from and buttons to accept or reject it.
fn suggestion_row<'a>(
    s: &'a belvedere_core::ipc::SuggestionDto,
    now: chrono::DateTime<Local>,
) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();
    let mut words = widget::column::with_capacity(3).spacing(spacing.space_xxs);
    words = words.push(text::body(s.title.as_str()));
    let mut detail = String::new();
    if !s.due_at.is_empty() {
        detail.push_str(&format!("Due {}", schedule::due_label(&s.due_at, now)));
    }
    if s.amount > 0.0 {
        if !detail.is_empty() {
            detail.push_str("  ·  ");
        }
        detail.push_str(&format!("${:.2}", s.amount));
    }
    if !detail.is_empty() {
        words = words.push(text::caption(detail));
    }
    if !s.source_label.is_empty() {
        words = words.push(text::caption(format!("From email: {}", s.source_label)));
    }
    let buttons = widget::row::with_capacity(2)
        .spacing(spacing.space_xxs)
        .push(button::suggested("Accept").on_press(Message::AcceptSuggestion(s.id)))
        .push(button::standard("Reject").on_press(Message::RejectSuggestion(s.id)));
    container(
        widget::row::with_capacity(2)
            .align_y(Alignment::Center)
            .spacing(spacing.space_xs)
            .push(words.width(Length::Fill))
            .push(buttons),
    )
    .padding(spacing.space_xs)
    .width(Length::Fill)
    .class(cosmic::theme::Container::Card)
    .into()
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
            source_kind: String::new(),
            source_label: String::new(),
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
            source_kind: String::new(),
            source_label: String::new(),
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
