//! Runs cases through the real agent loop against an in-memory task store.

use std::path::PathBuf;

use belvedere_core::agent::{self, NoProgress};
use belvedere_core::engine::Engine;
use belvedere_core::extract::{self, EmailInput};
use belvedere_core::model_ipc::ChatMessage;
use belvedere_core::tools::{MemoryStore, ToolTask};
use chrono::{DateTime, Local};

use crate::cases::{Case, Input, TaskFixture};
use crate::runner::Runner;
use crate::score::{Outcome, TaskAfter};

pub struct AgentRunner {
    engine: Engine,
    model_name: String,
    gpu: bool,
    max_tokens: u32,
}

impl AgentRunner {
    pub async fn new(model_path: PathBuf, model_name: String, gpu: bool) -> Result<Self, String> {
        let engine = Engine::new();
        engine.load(model_path, model_name.clone(), gpu).await?;
        Ok(AgentRunner {
            engine,
            model_name,
            gpu,
            max_tokens: 768,
        })
    }

    pub async fn shutdown(&self) {
        self.engine.unload().await;
    }
}

fn fixtures_to_tasks(fixtures: &[TaskFixture]) -> Vec<ToolTask> {
    fixtures
        .iter()
        .map(|f| ToolTask {
            id: f.id,
            title: f.title.clone(),
            notes: f.notes.clone(),
            due_at: f.due_at.clone(),
            status: f.status.clone(),
        })
        .collect()
}

/// What the store looks like afterwards, in the harness's terms.
fn store_to_outcome(store: &MemoryStore, fixtures: &[TaskFixture]) -> Vec<TaskAfter> {
    let fixture_ids: Vec<i64> = fixtures.iter().map(|f| f.id).collect();
    let mut after: Vec<TaskAfter> = store
        .tasks
        .iter()
        .map(|t| TaskAfter {
            fixture_id: fixture_ids.contains(&t.id).then_some(t.id),
            title: t.title.clone(),
            notes: t.notes.clone(),
            due_at: t.due_at.clone(),
            status: t.status.clone(),
            deleted: false,
        })
        .collect();
    for t in &store.deleted {
        after.push(TaskAfter {
            fixture_id: fixture_ids.contains(&t.id).then_some(t.id),
            title: t.title.clone(),
            notes: t.notes.clone(),
            due_at: t.due_at.clone(),
            status: t.status.clone(),
            deleted: true,
        });
    }
    // Keep fixture order first, then created tasks in creation order.
    after.sort_by_key(|t| match t.fixture_id {
        Some(id) => (0, id),
        None => (1, 0),
    });
    after
}

impl AgentRunner {
    async fn run_extraction(&mut self, case: &Case, now: DateTime<Local>) -> Outcome {
        let Input::Email {
            from,
            subject,
            body,
            date,
        } = &case.input
        else {
            return Outcome {
                error: Some("extract expectations need an email input".into()),
                ..Outcome::unchanged(&case.tasks)
            };
        };
        let (from_name, from_addr) = match from.rsplit_once('<') {
            Some((n, a)) => (
                n.trim().trim_matches('"').to_string(),
                a.trim_end_matches('>').to_string(),
            ),
            None => (String::new(), from.clone()),
        };
        let email = EmailInput {
            from_name,
            from_addr,
            subject: subject.clone(),
            date: date.clone(),
            body: body.clone(),
            attachments: Vec::new(),
        };
        let use_grammar = std::env::var_os("BELVEDERE_EXTRACT_GRAMMAR").is_some();
        let done = extract::extract(&self.engine, &email, now, use_grammar).await;
        if std::env::var_os("BELVEDERE_EVAL_DEBUG").is_some() {
            eprintln!(
                "--- {} ({} round(s), {:.1}s)\n    {:?}",
                case.id, done.rounds, done.seconds, done.result
            );
            if let Some(e) = &done.error {
                eprintln!("    error: {e}");
            }
        }
        Outcome {
            tasks: Vec::new(),
            reply: String::new(),
            error: done.error,
            extraction: Some(done.result),
            seconds: done.seconds,
        }
    }
}

impl Runner for AgentRunner {
    async fn run(&mut self, case: &Case) -> Outcome {
        let now: DateTime<Local> = match DateTime::parse_from_rfc3339(&case.now) {
            Ok(d) => d.with_timezone(&Local),
            Err(e) => {
                return Outcome {
                    error: Some(format!("bad now: {e}")),
                    ..Outcome::unchanged(&case.tasks)
                }
            }
        };
        if case.expect.extract.is_some() {
            return self.run_extraction(case, now).await;
        }
        let history: Vec<ChatMessage> = match &case.input {
            Input::Chat { turns } => turns
                .iter()
                .map(|t| ChatMessage {
                    role: t.role.clone(),
                    content: t.content.clone(),
                })
                .collect(),
            Input::Email {
                from,
                subject,
                body,
                date,
            } => vec![ChatMessage {
                role: "user".into(),
                content: format!(
                    "An email arrived.\nFrom: {from}\nDate: {date}\nSubject: {subject}\n\n{body}"
                ),
            }],
        };
        let mut store = MemoryStore::with(fixtures_to_tasks(&case.tasks));
        let turn = tokio::time::timeout(
            std::time::Duration::from_secs(600),
            agent::run_turn(
                &self.engine,
                &mut store,
                &history,
                now,
                self.max_tokens,
                &mut NoProgress,
            ),
        )
        .await;
        if let Ok(turn) = &turn {
            if std::env::var_os("BELVEDERE_EVAL_DEBUG").is_some() {
                eprintln!("--- {} ({} round(s))", case.id, turn.trace.rounds);
                for (call, outcome) in &turn.trace.calls {
                    eprintln!("    call {} {} -> {:?}", call.name, call.arguments, outcome);
                }
                eprintln!("    reply: {:?}", turn.reply);
                if let Some(e) = &turn.error {
                    eprintln!("    error: {e}");
                }
            }
        }
        match turn {
            Ok(turn) => Outcome {
                tasks: store_to_outcome(&store, &case.tasks),
                reply: turn.reply,
                error: turn.error,
                extraction: None,
                seconds: 0.0,
            },
            Err(_) => Outcome {
                tasks: store_to_outcome(&store, &case.tasks),
                reply: String::new(),
                error: Some("timed out after ten minutes".into()),
                extraction: None,
                seconds: 0.0,
            },
        }
    }

    fn describe(&self) -> String {
        format!(
            "{} on {} (agent)",
            self.model_name,
            if self.gpu { "GPU" } else { "CPU" }
        )
    }
}
