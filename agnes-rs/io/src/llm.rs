//! Model-call vocabulary: ids, bus messages, provider marker, and the
//! provider-facing error type.

use std::fmt;

use crate::http::TransportError;
use agnes_core::config::{ConfigError, ModelName};
use agnes_core::llm::{LlmRequest, LlmResponse};
use agnes_core::message::TokenUsage;

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
    Decode(#[source] anyhow::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// The identity of one model call, chosen by the caller.
///
/// The reply carries the same id back; ids only need to be unique among the
/// calls in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LlmCallId(u64);

impl LlmCallId {
    /// Wraps a raw number.
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw number.
    pub fn value(&self) -> u64 {
        self.0
    }
}

/// One model call submitted through the ECS message bus.
///
/// The runtime writes it; a provider plugin encodes it for its API, performs
/// the round trip (today through `agnes-plugin-http`), and answers with
/// [`LlmReply`].
#[derive(Debug)]
pub struct LlmCall {
    pub id: LlmCallId,
    pub request: LlmRequest,
}

/// The outcome of one model call.
#[derive(Debug)]
pub struct LlmReply {
    pub id: LlmCallId,
    pub result: Result<LlmResponse, LlmError>,
}

/// One observation-only update from an in-flight model call.
///
/// Provider bridges emit these while a response streams in; the final
/// [`LlmReply`] still carries the complete response and is the only input to
/// the runtime state machine. Hosts use the updates for live output and
/// token accounting.
#[derive(Debug)]
pub struct LlmStreamEvent {
    pub id: LlmCallId,
    pub update: LlmStreamUpdate,
}

/// The payload of one [`LlmStreamEvent`].
#[derive(Debug)]
pub enum LlmStreamUpdate {
    /// The provider accepted the request and started streaming.
    Started,
    /// Assistant text produced by this update.
    Delta { text: String },
    /// Reasoning text produced by this update. Observation only: the final
    /// [`LlmReply`] carries the answer, not the reasoning.
    Reasoning { text: String },
    /// The provider reported token usage for the whole response.
    Usage { usage: TokenUsage },
}

/// Cancels an in-flight model call (best effort). A cancellation always ends
/// in an [`LlmReply`] carrying [`TransportError::Cancelled`], so the runtime
/// never waits forever.
#[derive(Clone, Copy, Debug)]
pub struct LlmCancel {
    pub id: LlmCallId,
}

/// Asks the provider plugin for its available models; it answers with one
/// [`ModelsListed`]. Repeated requests while one is in flight are ignored.
#[derive(Debug)]
pub struct ListModels;

/// The outcome of one model-list query.
#[derive(Debug)]
pub struct ModelsListed {
    pub result: Result<Vec<ModelName>, LlmError>,
}

/// Marks that a provider plugin is installed.
///
/// The runtime refuses to issue model calls without it, so a missing provider
/// fails loudly instead of letting calls disappear into the message bus.
pub struct LlmProvider;
