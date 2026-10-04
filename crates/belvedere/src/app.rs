//! The window itself.

use belvedere_core::ipc::TaskDto;
use cosmic::app::{Core, Task};
use cosmic::iced::{Alignment, Length, Subscription};
use cosmic::widget::{self, container, text};
use cosmic::{Application, ApplicationExt, Element};

use crate::service;

pub struct Belvedere {
    core: Core,
    /// What's typed in the chat box but not yet sent.
    draft: String,
    /// Lines sent so far. Nothing answers yet; that arrives with the model.
    transcript: Vec<String>,
    tasks: Vec<TaskDto>,
    /// Whether the background service is answering.
    connected: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    Draft(String),
    Send,
    Service(service::Event),
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
            tasks: Vec::new(),
            connected: false,
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
                service::Event::Connected(tasks) => {
                    self.connected = true;
                    self.tasks = tasks;
                }
                service::Event::Tasks(tasks) => self.tasks = tasks,
                service::Event::Disconnected => self.connected = false,
            },
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        service::subscription().map(Message::Service)
    }

    fn view(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        let panes = widget::row::with_capacity(3)
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
        page = page.push(panes);

        container(page)
            .padding(spacing.space_s)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

impl Belvedere {
    fn chat_pane(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        let mut lines = widget::column::with_capacity(4).spacing(spacing.space_xs);
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

        widget::column::with_capacity(4)
            .spacing(spacing.space_s)
            .push(text::title4("Chat"))
            .push(widget::scrollable(lines).height(Length::Fill))
            .push(input)
            .width(Length::FillPortion(3))
            .into()
    }

    fn task_pane(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        let mut list = widget::column::with_capacity(4).spacing(spacing.space_xs);
        if self.tasks.is_empty() {
            let note = if self.connected {
                "No tasks yet."
            } else {
                "Waiting for the service…"
            };
            list = list.push(text::body(note));
        }
        for task in &self.tasks {
            list = list.push(task_row(task));
        }

        widget::column::with_capacity(4)
            .spacing(spacing.space_s)
            .push(
                widget::row::with_capacity(3)
                    .align_y(Alignment::Center)
                    .push(text::title4("Tasks"))
                    .push(cosmic::iced::widget::space().width(Length::Fill))
                    .push(text::caption(format!("{}", self.tasks.len()))),
            )
            .push(widget::scrollable(list).height(Length::Fill))
            .width(Length::FillPortion(2))
            .into()
    }
}

fn task_row(task: &TaskDto) -> Element<'_, Message> {
    let spacing = cosmic::theme::spacing();
    let mut column = widget::column::with_capacity(4)
        .spacing(spacing.space_xxs)
        .push(text::body(task.title.as_str()));
    if let Some(due) = task.due_at() {
        column = column.push(text::caption(format!("Due {}", due_label(due))));
    }
    container(column)
        .padding(spacing.space_xs)
        .width(Length::Fill)
        .class(cosmic::theme::Container::Card)
        .into()
}

/// "2026-10-20T00:00:00.000Z" -> "2026-10-20". Dates get friendlier words
/// in a later chunk; for now, just don't show the clock noise.
fn due_label(due: &str) -> &str {
    due.split('T').next().unwrap_or(due)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_label_drops_the_time() {
        assert_eq!(due_label("2026-10-20T00:00:00.000Z"), "2026-10-20");
        assert_eq!(due_label("2026-10-20"), "2026-10-20");
    }
}
