//! Model-call data: the semantic prompt, the concrete request, and the
//! standard projection from a sequence of runtime events.
//!
//! `core` never assembles a request itself — the host decides what the model
//! sees: which messages, which tools, any extra priming. The async seam that
//! carries the request lives in `agnes-io`.

use crate::config::ModelName;
use crate::content::Content;
use crate::event::Event;
use crate::message::{Message, TokenUsage, ToolCall};
use crate::tool::ToolSpec;
use agnes_util::NonEmpty;

/// The semantic model input for one completion round: the conversation so far
/// and the tools offered. The host attaches the model name to build the
/// concrete [`LlmRequest`].
#[derive(Debug, Clone)]
pub struct Prompt {
    /// The model input history — the system prompt, the messages recorded so
    /// far (goals, replies, tool calls and their results).
    pub input: Vec<Message>,
    /// Tool definitions offered this round.
    pub tools: Vec<ToolSpec>,
}

impl Prompt {
    /// The standard projection of a sequence of runtime events: the most
    /// recent header (system prompt and tools), then every message, in order.
    ///
    /// Events without a header — a hand-built or restored history whose driver
    /// has not yet recorded this turn's header — project to no system prompt
    /// and no tools.
    pub fn from_events(events: &[Event]) -> Self {
        let header = events.iter().rev().find_map(|event| match event {
            Event::Header(header) => Some(header),
            _ => None,
        });
        let messages = events
            .iter()
            .filter(|event| matches!(event, Event::Message(_)))
            .count();
        let mut input = Vec::with_capacity(messages + 1);
        if let Some(header) = header {
            input.push(Message::System {
                content: header.system().clone(),
            });
        }
        input.extend(events.iter().filter_map(|event| match event {
            Event::Message(message) => Some(message.clone()),
            _ => None,
        }));
        Self {
            input,
            tools: header
                .map(|header| header.environment().tools().to_vec())
                .unwrap_or_default(),
        }
    }
}

/// The concrete request the seam carries: the semantic [`Prompt`] plus the
/// model name the host wires in.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: ModelName,
    pub prompt: Prompt,
}

/// Why the model stopped — carrying the data that distinguishes the case,
/// so impossible combinations (tool use without calls, a final answer
/// without content) cannot be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The model produced a final answer; guaranteed non-empty.
    Completed { content: Content },
    /// The model asked for tool calls — at least one. `content` carries any
    /// text the model produced alongside the calls.
    ToolUse {
        content: Option<Content>,
        calls: NonEmpty<ToolCall>,
    },
    /// Output hit the token limit; the truncated text is optional.
    LengthExceeded { content: Option<Content> },
    /// The provider's content filter stopped the response; any partial text
    /// is optional.
    ContentFiltered { content: Option<Content> },
}

/// Why a model message ended, without duplicating its payload. The payload
/// itself lives in the recorded [`Message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    Completed,
    ToolUse,
    LengthExceeded,
    ContentFiltered,
}

impl From<&StopReason> for StopKind {
    fn from(stop: &StopReason) -> Self {
        match stop {
            StopReason::Completed { .. } => Self::Completed,
            StopReason::ToolUse { .. } => Self::ToolUse,
            StopReason::LengthExceeded { .. } => Self::LengthExceeded,
            StopReason::ContentFiltered { .. } => Self::ContentFiltered,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    /// 产生这条响应的模型 —— 由适配器从请求原样回带，切换模型后日志可追溯。
    pub model: ModelName,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemPrompt;
    use crate::header::{Environment, Header};
    use crate::tool::{JsonSchema, ToolName};
    use std::str::FromStr;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec::new(
            ToolName::from_str(name).unwrap(),
            "",
            JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
        )
    }

    #[test]
    fn the_standard_projection_uses_the_header_and_keeps_order() {
        let events = vec![
            Event::Header(Header::new(
                SystemPrompt::new("be brief"),
                Environment::new(vec![spec("read_file")]),
            )),
            Event::Message(Message::User {
                content: Content::try_from("hello").unwrap(),
            }),
            Event::Message(Message::Assistant(crate::message::AssistantMessage::Text(
                Content::try_from("hi").unwrap(),
            ))),
        ];

        let prompt = Prompt::from_events(&events);
        assert_eq!(prompt.input.len(), 3);
        assert!(
            matches!(&prompt.input[0], Message::System { content } if content.as_str() == "be brief")
        );
        assert!(
            matches!(&prompt.input[1], Message::User { content } if content.as_str() == "hello")
        );
        assert!(matches!(&prompt.input[2], Message::Assistant(_)));
        assert_eq!(prompt.tools.len(), 1);
        assert_eq!(prompt.tools[0].name().as_str(), "read_file");
    }

    #[test]
    fn a_later_header_replaces_the_prompt_and_the_tools() {
        let events = vec![
            Event::Header(Header::new(
                SystemPrompt::new("old"),
                Environment::new(vec![spec("read_file")]),
            )),
            Event::Header(Header::new(
                SystemPrompt::new("new"),
                Environment::default(),
            )),
        ];

        let prompt = Prompt::from_events(&events);
        assert!(
            matches!(&prompt.input[0], Message::System { content } if content.as_str() == "new")
        );
        assert!(prompt.tools.is_empty());
    }

    #[test]
    fn events_without_a_header_project_no_system_prompt_and_no_tools() {
        let events = vec![Event::Message(Message::User {
            content: Content::try_from("hello").unwrap(),
        })];

        let prompt = Prompt::from_events(&events);
        assert_eq!(prompt.input.len(), 1);
        assert!(matches!(&prompt.input[0], Message::User { .. }));
        assert!(prompt.tools.is_empty());
    }
}
