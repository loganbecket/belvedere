//! The loop that lets a chat turn change tasks: ask the model, carry out
//! its tool calls under the rules in `tools`, show it the results, repeat
//! until it answers in words.

use chrono::{DateTime, Local};

use crate::engine::{Chunk, Engine};
use crate::model_ipc::{ChatMessage, Grammar};
use crate::think::ThinkFilter;
use crate::tools::{self, TaskStore, ToolCall, Trace};

/// Most model rounds per user turn. Each tool round costs a model call.
pub const MAX_ROUNDS: usize = 4;

/// Progress a caller can show while a turn runs.
pub trait Progress {
    /// A piece of the final answer, as it streams.
    fn text(&mut self, piece: &str);
    /// A short status: "thinking", "working", "writing".
    fn status(&mut self, status: &str);
}

/// Where the final answer should go when nobody is watching.
pub struct NoProgress;
impl Progress for NoProgress {
    fn text(&mut self, _: &str) {}
    fn status(&mut self, _: &str) {}
}

/// What a turn produced.
#[derive(Debug, Clone, Default)]
pub struct TurnResult {
    /// The visible answer (think blocks and tool calls removed).
    pub reply: String,
    pub trace: Trace,
    /// Whether any tool changed a task (so the UI can refresh).
    pub changed_tasks: bool,
    /// A model or helper failure, if the turn could not finish.
    pub error: Option<String>,
}

/// Runs one user turn. `history` is the conversation so far, ending with
/// the user's latest message (no system message; that is built here).
pub async fn run_turn(
    engine: &Engine,
    store: &mut (dyn TaskStore + Send),
    history: &[ChatMessage],
    now: DateTime<Local>,
    max_tokens: u32,
    progress: &mut (dyn Progress + Send),
) -> TurnResult {
    let mut result = TurnResult::default();
    let delete_ok = tools::delete_confirmed(history);
    // The JSON grammar is optional: the models in use write valid tool
    // calls on their own, malformed ones are caught and reported back, and
    // the lazy-grammar sampler has crashed the helper on this machine. Set
    // BELVEDERE_TOOL_GRAMMAR=1 to turn it on.
    let grammar = std::env::var_os("BELVEDERE_TOOL_GRAMMAR").map(|_| Grammar {
        text: tools::TOOL_CALL_GRAMMAR.to_string(),
        root: "root".to_string(),
        triggers: vec![tools::OPEN_TAG.to_string()],
    });

    // The conversation the model sees: system (with the live task list),
    // then the history, then tool results as they accumulate.
    let mut transcript: Vec<ChatMessage> = Vec::new();
    for round in 0..MAX_ROUNDS {
        result.trace.rounds = round + 1;
        let tasks = match store.list() {
            Ok(t) => t,
            Err(e) => {
                result.error = Some(format!("could not read tasks: {e}"));
                return result;
            }
        };
        let mut messages = vec![ChatMessage {
            role: "system".into(),
            content: tools::system_prompt(now, &tasks),
        }];
        messages.extend(history.iter().cloned());
        messages.extend(transcript.iter().cloned());

        progress.status(if round == 0 { "thinking" } else { "working" });
        let last_round = round + 1 == MAX_ROUNDS;
        let raw = match generate(
            engine,
            messages,
            max_tokens,
            if last_round { None } else { grammar.clone() },
            progress,
        )
        .await
        {
            Ok(raw) => raw,
            Err(e) => {
                result.error = Some(e);
                return result;
            }
        };
        let (text, calls) = tools::parse_reply(&raw);
        if calls.is_empty() || last_round {
            result.reply = text;
            return result;
        }

        // Carry out the calls and show the model what happened.
        transcript.push(ChatMessage {
            role: "assistant".into(),
            content: raw.clone(),
        });
        let mut outcomes = Vec::new();
        for call in calls {
            let (call, outcome) = match call {
                Ok(call) => {
                    let outcome = tools::execute(&call, store, now, delete_ok);
                    if outcome.is_ok() && call.name != "list_tasks" {
                        result.changed_tasks = true;
                    }
                    (call, outcome)
                }
                Err(e) => (
                    ToolCall {
                        name: "invalid".into(),
                        arguments: serde_json::Value::Null,
                    },
                    Err(e),
                ),
            };
            outcomes.push(format!(
                "{}: {}",
                call.name,
                match &outcome {
                    Ok(s) => s.clone(),
                    Err(e) => format!("error: {e}"),
                }
            ));
            result.trace.calls.push((call, outcome));
        }
        transcript.push(ChatMessage {
            role: "user".into(),
            content: {
                let any_error = result
                    .trace
                    .calls
                    .iter()
                    .rev()
                    .take(outcomes.len())
                    .any(|(_, o)| o.is_err());
                if any_error {
                    format!(
                        "[tool results]\n{}\n\nAt least one call failed. Fix it and call again now; do not tell the user it was done.",
                        outcomes.join("\n")
                    )
                } else {
                    format!(
                        "[tool results]\n{}\n\nNow reply to the user in plain words about what was done. Do not repeat the tool calls unless something else is still needed.",
                        outcomes.join("\n")
                    )
                }
            },
        });
    }
    result
}

/// One model call. Streams visible text to `progress` only until a tool
/// call begins, since text around tool calls is for the loop, not the
/// user; the final round (no tool calls) streams fully.
async fn generate(
    engine: &Engine,
    messages: Vec<ChatMessage>,
    max_tokens: u32,
    grammar: Option<Grammar>,
    progress: &mut (dyn Progress + Send),
) -> Result<String, String> {
    let mut chunks = engine.chat_fast(messages, max_tokens, grammar).await;
    let mut filter = ThinkFilter::new();
    let mut raw = String::new();
    let mut streamed_to = 0usize;
    let mut streaming = true;
    let mut was_thinking = false;
    while let Some(chunk) = chunks.recv().await {
        match chunk {
            Chunk::Text(piece) => {
                let visible = filter.push(&piece);
                if filter.thinking() != was_thinking {
                    was_thinking = filter.thinking();
                    progress.status(if was_thinking { "thinking" } else { "writing" });
                }
                raw.push_str(&visible);
                if streaming {
                    if let Some(at) = raw.find(tools::OPEN_TAG) {
                        // Hold everything from the tag on; what came before
                        // was already shown.
                        streaming = false;
                        if at > streamed_to {
                            progress.text(&raw[streamed_to..at]);
                            streamed_to = at;
                        }
                    } else {
                        // Keep back a possible partial tag at the end.
                        let safe = raw.len() - partial_tag_len(&raw);
                        if safe > streamed_to {
                            progress.text(&raw[streamed_to..safe]);
                            streamed_to = safe;
                        }
                    }
                }
            }
            Chunk::Done { .. } => break,
            Chunk::Failed(e) => return Err(e),
        }
    }
    raw.push_str(&filter.finish());
    if streaming && raw.len() > streamed_to {
        progress.text(&raw[streamed_to..]);
    }
    Ok(raw)
}

/// Bytes at the end of `s` that could be the start of `<tool_call>`.
fn partial_tag_len(s: &str) -> usize {
    let tag = tools::OPEN_TAG;
    for n in (1..tag.len()).rev() {
        if s.len() >= n && s.is_char_boundary(s.len() - n) && tag.starts_with(&s[s.len() - n..]) {
            return n;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_tag_detection() {
        assert_eq!(partial_tag_len("hello <tool"), 5);
        assert_eq!(partial_tag_len("hello <"), 1);
        assert_eq!(partial_tag_len("hello"), 0);
        assert_eq!(partial_tag_len("a < b"), 0);
    }
}
