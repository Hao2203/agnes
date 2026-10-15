//! Conversation protocol types.
//!
//! [`Message`] is an enum so protocol violations — a tool result without a
//! preceding call, a system message mid-conversation — are hard to express.

use crate::config::SystemPrompt;
use crate::content::Content;
use agnes_util::NonEmpty;
use serde::de::DeserializeOwned;
use std::fmt;
use std::sync::Arc;

/// Identifier of a tool call, assigned by the model. Cloned for every tool
/// result written back into the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallId(Arc<str>);

impl ToolCallId {
    pub fn new(value: impl AsRef<str>) -> Self {
        Self(Arc::from(value.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Raw JSON arguments of a tool call, not yet parsed against the tool's
/// schema. Parsing happens inside the tool (parse at the boundary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolArguments(Arc<str>);

impl ToolArguments {
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse<T: DeserializeOwned>(&self) -> serde_json::Result<T> {
        serde_json::from_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: crate::tool::ToolName,
    pub arguments: ToolArguments,
}

/// Output of a tool run. Failures are reported *as* output so the model can
/// see them and react. Cloned into the transcript once per result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: Arc<str>,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn success(content: impl Into<Arc<str>>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<Arc<str>>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Token accounting for one model response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
}

impl TokenUsage {
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            input: self.input.saturating_add(other.input),
            output: self.output.saturating_add(other.output),
        }
    }
}

impl fmt::Display for TokenUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} in / {} out", self.input, self.output)
    }
}

/// A message produced by the model: either pure text, or tool calls with
/// optional commentary text alongside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantMessage {
    /// A pure text reply.
    Text(Content),
    /// Tool calls — always at least one — with any accompanying text.
    ToolCalls {
        content: Option<Content>,
        calls: NonEmpty<ToolCall>,
    },
}

/// One entry of the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    System {
        content: SystemPrompt,
    },
    User {
        content: Content,
    },
    Assistant(AssistantMessage),
    ToolResult {
        call_id: ToolCallId,
        output: ToolOutput,
    },
}

/// Ordered conversation history. The first entry is always the system
/// prompt, followed by any prior turns and the user goal; only the state
/// machine appends.
#[derive(Debug, Clone)]
pub struct Transcript {
    messages: Vec<Message>,
}

impl Transcript {
    /// Starts a conversation: system prompt, prior turns (when continuing a
    /// session), then the new goal.
    pub fn new(system: SystemPrompt, goal: Content, history: Vec<Message>) -> Self {
        let mut messages = Vec::with_capacity(history.len() + 2);
        messages.push(Message::System { content: system });
        messages.extend(history);
        messages.push(Message::User { content: goal });
        Self { messages }
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }

    /// Consumes the transcript into the prior-turn context of a follow-up
    /// conversation: everything except the leading system prompt.
    pub fn into_history(self) -> Vec<Message> {
        let mut messages = self.messages;
        if matches!(messages.first(), Some(Message::System { .. })) {
            messages.remove(0);
        }
        messages
    }

    pub(crate) fn push(&mut self, message: Message) {
        self.messages.push(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn content(text: &str) -> Content {
        Content::try_from(text).unwrap()
    }

    #[test]
    fn transcript_starts_with_system_and_goal() {
        let t = Transcript::new(
            SystemPrompt::new("be brief"),
            content("list files"),
            Vec::new(),
        );
        assert_eq!(t.messages().len(), 2);
        assert!(matches!(t.messages()[0], Message::System { .. }));
        assert!(
            matches!(t.messages()[1], Message::User { ref content } if content.as_str() == "list files")
        );
    }

    #[test]
    fn history_slots_in_between_system_and_goal() {
        let prior = vec![
            Message::User {
                content: content("first task"),
            },
            Message::Assistant(AssistantMessage::Text(content("done"))),
        ];
        let history =
            Transcript::new(SystemPrompt::new("sys"), content("second task"), prior).into_history();
        assert_eq!(history.len(), 3); // user, assistant, user — system dropped
        let round = Transcript::new(SystemPrompt::new("sys"), content("third"), history.clone());
        assert_eq!(round.messages().len(), 5);
        assert!(matches!(round.messages()[0], Message::System { .. }));
        assert!(
            matches!(round.messages()[3], Message::User { ref content } if content.as_str() == "second task")
        );
        assert!(
            matches!(round.messages()[4], Message::User { ref content } if content.as_str() == "third")
        );
    }

    #[test]
    fn tool_arguments_parse_against_concrete_type() {
        #[derive(Deserialize)]
        struct Args {
            path: String,
        }
        let args = ToolArguments::new(r#"{"path": "a.txt"}"#);
        let parsed: Args = args.parse().unwrap();
        assert_eq!(parsed.path, "a.txt");
        assert!(ToolArguments::new("not json").parse::<Args>().is_err());
    }

    #[test]
    fn token_usage_accumulates_saturating() {
        let big = TokenUsage {
            input: u64::MAX,
            output: 1,
        };
        let sum = big.saturating_add(TokenUsage {
            input: 5,
            output: 2,
        });
        assert_eq!(sum.input, u64::MAX);
        assert_eq!(sum.output, 3);
        assert_eq!(sum.to_string(), format!("{} in / 3 out", u64::MAX));
    }
}
