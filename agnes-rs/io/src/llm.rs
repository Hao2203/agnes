//! Async I/O seam to a chat-completion model.

use std::fmt;

use crate::http::TransportError;
use agnes_core::config::ConfigError;
use agnes_core::llm::{LlmRequest, LlmResponse};
use async_trait::async_trait;

/// An API error body: the raw response is kept as data, while `Display`
/// uses the bounded human-readable detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiBody {
    raw: String,
    detail: String,
}

impl ApiBody {
    pub fn new(raw: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            raw: raw.into(),
            detail: detail.into(),
        }
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for ApiBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

/// A response that parsed as JSON but does not describe a legal completion.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolError {
    #[error("response contains no choices")]
    NoChoices,
    #[error("finish_reason is missing")]
    MissingFinishReason,
    #[error("finish_reason is not supported")]
    UnknownFinishReason,
    #[error("finish_reason `stop` without content")]
    MissingContent,
    #[error("finish_reason `tool_calls` without any tool calls")]
    EmptyToolCalls,
    #[error("tool call id must not be empty")]
    EmptyToolCallId,
    #[error("tool name is invalid")]
    InvalidToolName(#[source] ConfigError),
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LlmError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("LLM API error (HTTP {status}): {body}")]
    Api {
        status: http::StatusCode,
        body: ApiBody,
    },
    #[error("could not decode the LLM response")]
    Decode(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// Async I/O seam to a chat-completion model. Implementations do the async
/// I/O; the state machine and driver stay runtime-agnostic.
#[async_trait]
pub trait Llm: Send + Sync {
    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError>;
}
