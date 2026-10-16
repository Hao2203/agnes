//! Model-call data: the semantic prompt, the concrete request, and the
//! builder ("the converter") a host supplies to turn session state into one.
//!
//! `core` never assembles a request itself — a task exposes its [`Context`]
//! and the host's [`RequestBuilder`] decides what the model sees: which
//! messages, which tools, any extra priming. The async seam that carries the
//! request lives in `agnes-io`.

use crate::config::ModelName;
use crate::content::Content;
use crate::context::Context;
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
    /// The standard projection of a context: its system prompt, then every
    /// message in the log, in order, then the environment's tools.
    pub fn from_context(context: &Context) -> Self {
        let mut input = Vec::with_capacity(context.messages().count() + 1);
        input.push(Message::System {
            content: context.system().clone(),
        });
        input.extend(context.messages().cloned());
        Self {
            input,
            tools: context.environment().tools().to_vec(),
        }
    }
}

/// Turns a [`Context`] into a concrete request. Hosts supply one and the
/// driver asks it for a request before every model call, so the shape of the
/// prompt is entirely the host's business — [`Prompt::from_context`] is the
/// standard projection, not a requirement.
pub trait RequestBuilder {
    fn build(&self, context: &Context) -> LlmRequest;
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
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemPrompt;
    use crate::context::Environment;
    use crate::tool::{JsonSchema, ToolName};
    use std::str::FromStr;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: ToolName::from_str(name).unwrap(),
            description: "".into(),
            parameters: JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
        }
    }

    #[test]
    fn the_standard_projection_prepends_the_system_prompt_and_keeps_order() {
        let mut context = Context::new(
            SystemPrompt::new("be brief"),
            Environment::new(vec![spec("read_file")]),
        );
        context.record(crate::event::Event::Message(Message::User {
            content: Content::try_from("hello").unwrap(),
        }));
        context.record(crate::event::Event::Message(Message::Assistant(
            crate::message::AssistantMessage::Text(Content::try_from("hi").unwrap()),
        )));

        let prompt = Prompt::from_context(&context);
        assert_eq!(prompt.input.len(), 3);
        assert!(
            matches!(&prompt.input[0], Message::System { content } if content.as_str() == "be brief")
        );
        assert!(
            matches!(&prompt.input[1], Message::User { content } if content.as_str() == "hello")
        );
        assert!(matches!(&prompt.input[2], Message::Assistant(_)));
        assert_eq!(prompt.tools.len(), 1);
        assert_eq!(prompt.tools[0].name.as_str(), "read_file");
    }
}
