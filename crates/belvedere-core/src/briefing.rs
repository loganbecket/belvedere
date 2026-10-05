//! The morning briefing: what is overdue, what is due today, today's
//! events, and what is coming up in the next three days. Pure text built
//! from the task list and the calendar reading; no model involved, so it
//! is ready instantly.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Utc};

use crate::calendar::Event;
use crate::db::{Task, TaskStatus};

/// Days after today that count as "coming up".
pub const AHEAD_DAYS: i64 = 3;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Briefing {
    pub day: NaiveDate,
    pub overdue: Vec<Task>,
    pub due_today: Vec<Task>,
    pub events_today: Vec<Event>,
    /// Tasks due tomorrow through three days out.
    pub upcoming_tasks: Vec<Task>,
    /// Events tomorrow through three days out.
    pub upcoming_events: Vec<Event>,
}

fn local_day(rfc3339: &str) -> Option<NaiveDate> {
    DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|d| d.with_timezone(&Local).date_naive())
}

/// Sorts the day's tasks and events into the briefing.
pub fn compose(now: DateTime<Local>, tasks: &[Task], events: &[Event]) -> Briefing {
    let today = now.date_naive();
    let horizon = today + Duration::days(AHEAD_DAYS);
    let mut b = Briefing {
        day: today,
        ..Default::default()
    };
    let mut open: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Open && t.deleted_at.is_none())
        .collect();
    open.sort_by(|a, b| a.due_at.cmp(&b.due_at).then(a.id.cmp(&b.id)));
    for t in open {
        let Some(day) = t.due_at.as_deref().and_then(local_day) else {
            continue;
        };
        if day < today {
            b.overdue.push(t.clone());
        } else if day == today {
            b.due_today.push(t.clone());
        } else if day <= horizon {
            b.upcoming_tasks.push(t.clone());
        }
    }
    let mut sorted: Vec<&Event> = events.iter().collect();
    sorted.sort_by(|a, b| a.start.cmp(&b.start).then(a.title.cmp(&b.title)));
    for e in sorted {
        let start_day = e.start.with_timezone(&Local).date_naive();
        // An event's last day is the day before its end for all-day
        // events (the end is the next midnight).
        let end_day = if e.all_day {
            (e.end - Duration::seconds(1))
                .with_timezone(&Local)
                .date_naive()
        } else {
            e.end.with_timezone(&Local).date_naive()
        };
        if start_day <= today && end_day >= today && e.end > now.with_timezone(&Utc) {
            b.events_today.push(e.clone());
        } else if start_day > today && start_day <= horizon {
            b.upcoming_events.push(e.clone());
        }
    }
    b
}

impl Briefing {
    /// The one-line summary for the notification: "3 events, 2 tasks due,
    /// 1 overdue", or "Nothing due today" when the day is clear.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        let plural =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        if !self.events_today.is_empty() {
            parts.push(plural(self.events_today.len(), "event", "events"));
        }
        if !self.due_today.is_empty() {
            parts.push(format!(
                "{} due",
                plural(self.due_today.len(), "task", "tasks")
            ));
        }
        if !self.overdue.is_empty() {
            parts.push(format!("{} overdue", self.overdue.len()));
        }
        if parts.is_empty() {
            let coming = self.upcoming_tasks.len() + self.upcoming_events.len();
            if coming == 0 {
                "Nothing due today and a clear few days ahead.".to_string()
            } else {
                format!("Nothing due today; {coming} coming up in the next {AHEAD_DAYS} days.")
            }
        } else {
            parts.join(", ")
        }
    }

    /// The full briefing, in the order overdue, due today, today's
    /// events, coming up.
    pub fn text(&self) -> String {
        let mut out = format!("Good morning. Here is {}.\n", self.day.format("%A, %B %-d"));
        let when = |t: &Task| -> String {
            t.due_at
                .as_deref()
                .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
                .map(|d| {
                    let l = d.with_timezone(&Local);
                    if l.date_naive() == self.day {
                        if l.time() == crate::schedule::DEFAULT_DUE_TIME {
                            "today".to_string()
                        } else {
                            l.format("%-I:%M %p").to_string()
                        }
                    } else {
                        l.format("%a %b %-d").to_string()
                    }
                })
                .unwrap_or_default()
        };
        if !self.overdue.is_empty() {
            out.push_str("\nOverdue:\n");
            for t in &self.overdue {
                out.push_str(&format!("  • {} (was due {})\n", t.title, when(t)));
            }
        }
        if !self.due_today.is_empty() {
            out.push_str("\nDue today:\n");
            for t in &self.due_today {
                let w = when(t);
                if w == "today" {
                    out.push_str(&format!("  • {}\n", t.title));
                } else {
                    out.push_str(&format!("  • {} ({})\n", t.title, w));
                }
            }
        }
        if !self.events_today.is_empty() {
            out.push_str("\nToday's events:\n");
            for e in &self.events_today {
                out.push_str(&format!("  • {}\n", event_line(e, self.day)));
            }
        }
        if !self.upcoming_tasks.is_empty() || !self.upcoming_events.is_empty() {
            out.push_str(&format!("\nComing up (next {AHEAD_DAYS} days):\n"));
            let mut lines: Vec<(DateTime<Utc>, String)> = Vec::new();
            for t in &self.upcoming_tasks {
                if let Some(d) = t
                    .due_at
                    .as_deref()
                    .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
                {
                    lines.push((
                        d.with_timezone(&Utc),
                        format!("{} (due {})", t.title, when(t)),
                    ));
                }
            }
            for e in &self.upcoming_events {
                lines.push((e.start, event_line(e, self.day)));
            }
            lines.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, l) in lines {
                out.push_str(&format!("  • {l}\n"));
            }
        }
        if self.overdue.is_empty()
            && self.due_today.is_empty()
            && self.events_today.is_empty()
            && self.upcoming_tasks.is_empty()
            && self.upcoming_events.is_empty()
        {
            out.push_str("\nNothing on the list and nothing on the calendar. Enjoy the quiet.\n");
        }
        out.trim_end().to_string()
    }
}

fn event_line(e: &Event, today: NaiveDate) -> String {
    let start = e.start.with_timezone(&Local);
    let day = if start.date_naive() == today {
        String::new()
    } else {
        format!("{} ", start.format("%a %b %-d"))
    };
    let time = if e.all_day {
        "all day".to_string()
    } else {
        let end = e.end.with_timezone(&Local);
        format!("{}–{}", start.format("%-I:%M %p"), end.format("%-I:%M %p"))
    };
    let place = if e.location.is_empty() {
        String::new()
    } else {
        format!(", {}", e.location)
    };
    let _ = today.year();
    format!("{day}{time}: {}{place}", e.title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn task(id: i64, title: &str, due_local: Option<(i32, u32, u32, u32, u32)>) -> Task {
        Task {
            id,
            title: title.into(),
            notes: String::new(),
            due_at: due_local.map(|(y, m, d, h, mi)| {
                Local
                    .with_ymd_and_hms(y, m, d, h, mi, 0)
                    .unwrap()
                    .with_timezone(&Utc)
                    .to_rfc3339()
            }),
            status: TaskStatus::Open,
            dismiss_reason: None,
            created_at: String::new(),
            updated_at: String::new(),
            completed_at: None,
            deleted_at: None,
            kind: String::new(),
            reference: String::new(),
        }
    }

    fn event(title: &str, y: i32, m: u32, d: u32, h: u32, all_day: bool) -> Event {
        let start = Local
            .with_ymd_and_hms(y, m, d, h, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        Event {
            calendar_id: "home".into(),
            uid: title.to_lowercase(),
            title: title.into(),
            start,
            end: start
                + if all_day {
                    Duration::days(1)
                } else {
                    Duration::hours(1)
                },
            all_day,
            location: String::new(),
            recurrence_id: None,
            alarm: None,
        }
    }

    #[test]
    fn sorts_the_day_and_lists_nothing_from_other_days_as_today() {
        let now = Local.with_ymd_and_hms(2026, 10, 20, 8, 0, 0).unwrap();
        let tasks = vec![
            task(1, "Pay the City Power bill", Some((2026, 10, 18, 9, 0))),
            task(2, "Call the dentist", Some((2026, 10, 20, 9, 0))),
            task(3, "Submit the expense report", Some((2026, 10, 20, 17, 0))),
            task(4, "Renew the permit", Some((2026, 10, 22, 9, 0))),
            task(5, "Far away", Some((2026, 11, 20, 9, 0))),
            task(6, "No date", None),
            Task {
                status: TaskStatus::Done,
                ..task(7, "Done already", Some((2026, 10, 20, 9, 0)))
            },
        ];
        let events = vec![
            event("Practice", 2026, 10, 20, 16, false),
            event("Holiday", 2026, 10, 20, 0, true),
            event("Yesterday", 2026, 10, 19, 16, false),
            event("Game", 2026, 10, 23, 11, false),
            event("Next week", 2026, 10, 27, 11, false),
            event("Early call", 2026, 10, 20, 7, false), // over before 8:00
        ];
        let b = compose(now, &tasks, &events);
        let titles = |v: &[Task]| v.iter().map(|t| t.title.clone()).collect::<Vec<_>>();
        assert_eq!(titles(&b.overdue), ["Pay the City Power bill"]);
        assert_eq!(
            titles(&b.due_today),
            ["Call the dentist", "Submit the expense report"]
        );
        assert_eq!(titles(&b.upcoming_tasks), ["Renew the permit"]);
        let names = |v: &[Event]| v.iter().map(|e| e.title.clone()).collect::<Vec<_>>();
        assert_eq!(names(&b.events_today), ["Holiday", "Practice"]);
        assert_eq!(names(&b.upcoming_events), ["Game"]);
        assert_eq!(b.summary(), "2 events, 2 tasks due, 1 overdue");
        let text = b.text();
        let order = |s: &str| {
            text.find(s)
                .unwrap_or_else(|| panic!("{s} missing in {text}"))
        };
        assert!(order("Overdue:") < order("Due today:"));
        assert!(order("Due today:") < order("Today's events:"));
        assert!(order("Today's events:") < order("Coming up"));
        assert!(text.contains("Submit the expense report (5:00 PM)"));
        assert!(text.contains("all day: Holiday"));
        assert!(
            !text.contains("Yesterday")
                && !text.contains("Next week")
                && !text.contains("Far away")
        );
    }

    #[test]
    fn a_clear_day_says_so() {
        let now = Local.with_ymd_and_hms(2026, 10, 20, 8, 0, 0).unwrap();
        let b = compose(now, &[], &[]);
        assert_eq!(b.summary(), "Nothing due today and a clear few days ahead.");
        assert!(b.text().contains("Enjoy the quiet"));
        let soon = compose(now, &[task(1, "Renew", Some((2026, 10, 21, 9, 0)))], &[]);
        assert_eq!(
            soon.summary(),
            "Nothing due today; 1 coming up in the next 3 days."
        );
    }
}
