//! Async I/O seam to a chat-completion model.

use crate::http::TransportError;
use agnes_core::llm::{LlmRequest, LlmResponse};
use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("LLM API error (HTTP {status}): {detail}")]
    Api {
        status: http::StatusCode,
        detail: String,
    },
    #[error("malformed LLM response: {detail}")]
    Malformed { detail: String },
}

/// Async I/O seam to a chat-completion model. Implementations do the async
/// I/O; the state machine and driver stay runtime-agnostic.
#[async_trait]
pub trait Llm: Send + Sync {
    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError>;
}
