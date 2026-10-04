//! Runners turn a case into an outcome. `ChatRunner` asks a real model;
//! `Scripted` answers from a table, for testing the harness itself.

#[cfg(test)]
use std::collections::HashMap;
use std::path::PathBuf;

use belvedere_core::engine::{Chunk, Engine};
use belvedere_core::model_ipc::ChatMessage;
use belvedere_core::think::ThinkFilter;

use crate::cases::{Case, Input};
use crate::score::Outcome;

/// Something that can run a case.
pub trait Runner {
    fn run(&mut self, case: &Case) -> impl std::future::Future<Output = Outcome> + Send;
    /// A short description for the results file ("Qwen3.5 9B on GPU").
    fn describe(&self) -> String;
}

/// Fixed answers keyed by case id. Unknown ids get the unchanged outcome.
#[cfg(test)]
#[derive(Default)]
pub struct Scripted {
    pub outcomes: HashMap<String, Outcome>,
}

#[cfg(test)]
impl Runner for Scripted {
    async fn run(&mut self, case: &Case) -> Outcome {
        self.outcomes
            .get(&case.id)
            .cloned()
            .unwrap_or_else(|| Outcome::unchanged(&case.tasks))
    }

    fn describe(&self) -> String {
        "scripted".into()
    }
}

/// Talks to a real model through the helper process. Until the chat has
/// tools (chunk 2.5) this can only produce a reply, never change tasks,
/// so task expectations fail; that is expected for now.
pub struct ChatRunner {
    engine: Engine,
    model_name: String,
    gpu: bool,
    max_tokens: u32,
}

impl ChatRunner {
    pub async fn new(model_path: PathBuf, model_name: String, gpu: bool) -> Result<Self, String> {
        let engine = Engine::new();
        engine.load(model_path, model_name.clone(), gpu).await?;
        Ok(ChatRunner {
            engine,
            model_name,
            gpu,
            max_tokens: 512,
        })
    }

    pub async fn shutdown(&self) {
        self.engine.unload().await;
    }
}

/// The standing instruction used for eval chats. Deliberately the same
/// shape as the service's, with the case's date instead of today's.
pub fn system_prompt(now: &str) -> String {
    let when = chrono::DateTime::parse_from_rfc3339(now)
        .map(|d| d.format("%A, %B %-d, %Y").to_string())
        .unwrap_or_else(|_| now.to_string());
    format!(
        "You are Belvedere, a discreet and capable personal butler who runs on the user's own computer. \
You help with tasks, reminders, mail, and the calendar. Be brief, plain, and useful; answer directly \
without preamble. Use American English. Today is {when}."
    )
}

impl Runner for ChatRunner {
    async fn run(&mut self, case: &Case) -> Outcome {
        let mut outcome = Outcome::unchanged(&case.tasks);
        let mut messages = vec![ChatMessage {
            role: "system".into(),
            content: system_prompt(&case.now),
        }];
        match &case.input {
            Input::Chat { turns } => {
                for t in turns {
                    messages.push(ChatMessage {
                        role: t.role.clone(),
                        content: t.content.clone(),
                    });
                }
            }
            Input::Email {
                from,
                subject,
                body,
                date,
            } => {
                messages.push(ChatMessage {
                    role: "user".into(),
                    content: format!(
                        "An email arrived.\nFrom: {from}\nDate: {date}\nSubject: {subject}\n\n{body}"
                    ),
                });
            }
        }

        let mut chunks = self.engine.chat(messages, self.max_tokens).await;
        let mut filter = ThinkFilter::new();
        let mut reply = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
        loop {
            let next = tokio::time::timeout_at(deadline, chunks.recv()).await;
            match next {
                Ok(Some(Chunk::Text(t))) => reply.push_str(&filter.push(&t)),
                Ok(Some(Chunk::Done { .. })) | Ok(None) => break,
                Ok(Some(Chunk::Failed(message))) => {
                    outcome.error = Some(message);
                    break;
                }
                Err(_) => {
                    outcome.error = Some("timed out after five minutes".into());
                    break;
                }
            }
        }
        reply.push_str(&filter.finish());
        outcome.reply = reply.trim().to_string();
        outcome
    }

    fn describe(&self) -> String {
        format!(
            "{} on {}",
            self.model_name,
            if self.gpu { "GPU" } else { "CPU" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_system_prompt_uses_the_case_date() {
        let p = system_prompt("2026-10-20T09:00:00-05:00");
        assert!(p.contains("Tuesday, October 20, 2026"));
        assert!(p.starts_with("You are Belvedere"));
    }
}
