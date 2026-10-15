//! Model-call data for one chat-completion round: the prompt the state
//! machine hands out, and the concrete request/response the seam carries.
//! Pure data — the seam itself lives in `agnes-io`.

use crate::config::ModelName;
use crate::content::Content;
use crate::message::{Message, TokenUsage, ToolCall};
use crate::tool::ToolSpec;
use agnes_util::NonEmpty;

/// The semantic model input for the next completion round: the conversation
/// so far and the tools offered this round. The host attaches the model name
/// to build the concrete [`LlmRequest`].
#[derive(Debug, Clone)]
pub struct Prompt {
    /// The model input history — the system prompt, prior turns, tool calls
    /// and their results, the goal.
    pub input: Vec<Message>,
    /// Tool definitions offered this round.
    pub tools: Vec<ToolSpec>,
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
