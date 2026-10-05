//! The settings a person can change, with their defaults and checks.
//! Every one is read where it is used, so a change takes effect at once.

use serde::{Deserialize, Serialize};

/// What kind of value a setting holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// `true` or `false`.
    Bool,
    /// A whole number within a range.
    Int,
    /// `HH:MM`, 24-hour.
    Time,
    /// A number between 0 and 1.
    Fraction,
    /// Comma-separated words.
    List,
    /// Free text.
    Text,
}

/// One setting's description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    pub default: &'static str,
    /// For `Int`: the smallest and largest allowed.
    pub range: (i64, i64),
}

/// Every user-facing setting, in the order the window shows them.
pub const SPECS: &[Spec] = &[
    Spec { key: "sweep_minutes", label: "Check mail every (minutes)", help: "How often Belvedere re-checks every mail folder for anything the live watcher missed.", kind: Kind::Int, default: "5", range: (1, 1440) },
    Spec { key: "skipped_folder_roles", label: "Mail folders never read for tasks", help: "Folder roles whose mail never becomes a task: junk, trash, drafts, sent, outbox, templates, archive, inbox, other. Sent mail is still used to notice your replies.", kind: Kind::List, default: "junk,trash,drafts,sent,outbox,templates", range: (0, 0) },
    Spec { key: "extract_confidence_threshold", label: "Confidence needed to make a task", help: "Below this the model's reading becomes a suggestion for you to accept or reject instead of a task.", kind: Kind::Fraction, default: "0.75", range: (0, 0) },
    Spec { key: "briefing_enabled", label: "Morning briefing", help: "A short notification each morning with what is due and on the calendar; click it for the full rundown.", kind: Kind::Bool, default: "true", range: (0, 0) },
    Spec { key: "briefing_time", label: "Briefing time", help: "When the briefing arrives, or at first login after that time.", kind: Kind::Time, default: "08:00", range: (0, 0) },
    Spec { key: "default_due_time", label: "Default time for a dated task", help: "When a task has a day but no time, this is the time used for its reminder.", kind: Kind::Time, default: "09:00", range: (0, 0) },
    Spec { key: "reminder_lead_days", label: "Early reminder (days before)", help: "Tasks made from mail get a reminder this many days before they are due, besides the one on the day.", kind: Kind::Int, default: "3", range: (0, 30) },
    Spec { key: "event_reminder_minutes", label: "Event reminder (minutes before)", help: "How far ahead of a calendar event Belvedere reminds you.", kind: Kind::Int, default: "15", range: (0, 1440) },
    Spec { key: "all_day_reminder_time", label: "All-day event reminder time", help: "When Belvedere reminds you about an all-day event, on its day.", kind: Kind::Time, default: "09:00", range: (0, 0) },
    Spec { key: "event_reminders_despite_thunderbird", label: "Remind even when Thunderbird will", help: "Off means Belvedere stays quiet for events that already have a Thunderbird alarm.", kind: Kind::Bool, default: "false", range: (0, 0) },
    Spec { key: "model_idle_unload_minutes", label: "Unload the model after (minutes idle)", help: "The model's memory is freed after sitting unused this long; it loads again on demand.", kind: Kind::Int, default: "5", range: (1, 1440) },
    Spec { key: "file_search_excludes", label: "Extra places never opened", help: "Folders or file names (relative to your home folder) that file search and reading never enter, besides the built-in list of password and key locations.", kind: Kind::List, default: "", range: (0, 0) },
];

pub fn spec(key: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.key == key)
}

/// Checks a value for a setting and returns it tidied, or says what is
/// wrong in plain words.
pub fn validate(key: &str, value: &str) -> Result<String, String> {
    let s = spec(key).ok_or_else(|| format!("{key} is not a setting"))?;
    let v = value.trim();
    match s.kind {
        Kind::Bool => match v.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" | "1" => Ok("true".into()),
            "false" | "off" | "no" | "0" => Ok("false".into()),
            _ => Err(format!("{} must be on or off", s.label)),
        },
        Kind::Int => {
            let n: i64 = v
                .parse()
                .map_err(|_| format!("{} must be a whole number", s.label))?;
            if n < s.range.0 || n > s.range.1 {
                return Err(format!(
                    "{} must be between {} and {}",
                    s.label, s.range.0, s.range.1
                ));
            }
            Ok(n.to_string())
        }
        Kind::Time => {
            let t = chrono::NaiveTime::parse_from_str(v, "%H:%M")
                .or_else(|_| chrono::NaiveTime::parse_from_str(v, "%-H:%M"))
                .map_err(|_| format!("{} must look like 08:30 (24-hour)", s.label))?;
            Ok(t.format("%H:%M").to_string())
        }
        Kind::Fraction => {
            let f: f64 = v
                .parse()
                .map_err(|_| format!("{} must be a number between 0 and 1", s.label))?;
            if !(0.0..=1.0).contains(&f) {
                return Err(format!("{} must be between 0 and 1", s.label));
            }
            Ok(format!("{f}"))
        }
        Kind::List => {
            let items: Vec<String> = v
                .split([',', '\n'])
                .map(|w| w.trim().to_string())
                .filter(|w| !w.is_empty())
                .collect();
            if key == "skipped_folder_roles" {
                for item in &items {
                    if ![
                        "inbox",
                        "sent",
                        "drafts",
                        "junk",
                        "trash",
                        "archive",
                        "templates",
                        "outbox",
                        "other",
                    ]
                    .contains(&item.to_ascii_lowercase().as_str())
                    {
                        return Err(format!("{item} is not a folder role"));
                    }
                }
                return Ok(items
                    .iter()
                    .map(|i| i.to_ascii_lowercase())
                    .collect::<Vec<_>>()
                    .join(","));
            }
            Ok(items.join(","))
        }
        Kind::Text => Ok(v.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_checked_and_tidied() {
        assert_eq!(validate("sweep_minutes", " 10 ").unwrap(), "10");
        assert!(validate("sweep_minutes", "0")
            .unwrap_err()
            .contains("between 1 and 1440"));
        assert!(validate("sweep_minutes", "soon").is_err());
        assert_eq!(validate("briefing_enabled", "Off").unwrap(), "false");
        assert!(validate("briefing_enabled", "maybe").is_err());
        assert_eq!(validate("briefing_time", "7:05").unwrap(), "07:05");
        assert!(validate("briefing_time", "25:00").is_err());
        assert_eq!(
            validate("extract_confidence_threshold", "0.6").unwrap(),
            "0.6"
        );
        assert!(validate("extract_confidence_threshold", "1.5").is_err());
        assert_eq!(
            validate("skipped_folder_roles", "Junk, Trash").unwrap(),
            "junk,trash"
        );
        assert!(validate("skipped_folder_roles", "junk,attic")
            .unwrap_err()
            .contains("attic"));
        assert_eq!(
            validate("file_search_excludes", "Work/secrets, .notes").unwrap(),
            "Work/secrets,.notes"
        );
        assert!(validate("no_such_thing", "1").is_err());
        assert!(
            SPECS.iter().all(|s| validate(s.key, s.default).is_ok()),
            "every default is valid"
        );
    }
}
