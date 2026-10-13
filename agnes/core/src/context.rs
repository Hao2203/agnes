//! Session context: everything the model needs to know beyond the current
//! goal — the system prompt plus every prior turn in order (user goals,
//! assistant replies, tool calls and their results as messages).
//!
//! A [`Context`] is the long-lived session state hosts carry between runs;
//! the state machine consumes it per run via
//! [`Context::into_transcript`] and the completed run hands back a fresh
//! [`Context`] through `RunSummary`.

use crate::config::SystemPrompt;
use crate::message::{Message, Transcript};

/// The system prompt and the ordered prior turns of a session.
#[derive(Debug, Clone)]
pub struct Context {
    system: SystemPrompt,
    messages: Vec<Message>,
}

impl Context {
    pub fn new(system: SystemPrompt) -> Self {
        Self {
            system,
            messages: Vec::new(),
        }
    }

    pub fn from_parts(system: SystemPrompt, messages: Vec<Message>) -> Self {
        Self { system, messages }
    }

    pub fn system(&self) -> &SystemPrompt {
        &self.system
    }

    /// Prior turns in order, excluding the system prompt. Includes user
    /// goals, assistant replies, tool calls and tool results.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Drops all prior turns; the system prompt is kept.
    pub fn reset(&mut self) {
        self.messages.clear();
    }

    /// Consumes the context into a [`Transcript`] for one agent run: system
    /// prompt, prior turns, then the new goal.
    pub fn into_transcript(self, goal: impl Into<String>) -> Transcript {
        Transcript::new(self.system, goal, self.messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::AssistantMessage;

    #[test]
    fn into_transcript_appends_goal_after_prior_turns() {
        let context = Context::from_parts(
            SystemPrompt::new("sys"),
            vec![
                Message::User {
                    content: "first".to_owned(),
                },
                Message::Assistant(AssistantMessage {
                    content: "reply".to_owned(),
                    tool_calls: vec![],
                }),
            ],
        );
        let transcript = context.into_transcript("second");
        assert_eq!(transcript.messages().len(), 4);
        assert!(matches!(transcript.messages()[0], Message::System { .. }));
        assert!(
            matches!(transcript.messages()[3], Message::User { ref content } if content == "second")
        );
    }

    #[test]
    fn reset_keeps_system_drops_turns() {
        let mut context = Context::new(SystemPrompt::new("sys"));
        context.messages.push(Message::User {
            content: "old".to_owned(),
        });
        assert_eq!(context.len(), 1);
        context.reset();
        assert!(context.is_empty());
        assert_eq!(context.system().as_str(), "sys");
    }
}
