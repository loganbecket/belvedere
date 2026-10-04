//! The window's live link to the background service. The loop itself is
//! shared with the applet and lives in `belvedere_core::watch`.

pub use belvedere_core::watch::{Event, Lists};
use cosmic::iced::{stream, Subscription};

pub fn subscription() -> Subscription<Event> {
    Subscription::run(|| stream::channel(16, belvedere_core::watch::watch))
}
