//! The case file format: what goes in, what Belvedere is expected to do.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// One suite: a list of cases, usually from `evals/<suite>/cases.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suite {
    pub name: String,
    pub cases: Vec<Case>,
}

/// One scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Case {
    /// Unique within the suite, e.g. `create-dentist-friday`.
    pub id: String,
    /// Grouping for the report: `create`, `date`, `complete`, `hostile`, ...
    pub kind: String,
    /// The moment the case happens, RFC 3339 with offset. "Friday" means
    /// the Friday after this.
    pub now: String,
    /// What Belvedere sees.
    pub input: Input,
    /// Tasks that exist before the input arrives.
    #[serde(default)]
    pub tasks: Vec<TaskFixture>,
    pub expect: Expect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Input {
    /// A conversation; the last turn is the one Belvedere answers.
    Chat { turns: Vec<Turn> },
    /// An email to read.
    Email {
        from: String,
        subject: String,
        body: String,
        #[serde(default)]
        date: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    /// `user` or `assistant`.
    pub role: String,
    pub content: String,
}

/// A task as it exists at the start of a case.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskFixture {
    pub id: i64,
    pub title: String,
    #[serde(default)]
    pub notes: String,
    /// RFC 3339, or empty.
    #[serde(default)]
    pub due_at: String,
    /// `open`, `done`, `dismissed`. Default open.
    #[serde(default = "open")]
    pub status: String,
}

fn open() -> String {
    "open".into()
}

/// What should be true afterwards. Every field present is checked;
/// absent fields are not.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Expect {
    /// Tasks that must have been created, each matched in order of the
    /// list against the created tasks.
    #[serde(default)]
    pub created: Vec<CreatedExpect>,
    /// Exactly this many tasks created. Defaults to `created.len()` when
    /// `created` is given, otherwise unchecked.
    #[serde(default)]
    pub created_count: Option<usize>,
    /// Ids (from `tasks`) that must be done afterwards.
    #[serde(default)]
    pub completed: Vec<i64>,
    /// Ids that must be soft-deleted afterwards.
    #[serde(default)]
    pub deleted: Vec<i64>,
    /// Ids that must still be open and not deleted afterwards.
    #[serde(default)]
    pub untouched: Vec<i64>,
    /// Updates that must have happened to existing tasks.
    #[serde(default)]
    pub updated: Vec<UpdatedExpect>,
    /// No task created, changed, completed, or deleted.
    #[serde(default)]
    pub tasks_unchanged: bool,
    /// Words (case-insensitive) the reply must contain, all of them.
    #[serde(default)]
    pub reply_contains: Vec<String>,
    /// Words the reply must not contain.
    #[serde(default)]
    pub reply_lacks: Vec<String>,
    /// For email cases: what the extraction must say. Present means the
    /// case runs through extraction rather than chat.
    #[serde(default)]
    pub extract: Option<ExtractExpect>,
}

/// Expectations on an extraction result. Absent fields are unchecked.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractExpect {
    pub action_needed: bool,
    #[serde(default)]
    pub kind: Option<String>,
    /// `YYYY-MM-DD`.
    #[serde(default)]
    pub due_date: Option<String>,
    #[serde(default)]
    pub amount: Option<f64>,
    #[serde(default)]
    pub title_contains: Vec<String>,
    /// Words that must never appear in the title (injected instructions).
    #[serde(default)]
    pub title_lacks: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreatedExpect {
    /// All of these appear in the title (case-insensitive).
    #[serde(default)]
    pub title_contains: Vec<String>,
    /// Local calendar day of the due date, `YYYY-MM-DD`.
    #[serde(default)]
    pub due_date: Option<String>,
    /// Local time of the due date, `HH:MM`.
    #[serde(default)]
    pub due_time: Option<String>,
    /// Must have no due date at all.
    #[serde(default)]
    pub no_due: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdatedExpect {
    pub id: i64,
    #[serde(default)]
    pub title_contains: Vec<String>,
    #[serde(default)]
    pub due_date: Option<String>,
    #[serde(default)]
    pub due_time: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("cannot read {0}: {1}")]
    Read(String, std::io::Error),
    #[error("{0} is not a valid suite: {1}")]
    Parse(String, serde_json::Error),
    #[error("case ids must be unique; `{0}` appears twice")]
    DuplicateId(String),
    #[error("case `{0}` has a bad `now`: {1}")]
    BadNow(String, String),
}

impl Suite {
    pub fn load(path: &Path) -> Result<Suite, LoadError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| LoadError::Read(path.display().to_string(), e))?;
        let suite: Suite = serde_json::from_str(&text)
            .map_err(|e| LoadError::Parse(path.display().to_string(), e))?;
        suite.validate()?;
        Ok(suite)
    }

    pub fn validate(&self) -> Result<(), LoadError> {
        let mut seen = std::collections::HashSet::new();
        for case in &self.cases {
            if !seen.insert(case.id.as_str()) {
                return Err(LoadError::DuplicateId(case.id.clone()));
            }
            chrono::DateTime::parse_from_rfc3339(&case.now)
                .map_err(|e| LoadError::BadNow(case.id.clone(), e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_chat_case() {
        let json = r#"{"name":"t","cases":[{"id":"a","kind":"create","now":"2026-10-20T09:00:00-05:00",
            "input":{"chat":{"turns":[{"role":"user","content":"remind me to call the dentist Friday"}]}},
            "expect":{"created":[{"title_contains":["dentist"],"due_date":"2026-10-23"}]}}]}"#;
        let suite: Suite = serde_json::from_str(json).unwrap();
        suite.validate().unwrap();
        assert_eq!(suite.cases[0].tasks.len(), 0);
        assert_eq!(
            suite.cases[0].expect.created[0].due_date.as_deref(),
            Some("2026-10-23")
        );
    }

    #[test]
    fn rejects_duplicate_ids_and_bad_dates() {
        let dup = Suite {
            name: "t".into(),
            cases: vec![
                case("a", "2026-10-20T09:00:00-05:00"),
                case("a", "2026-10-20T09:00:00-05:00"),
            ],
        };
        assert!(matches!(dup.validate(), Err(LoadError::DuplicateId(_))));
        let bad = Suite {
            name: "t".into(),
            cases: vec![case("a", "tomorrow")],
        };
        assert!(matches!(bad.validate(), Err(LoadError::BadNow(..))));
    }

    fn case(id: &str, now: &str) -> Case {
        Case {
            id: id.into(),
            kind: "create".into(),
            now: now.into(),
            input: Input::Chat { turns: vec![] },
            tasks: vec![],
            expect: Expect::default(),
        }
    }
}
