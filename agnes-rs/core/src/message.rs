//! Conversation protocol types.
//!
//! [`Message`] is an enum so protocol violations — a tool result without a
//! preceding call, a system message mid-conversation — are hard to express.
//!
//! String storage follows one rule: values that may be short (call ids,
//! arguments) wrap `ArcColdString` so short text is inline and longer text is
//! refcounted; values that certainly cannot be inlined (tool output content,
//! streamed fragments) stay on `Arc<str>`. The internal representations never
//! appear in public signatures.

use crate::config::SystemPrompt;
use crate::content::Content;
use crate::tool::ToolName;
use agnes_util::NonEmpty;
use cold_string::ArcColdString;
use serde::de::DeserializeOwned;
use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

/// Identifier of a tool call, assigned by the model. Cloned for every tool
/// result written back into the log; short identifiers are stored inline and
/// longer ones are refcounted.
#[derive(Clone, PartialEq, Eq)]
pub struct ToolCallId(ArcColdString);

impl ToolCallId {
    pub fn new(value: impl AsRef<str>) -> Self {
        Self(ArcColdString::new(value.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ToolCallId").field(&self.as_str()).finish()
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialOrd for ToolCallId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ToolCallId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

/// Raw JSON arguments of a tool call, not yet parsed against the tool's
/// schema. Parsing happens inside the tool (parse at the boundary).
#[derive(Clone, PartialEq, Eq)]
pub struct ToolArguments(ArcColdString);

impl ToolArguments {
    pub fn new(value: impl AsRef<str>) -> Self {
        Self(ArcColdString::new(value.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn parse<T: DeserializeOwned>(&self) -> serde_json::Result<T> {
        serde_json::from_str(self.0.as_str())
    }
}

impl fmt::Debug for ToolArguments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ToolArguments")
            .field(&self.as_str())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: crate::tool::ToolName,
    pub arguments: ToolArguments,
}

/// Output of a tool run. Failures are reported *as* output so the model can
/// see them and react. Cloned into the log once per result. The content is
/// bulk text (file contents, command output) that can never be inline, so it
/// stays on the standard `Arc<str>`.
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

/// One message of the conversation. Messages are recorded as
/// [`Event::Message`](crate::event::Event::Message) entries of the context
/// log, so their order relative to everything else that happened is fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// The system prompt as a model input element. The prompt itself is a
    /// field of the context; this variant exists for the projection into a
    /// model request.
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

/// One fragment of a message the model is still producing, in arrival order.
/// Fragments are not messages: only the assembled whole — which the host
/// receives when the message ends — is. Streaming adapters map their
/// protocol's frames onto these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageDelta {
    /// Text appended to the message body.
    Text(Arc<str>),
    /// The model announced a tool call; its arguments may still be streaming.
    ToolCallStarted { id: ToolCallId, name: ToolName },
    /// Raw arguments text appended to the named tool call.
    ToolCallArguments { id: ToolCallId, fragment: Arc<str> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

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
