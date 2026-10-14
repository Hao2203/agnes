//! Request/response data for one chat-completion round. Pure data — the
//! port that carries them lives in `agnes-ports`.

use crate::config::ModelName;
use crate::content::Content;
use crate::message::{Message, TokenUsage, ToolCall};
use crate::tool::ToolSpec;
use agnes_util::NonEmpty;

/// Everything the model needs for one completion round.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: ModelName,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
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
