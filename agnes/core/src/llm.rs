//! Request/response data for one chat-completion round. Pure data — the
//! port that carries them lives in `agnes-ports`.

use crate::config::ModelName;
use crate::message::{AssistantMessage, Message, TokenUsage};
use crate::tool::ToolSpec;

/// Everything the model needs for one completion round.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: ModelName,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The model produced a final answer.
    Completed,
    /// The model asked for tool calls.
    ToolUse,
    /// Output hit the token limit.
    LengthExceeded,
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub assistant: AssistantMessage,
    pub usage: TokenUsage,
    pub stop_reason: StopReason,
}
