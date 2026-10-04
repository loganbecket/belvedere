//! The line protocol between the service and the model helper process.
//!
//! The helper (`belvedere-model`) is started with the model to load and
//! then reads one JSON command per line on stdin and writes one JSON event
//! per line on stdout. Killing the helper is how a model is unloaded; all
//! of its memory goes with it.

use serde::{Deserialize, Serialize};

/// One turn of a conversation, for the model's chat template.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// `system`, `user`, or `assistant`.
    pub role: String,
    pub content: String,
}

/// Service -> helper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Generate a reply to a raw `prompt`, streaming `Event::Text` until
    /// `Event::Done`. Debug use.
    Generate { prompt: String, max_tokens: u32 },
    /// Continue a conversation: the helper applies the model's own chat
    /// template to `messages` and streams the assistant's reply.
    Chat {
        messages: Vec<ChatMessage>,
        max_tokens: u32,
    },
    /// Stop the generation in progress, if any. A `Done` follows.
    Cancel,
}

/// Helper -> service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Event {
    /// The model is in memory and ready.
    Loaded {
        gpu_layers: u32,
        context: u32,
        load_ms: u64,
    },
    Text {
        text: String,
    },
    Done {
        tokens: u32,
        seconds: f32,
    },
    /// Something failed. After a failed load the helper exits.
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_and_events_round_trip_as_single_lines() {
        let cmd = Command::Generate {
            prompt: "Hello\nthere".into(),
            max_tokens: 8,
        };
        let line = serde_json::to_string(&cmd).unwrap();
        assert!(!line.contains('\n'), "a command must fit on one line");
        assert_eq!(serde_json::from_str::<Command>(&line).unwrap(), cmd);
        assert_eq!(
            serde_json::to_string(&Command::Cancel).unwrap(),
            r#"{"cmd":"cancel"}"#
        );

        let ev = Event::Text {
            text: "a line\nbreak".into(),
        };
        let line = serde_json::to_string(&ev).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), ev);
    }
}
