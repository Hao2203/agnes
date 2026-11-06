//! Model-call data: the semantic prompt and the concrete request.
//!
//! `core` never assembles a request itself — the host decides what the model
//! sees. [`Turn::prompt`](crate::turn::Turn::prompt) is the standard
//! projection of the turn's state; [`Prompt`] is only the resulting data.

use crate::config::{ModelName, Thinking};
use crate::content::Content;
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

/// The concrete request the seam carries: the semantic [`Prompt`] plus the
/// request parameters the host wires in.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: ModelName,
    /// Whether the model should reason and how hard; `Off` sends no parameter.
    pub thinking: Thinking,
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
/// itself lives in the recorded message.
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

/// One model round trip's result. The model that produced it is known to the
/// host from the request it sent; core does not need to echo it.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
}
