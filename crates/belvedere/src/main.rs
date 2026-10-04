//! The Belvedere window: chat on the left, tasks on the right.

mod app;
mod service;

use cosmic::iced::Limits;

fn main() -> cosmic::iced::Result {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("{}", belvedere_core::version_line("belvedere"));
        return Ok(());
    }

    let settings = cosmic::app::Settings::default()
        .size_limits(Limits::NONE.min_width(640.0).min_height(400.0))
        .size(cosmic::iced::Size::new(960.0, 640.0));

    cosmic::app::run::<app::Belvedere>(settings, ())
}
