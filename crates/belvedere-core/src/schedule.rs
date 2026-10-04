//! Where a task sits in time: which section of the list it belongs in,
//! and turning the dates people type into the timestamps we store.
//!
//! Everything here takes "now" as an argument so it can be tested at any
//! moment, including the stroke of midnight.

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};

use crate::ipc::TaskDto;

/// The groups the task list is shown in, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Overdue,
    Today,
    Upcoming,
    NoDate,
    Done,
    Deleted,
}

impl Section {
    pub const ALL: [Section; 6] = [
        Section::Overdue,
        Section::Today,
        Section::Upcoming,
        Section::NoDate,
        Section::Done,
        Section::Deleted,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Overdue => "Overdue",
            Section::Today => "Today",
            Section::Upcoming => "Upcoming",
            Section::NoDate => "No date",
            Section::Done => "Done",
            Section::Deleted => "Deleted",
        }
    }

    /// Sections that start folded shut.
    pub fn collapsed_by_default(self) -> bool {
        matches!(self, Section::Done | Section::Deleted)
    }
}

/// Decides a task's section as of `now` (local time).
///
/// Deleted and done tasks go to their own sections whatever their date.
/// Otherwise it is the *calendar day* of the due date in local time that
/// counts: a bill due today at any hour is "Today", not "Overdue", until
/// midnight passes.
pub fn section_of(task: &TaskDto, now: DateTime<Local>) -> Section {
    if task.is_deleted() {
        return Section::Deleted;
    }
    if task.status != "open" {
        return Section::Done;
    }
    let Some(due) = task.due_at().and_then(parse_rfc3339) else {
        return Section::NoDate;
    };
    let due_day = due.with_timezone(&Local).date_naive();
    let today = now.date_naive();
    if due_day < today {
        Section::Overdue
    } else if due_day == today {
        Section::Today
    } else {
        Section::Upcoming
    }
}

/// Groups tasks by section, keeping each section in the order given.
pub fn group<'a>(
    tasks: impl IntoIterator<Item = &'a TaskDto>,
    now: DateTime<Local>,
) -> Vec<(Section, Vec<&'a TaskDto>)> {
    let mut groups: Vec<(Section, Vec<&TaskDto>)> =
        Section::ALL.iter().map(|s| (*s, Vec::new())).collect();
    for task in tasks {
        let section = section_of(task, now);
        if let Some((_, list)) = groups.iter_mut().find(|(s, _)| *s == section) {
            list.push(task);
        }
    }
    groups
}

pub fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// What a person typed for a due date: a date, and optionally a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueInput {
    /// `YYYY-MM-DD`
    pub date: String,
    /// `HH:MM`, or empty for "sometime that day" (stored as 09:00 local).
    pub time: String,
}

/// The hour a date-only task is considered due.
pub const DEFAULT_DUE_TIME: NaiveTime = NaiveTime::from_hms_opt(9, 0, 0).unwrap();

impl DueInput {
    /// Converts typed local date and time into the stored RFC 3339 UTC
    /// string. `None` if the date is empty; `Err` with a plain message if
    /// it doesn't parse.
    pub fn to_rfc3339(&self) -> Result<Option<String>, String> {
        let date = self.date.trim();
        if date.is_empty() {
            return Ok(None);
        }
        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .map_err(|_| "Date must look like 2026-10-20".to_string())?;
        let time = self.time.trim();
        let time = if time.is_empty() {
            DEFAULT_DUE_TIME
        } else {
            NaiveTime::parse_from_str(time, "%H:%M")
                .map_err(|_| "Time must look like 14:30".to_string())?
        };
        let local = Local
            .from_local_datetime(&NaiveDateTime::new(date, time))
            .earliest()
            .ok_or_else(|| "That time doesn't exist locally (daylight saving gap)".to_string())?;
        Ok(Some(
            local
                .with_timezone(&Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        ))
    }

    /// The reverse: a stored timestamp back into local date and time
    /// fields for editing.
    pub fn from_rfc3339(s: &str) -> Self {
        match parse_rfc3339(s) {
            Some(utc) => {
                let local = utc.with_timezone(&Local);
                DueInput {
                    date: local.format("%Y-%m-%d").to_string(),
                    time: local.format("%H:%M").to_string(),
                }
            }
            None => DueInput {
                date: String::new(),
                time: String::new(),
            },
        }
    }
}

/// A short human label for a due date relative to now: "Today 14:30",
/// "Tomorrow", "Mon, Oct 20", "3 days ago".
pub fn due_label(due_rfc3339: &str, now: DateTime<Local>) -> String {
    let Some(due) = parse_rfc3339(due_rfc3339) else {
        return due_rfc3339.to_string();
    };
    let due = due.with_timezone(&Local);
    let days = (due.date_naive() - now.date_naive()).num_days();
    let time = if due.time() == DEFAULT_DUE_TIME {
        String::new()
    } else {
        format!(" {}", due.format("%H:%M"))
    };
    match days {
        0 => format!("Today{time}"),
        1 => format!("Tomorrow{time}"),
        -1 => format!("Yesterday{time}"),
        d if d < -1 => format!("{} days ago", -d),
        d if d < 7 => format!("{}{time}", due.format("%A")),
        _ => format!("{}{time}", due.format("%a, %b %-d")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(due: Option<&str>, status: &str, deleted: bool) -> TaskDto {
        TaskDto {
            id: 1,
            title: "t".into(),
            notes: String::new(),
            due_at: due.unwrap_or("").into(),
            status: status.into(),
            dismiss_reason: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            completed_at: String::new(),
            deleted_at: if deleted { "x".into() } else { String::new() },
        }
    }

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, min, 0).single().unwrap()
    }

    /// A due timestamp for a local wall-clock moment.
    fn due(y: i32, m: u32, d: u32, h: u32, min: u32) -> String {
        local(y, m, d, h, min)
            .with_timezone(&Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    #[test]
    fn done_and_deleted_win_over_dates() {
        let now = local(2026, 10, 20, 12, 0);
        let overdue = due(2026, 10, 1, 9, 0);
        assert_eq!(
            section_of(&task(Some(&overdue), "done", false), now),
            Section::Done
        );
        assert_eq!(
            section_of(&task(Some(&overdue), "dismissed", false), now),
            Section::Done
        );
        assert_eq!(
            section_of(&task(Some(&overdue), "open", true), now),
            Section::Deleted
        );
        assert_eq!(section_of(&task(None, "done", true), now), Section::Deleted);
    }

    #[test]
    fn open_tasks_sort_by_calendar_day() {
        let now = local(2026, 10, 20, 12, 0);
        assert_eq!(section_of(&task(None, "open", false), now), Section::NoDate);
        assert_eq!(
            section_of(&task(Some(&due(2026, 10, 19, 23, 59)), "open", false), now),
            Section::Overdue
        );
        // Earlier today is still "today", not overdue.
        assert_eq!(
            section_of(&task(Some(&due(2026, 10, 20, 0, 1)), "open", false), now),
            Section::Today
        );
        assert_eq!(
            section_of(&task(Some(&due(2026, 10, 20, 23, 59)), "open", false), now),
            Section::Today
        );
        assert_eq!(
            section_of(&task(Some(&due(2026, 10, 21, 0, 0)), "open", false), now),
            Section::Upcoming
        );
    }

    #[test]
    fn midnight_rollover_moves_tasks_down_a_section() {
        let today = task(Some(&due(2026, 10, 20, 9, 0)), "open", false);
        let tomorrow = task(Some(&due(2026, 10, 21, 9, 0)), "open", false);

        let before = local(2026, 10, 20, 23, 59);
        assert_eq!(section_of(&today, before), Section::Today);
        assert_eq!(section_of(&tomorrow, before), Section::Upcoming);

        let after = local(2026, 10, 21, 0, 0);
        assert_eq!(section_of(&today, after), Section::Overdue);
        assert_eq!(section_of(&tomorrow, after), Section::Today);
    }

    #[test]
    fn group_keeps_section_order_and_input_order() {
        let now = local(2026, 10, 20, 12, 0);
        let a = task(Some(&due(2026, 10, 25, 9, 0)), "open", false);
        let mut b = task(None, "open", false);
        b.id = 2;
        let mut c = task(Some(&due(2026, 10, 26, 9, 0)), "open", false);
        c.id = 3;
        let tasks = [a, b, c];
        let groups = group(tasks.iter(), now);
        let sections: Vec<_> = groups.iter().map(|(s, _)| *s).collect();
        assert_eq!(sections, Section::ALL);
        let upcoming: Vec<_> = groups[2].1.iter().map(|t| t.id).collect();
        assert_eq!(upcoming, [1, 3]);
        assert_eq!(groups[3].1.len(), 1);
    }

    #[test]
    fn typed_due_round_trips() {
        let input = DueInput {
            date: "2026-10-20".into(),
            time: "14:30".into(),
        };
        let stored = input.to_rfc3339().unwrap().unwrap();
        assert_eq!(DueInput::from_rfc3339(&stored), input);

        let date_only = DueInput {
            date: "2026-10-20".into(),
            time: String::new(),
        };
        let stored = date_only.to_rfc3339().unwrap().unwrap();
        assert_eq!(
            DueInput::from_rfc3339(&stored),
            DueInput {
                date: "2026-10-20".into(),
                time: "09:00".into()
            }
        );
    }

    #[test]
    fn typed_due_validation() {
        assert_eq!(
            DueInput {
                date: "  ".into(),
                time: "14:30".into()
            }
            .to_rfc3339(),
            Ok(None)
        );
        assert!(DueInput {
            date: "10/20/2026".into(),
            time: String::new()
        }
        .to_rfc3339()
        .is_err());
        assert!(DueInput {
            date: "2026-10-20".into(),
            time: "2pm".into()
        }
        .to_rfc3339()
        .is_err());
    }

    #[test]
    fn due_labels() {
        let now = local(2026, 10, 20, 12, 0);
        assert_eq!(due_label(&due(2026, 10, 20, 9, 0), now), "Today");
        assert_eq!(due_label(&due(2026, 10, 20, 14, 30), now), "Today 14:30");
        assert_eq!(due_label(&due(2026, 10, 21, 9, 0), now), "Tomorrow");
        assert_eq!(due_label(&due(2026, 10, 19, 9, 0), now), "Yesterday");
        assert_eq!(due_label(&due(2026, 10, 17, 9, 0), now), "3 days ago");
        assert_eq!(due_label(&due(2026, 10, 23, 9, 0), now), "Friday");
        assert_eq!(due_label(&due(2026, 11, 2, 9, 0), now), "Mon, Nov 2");
        assert_eq!(due_label("garbage", now), "garbage");
    }
}
