//! The Belvedere panel applet: a bow tie in the panel with a count of
//! what's due today or overdue, and a popup to tick things off or open
//! the window.

use belvedere_core::ipc::{ServiceProxy, TaskDto};
use belvedere_core::schedule::{self, Section};
use belvedere_core::watch::{self, Event, Lists};
use chrono::Local;
use cosmic::app::{Core, Task};
use cosmic::iced::platform_specific::shell::commands::popup::{destroy_popup, get_popup};
use cosmic::iced::window::Id;
use cosmic::iced::{stream, Alignment, Length, Limits, Subscription};
use cosmic::widget::{self, button, text};
use cosmic::{Application, Element};

fn main() -> cosmic::iced::Result {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("{}", belvedere_core::version_line("belvedere-applet"));
        return Ok(());
    }
    cosmic::applet::run::<Applet>(())
}

struct Applet {
    core: Core,
    popup: Option<Id>,
    service: Option<ServiceProxy<'static>>,
    lists: Lists,
    connected: bool,
}

#[derive(Debug, Clone)]
enum Message {
    TogglePopup,
    PopupClosed(Id),
    Service(Event),
    SetDone(i64, bool),
    OpenWindow,
    Done(Result<(), String>),
}

impl Application for Applet {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = "org.belvedere.Applet";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: ()) -> (Self, Task<Message>) {
        (
            Applet {
                core,
                popup: None,
                service: None,
                lists: Lists::default(),
                connected: false,
            },
            Task::none(),
        )
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::run(|| stream::channel(16, watch::watch)).map(Message::Service)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::TogglePopup => {
                if let Some(id) = self.popup.take() {
                    return destroy_popup(id);
                }
                let Some(parent) = self.core.main_window_id() else {
                    return Task::none();
                };
                let id = Id::unique();
                self.popup = Some(id);
                let mut settings = self
                    .core
                    .applet
                    .get_popup_settings(parent, id, None, None, None);
                settings.positioner.size_limits = Limits::NONE
                    .min_width(300.0)
                    .max_width(420.0)
                    .min_height(80.0)
                    .max_height(600.0);
                return get_popup(settings);
            }
            Message::PopupClosed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                }
            }
            Message::Service(event) => match event {
                Event::Connected(proxy, lists) => {
                    self.connected = true;
                    self.service = Some(proxy);
                    self.lists = lists;
                }
                Event::Tasks(lists) => self.lists = lists,
                Event::ShowTask(_)
                | Event::ChatStatus { .. }
                | Event::ChatText { .. }
                | Event::ChatDone { .. }
                | Event::ChatFailed { .. } => {}
                Event::Disconnected => {
                    self.connected = false;
                    self.service = None;
                }
            },
            Message::SetDone(id, done) => {
                let Some(proxy) = self.service.clone() else {
                    return Task::none();
                };
                return Task::perform(
                    async move {
                        if done {
                            proxy.complete_task(id).await.map(|_| ())
                        } else {
                            proxy.reopen_task(id).await.map(|_| ())
                        }
                    },
                    |r| cosmic::Action::App(Message::Done(r.map_err(|e| e.to_string()))),
                );
            }
            Message::OpenWindow => {
                open_window();
                if let Some(id) = self.popup.take() {
                    return destroy_popup(id);
                }
            }
            Message::Done(Err(problem)) => tracing_lite(&problem),
            Message::Done(Ok(())) => {}
        }
        Task::none()
    }

    /// The panel button: bow tie, plus a count when anything is due.
    fn view(&self) -> Element<'_, Message> {
        let due = self.due_now().len();
        if due == 0 {
            return self
                .core
                .applet
                .icon_button("org.belvedere.Belvedere-symbolic")
                .on_press(Message::TogglePopup)
                .into();
        }
        let size = self.core.applet.suggested_size(true).0;
        let icon = widget::icon::from_name("org.belvedere.Belvedere-symbolic")
            .symbolic(true)
            .size(size);
        let content = widget::row::with_capacity(2)
            .align_y(Alignment::Center)
            .spacing(4)
            .push(widget::icon(icon.into()))
            .push(self.core.applet.text(due.to_string()));
        self.core
            .applet
            .button_from_element(content, true)
            .on_press(Message::TogglePopup)
            .into()
    }

    /// The popup.
    fn view_window(&self, _id: Id) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let now = Local::now();
        let due = self.due_now();

        let mut list = widget::column::with_capacity(due.len() + 2).spacing(spacing.space_xxs);
        if !self.connected {
            list = list.push(text::body("Belvedere's service isn't running."));
        } else if due.is_empty() {
            list = list.push(text::body("Nothing due today."));
        }
        for task in due {
            let section = schedule::section_of(task, now);
            let mut words = widget::column::with_capacity(2);
            words = words.push(text::body(task.title.as_str()));
            if let Some(d) = task.due_at() {
                let label = schedule::due_label(d, now);
                words = words.push(if section == Section::Overdue {
                    text::caption(label).class(cosmic::theme::Text::Color(
                        cosmic::theme::active().cosmic().destructive_color().into(),
                    ))
                } else {
                    text::caption(label)
                });
            }
            let id = task.id;
            list = list.push(
                widget::row::with_capacity(2)
                    .align_y(Alignment::Center)
                    .spacing(spacing.space_xs)
                    .push(
                        widget::checkbox(false)
                            .on_toggle(move |checked| Message::SetDone(id, checked)),
                    )
                    .push(words),
            );
        }

        let content = widget::column::with_capacity(3)
            .spacing(spacing.space_s)
            .padding(spacing.space_s)
            .push(text::title4("Due today"))
            .push(list)
            .push(
                button::suggested("Open Belvedere")
                    .on_press(Message::OpenWindow)
                    .width(Length::Fill),
            );

        self.core.applet.popup_container(content).into()
    }
}

impl Applet {
    /// Open tasks that are overdue or due today, overdue first.
    fn due_now(&self) -> Vec<&TaskDto> {
        let now = Local::now();
        let mut due: Vec<(Section, &TaskDto)> = self
            .lists
            .tasks
            .iter()
            .map(|t| (schedule::section_of(t, now), t))
            .filter(|(s, _)| matches!(s, Section::Overdue | Section::Today))
            .collect();
        due.sort_by_key(|(s, _)| *s != Section::Overdue);
        due.into_iter().map(|(_, t)| t).collect()
    }
}

/// Starts the window, or focuses it if it's already open (the window is
/// single-instance). Looks on PATH first, then where `just install` puts it.
fn open_window() {
    if std::process::Command::new("belvedere").spawn().is_ok() {
        return;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let local = std::path::PathBuf::from(home).join(".local/bin/belvedere");
        let _ = std::process::Command::new(local).spawn();
    }
}

/// No logging framework in the applet yet; stderr goes to the panel's journal.
fn tracing_lite(problem: &str) {
    eprintln!("belvedere-applet: {problem}");
}
