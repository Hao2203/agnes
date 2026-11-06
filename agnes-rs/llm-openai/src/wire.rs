//! Wire types for the OpenAI chat-completions API and mapping to core types.

use agnes_core::content::Content;
use agnes_core::llm::{LlmRequest, LlmResponse, StopReason};
use agnes_core::message::{
    AssistantMessage, Message, TokenUsage, ToolArguments, ToolCall, ToolCallId,
};
use agnes_core::tool::{ToolName, ToolSpec};
use agnes_io::llm::{LlmError, ProtocolError};
use agnes_util::NonEmpty;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::str::FromStr;

/// `POST /chat/completions` request body. Every request asks for a stream;
/// `include_usage` makes the provider report token usage in its final chunk.
#[derive(Debug, serde::Serialize)]
pub struct ChatCompletionRequest {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Debug, serde::Serialize)]
struct StreamOptions {
    include_usage: bool,
}

impl ChatCompletionRequest {
    pub fn from_core(request: &LlmRequest) -> Self {
        Self {
            model: request.model.as_str().to_owned(),
            messages: request
                .prompt
                .input
                .iter()
                .map(WireMessage::from_core)
                .collect(),
            tools: request
                .prompt
                .tools
                .iter()
                .map(WireTool::from_core)
                .collect(),
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        }
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum WireMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        #[serde(skip_serializing_if = "String::is_empty")]
        content: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<WireToolCall>,
    },
    #[serde(rename = "tool")]
    ToolResult {
        tool_call_id: String,
        content: String,
    },
}

impl WireMessage {
    fn from_core(message: &Message) -> Self {
        match message {
            Message::System { content } => Self::System {
                content: content.as_str().to_owned(),
            },
            Message::User { content } => Self::User {
                content: content.as_str().to_owned(),
            },
            Message::Assistant(assistant) => match assistant {
                AssistantMessage::Text(content) => Self::Assistant {
                    content: content.as_str().to_owned(),
                    tool_calls: Vec::new(),
                },
                AssistantMessage::ToolCalls { content, calls } => Self::Assistant {
                    content: content
                        .as_ref()
                        .map(|content| content.as_str().to_owned())
                        .unwrap_or_default(),
                    tool_calls: calls.iter().map(WireToolCall::from_core).collect(),
                },
            },
            Message::ToolResult { call_id, output } => Self::ToolResult {
                tool_call_id: call_id.as_str().to_owned(),
                content: output.content().to_owned(),
            },
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction,
}

impl WireToolCall {
    fn from_core(call: &ToolCall) -> Self {
        Self {
            id: call.id.as_str().to_owned(),
            kind: "function",
            function: WireFunction {
                name: call.name.as_str().to_owned(),
                arguments: call.arguments.as_str().to_owned(),
            },
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct WireFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, serde::Serialize)]
pub struct WireTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunctionSpec,
}

impl WireTool {
    fn from_core(spec: &ToolSpec) -> Self {
        Self {
            kind: "function",
            function: WireFunctionSpec {
                name: spec.name().as_str().to_owned(),
                description: spec.description().to_owned(),
                parameters: spec.parameters().as_value().clone(),
            },
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct WireFunctionSpec {
    name: String,
    description: String,
    parameters: Value,
}

/// `POST /chat/completions` response body. OpenAI-compatible servers add
/// extra fields freely, so unknown fields are tolerated and required fields
/// are validated into typed errors.
#[derive(Debug, Deserialize)]
pub struct ChatCompletionResponse {
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
pub struct WireChoice {
    message: WireAssistantMessage,
    #[serde(default)]
    finish_reason: Option<WireFinishReason>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireFinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    FunctionCall,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct WireAssistantMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCallOut>,
}

#[derive(Debug, Deserialize)]
pub struct WireToolCallOut {
    id: String,
    function: WireFunctionOut,
}

#[derive(Debug, Deserialize)]
pub struct WireFunctionOut {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
pub struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

impl ChatCompletionResponse {
    pub fn into_core(self) -> Result<LlmResponse, LlmError> {
        let choice = self
            .choices
            .into_iter()
            .next()
            .ok_or(ProtocolError::NoChoices)?;
        let WireChoice {
            message,
            finish_reason,
        } = choice;
        let WireAssistantMessage {
            content: raw_content,
            tool_calls: raw_calls,
        } = message;
        let finish = finish_reason.ok_or(ProtocolError::MissingFinishReason)?;

        let stop_reason = match finish {
            WireFinishReason::Stop => StopReason::Completed {
                content: required_content(raw_content)?,
            },
            WireFinishReason::ToolCalls => {
                let mut calls = Vec::with_capacity(raw_calls.len());
                for call in raw_calls {
                    if call.id.is_empty() {
                        return Err(ProtocolError::EmptyToolCallId.into());
                    }
                    let name = ToolName::from_str(&call.function.name)
                        .map_err(ProtocolError::InvalidToolName)?;
                    calls.push(ToolCall {
                        id: ToolCallId::new(call.id),
                        name,
                        arguments: ToolArguments::new(call.function.arguments),
                    });
                }
                let calls = NonEmpty::from_vec(calls).map_err(|_| ProtocolError::EmptyToolCalls)?;
                StopReason::ToolUse {
                    content: optional_content(raw_content)?,
                    calls,
                }
            }
            WireFinishReason::Length => StopReason::LengthExceeded {
                content: optional_content(raw_content)?,
            },
            WireFinishReason::ContentFilter => StopReason::ContentFiltered {
                content: optional_content(raw_content)?,
            },
            WireFinishReason::FunctionCall | WireFinishReason::Unknown => {
                return Err(ProtocolError::UnknownFinishReason.into());
            }
        };
        Ok(LlmResponse {
            stop_reason,
            usage: self
                .usage
                .map(|u| TokenUsage {
                    input: u.prompt_tokens,
                    output: u.completion_tokens,
                })
                .unwrap_or_default(),
        })
    }
}

fn required_content(raw: Option<String>) -> Result<Content, LlmError> {
    let text = raw.ok_or(ProtocolError::MissingContent)?;
    Content::try_from(text).map_err(|_| ProtocolError::MissingContent.into())
}

/// Optional content may be null or whitespace-only; both mean "no text".
fn optional_content(raw: Option<String>) -> Result<Option<Content>, LlmError> {
    match raw {
        None => Ok(None),
        Some(text) => Ok(Content::try_from(text).ok()),
    }
}

/// One incremental update produced while decoding a streamed response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamDelta {
    /// Assistant text produced by this update.
    Text(String),
    /// The provider reported usage for the whole response.
    Usage(TokenUsage),
}

/// Incremental decoder for the OpenAI chat-completions SSE stream.
///
/// Text deltas are emitted as they arrive; tool calls and the finish reason
/// are accumulated until [`OpenAiStreamDecoder::finish`] validates them with
/// the same rules as a buffered response.
#[derive(Debug, Default)]
pub struct OpenAiStreamDecoder {
    buffer: String,
    content: String,
    tool_calls: BTreeMap<u32, AccumulatedToolCall>,
    finish_reason: Option<WireFinishReason>,
    usage: Option<TokenUsage>,
    done: bool,
}

#[derive(Debug, Default)]
struct AccumulatedToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl OpenAiStreamDecoder {
    /// Creates a decoder positioned before the first event.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one UTF-8 body chunk and returns the updates it completed.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Decode`] when a `data:` line is not valid JSON for
    /// the chunk shape.
    pub fn feed(&mut self, chunk: &str) -> Result<Vec<StreamDelta>, LlmError> {
        if self.done {
            return Ok(Vec::new());
        }
        self.buffer.push_str(chunk);
        let mut deltas = Vec::new();
        while let Some(end) = self.buffer.find('\n') {
            let line = self.buffer[..end].trim_end_matches('\r').to_owned();
            self.buffer.drain(..=end);
            self.handle_line(&line, &mut deltas)?;
        }
        Ok(deltas)
    }

    /// Validates the accumulated response.
    ///
    /// A final event without a trailing newline is still processed. The
    /// response must carry a finish reason; text, calls and usage follow the
    /// same rules as [`OpenAiCodec::parse_response`](crate::OpenAiCodec::parse_response).
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Decode`] for malformed JSON and [`LlmError::Protocol`]
    /// for a stream that does not describe a legal completion.
    pub fn finish(mut self) -> Result<LlmResponse, LlmError> {
        if !self.done {
            let tail = std::mem::take(&mut self.buffer);
            let tail = tail.trim();
            if !tail.is_empty() {
                let mut deltas = Vec::new();
                self.handle_line(tail, &mut deltas)?;
            }
        }
        let choice = WireChoice {
            message: WireAssistantMessage {
                content: (!self.content.is_empty()).then_some(self.content),
                tool_calls: self
                    .tool_calls
                    .into_values()
                    .map(|call| WireToolCallOut {
                        id: call.id,
                        function: WireFunctionOut {
                            name: call.name,
                            arguments: call.arguments,
                        },
                    })
                    .collect(),
            },
            finish_reason: self.finish_reason,
        };
        ChatCompletionResponse {
            choices: vec![choice],
            usage: self.usage.map(|usage| WireUsage {
                prompt_tokens: usage.input,
                completion_tokens: usage.output,
            }),
        }
        .into_core()
    }

    fn handle_line(&mut self, line: &str, deltas: &mut Vec<StreamDelta>) -> Result<(), LlmError> {
        let line = line.trim();
        if line.is_empty() || line.starts_with(':') {
            return Ok(());
        }
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let data = data.strip_prefix(' ').unwrap_or(data);
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        if data.is_empty() {
            return Ok(());
        }
        let chunk: ChatCompletionChunk =
            serde_json::from_str(data).map_err(|err| LlmError::Decode(Box::new(err)))?;

        if let Some(usage) = chunk.usage {
            let usage = TokenUsage {
                input: usage.prompt_tokens,
                output: usage.completion_tokens,
            };
            self.usage = Some(usage);
            deltas.push(StreamDelta::Usage(usage));
        }

        for choice in chunk.choices {
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
            if let Some(content) = choice.delta.content
                && !content.is_empty()
            {
                self.content.push_str(&content);
                deltas.push(StreamDelta::Text(content));
            }
            for call in choice.delta.tool_calls {
                let slot = self.tool_calls.entry(call.index).or_default();
                if let Some(id) = call.id
                    && !id.is_empty()
                {
                    slot.id = id;
                }
                if let Some(function) = call.function {
                    if let Some(name) = function.name {
                        slot.name.push_str(&name);
                    }
                    if let Some(arguments) = function.arguments {
                        slot.arguments.push_str(&arguments);
                    }
                }
            }
        }
        Ok(())
    }
}

/// One `data:` event of the chat-completions stream.
#[derive(Debug, Deserialize)]
struct ChatCompletionChunk {
    #[serde(default)]
    choices: Vec<WireChunkChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct WireChunkChoice {
    #[serde(default)]
    delta: WireDelta,
    #[serde(default)]
    finish_reason: Option<WireFinishReason>,
}

#[derive(Debug, Deserialize, Default)]
struct WireDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct WireToolCallDelta {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<WireFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct WireFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Extracts a human-readable detail from an error body, tolerating arbitrary
/// shapes across OpenAI-compatible servers.
pub fn extract_error_body(body: &str) -> String {
    const MAX_DETAIL_CHARS: usize = 512;

    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "empty error body".to_owned();
    }
    let parsed: Result<Value, _> = serde_json::from_str(trimmed);
    let detail = match parsed {
        Ok(Value::Object(map)) => {
            let error = map.get("error");
            if let Some(Value::Object(inner)) = error
                && let Some(Value::String(message)) = inner.get("message")
            {
                message.clone()
            } else if let Some(Value::String(message)) = map.get("message") {
                message.clone()
            } else {
                trimmed.to_owned()
            }
        }
        _ => trimmed.to_owned(),
    };

    if detail.len() <= MAX_DETAIL_CHARS {
        return detail;
    }
    match detail.char_indices().nth(MAX_DETAIL_CHARS) {
        Some((index, _)) => {
            let mut truncated = detail;
            truncated.truncate(index);
            truncated.push('…');
            truncated
        }
        None => detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::config::{ModelName, SystemPrompt};
    use agnes_core::llm::{LlmRequest, Prompt};
    use agnes_core::message::{AssistantMessage, Message, ToolOutput};

    fn content(text: &str) -> Content {
        Content::try_from(text).unwrap()
    }

    #[test]
    fn empty_request_serializes_without_tools() {
        let request = LlmRequest {
            model: ModelName::from_str("gpt-test").unwrap(),
            prompt: Prompt {
                input: vec![
                    Message::System {
                        content: SystemPrompt::new("sys"),
                    },
                    Message::User {
                        content: content("hi"),
                    },
                ],
                tools: Vec::new(),
            },
        };
        let json = serde_json::to_value(ChatCompletionRequest::from_core(&request)).unwrap();
        let object = json.as_object().unwrap();
        assert_eq!(object["model"], "gpt-test");
        assert!(object.get("tools").is_none());
        assert_eq!(object["messages"][0]["role"], "system");
        assert_eq!(object["messages"][1]["role"], "user");
        assert_eq!(object["stream"], true);
        assert_eq!(object["stream_options"]["include_usage"], true);
    }

    fn decoder() -> OpenAiStreamDecoder {
        OpenAiStreamDecoder::new()
    }

    #[test]
    fn a_stream_yields_text_and_usage_then_the_validated_response() {
        let mut decoder = decoder();
        let deltas = decoder
            .feed(concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
                "data: [DONE]\n\n",
            ))
            .unwrap();
        assert_eq!(
            deltas,
            vec![
                StreamDelta::Text("Hel".to_owned()),
                StreamDelta::Text("lo".to_owned()),
                StreamDelta::Usage(TokenUsage {
                    input: 3,
                    output: 2
                }),
            ]
        );

        let response = decoder.finish().unwrap();
        assert_eq!(
            response.stop_reason,
            StopReason::Completed {
                content: content("Hello")
            }
        );
        assert_eq!(
            response.usage,
            TokenUsage {
                input: 3,
                output: 2
            }
        );
    }

    #[test]
    fn stream_framing_survives_arbitrary_chunk_boundaries() {
        let mut decoder = decoder();
        assert!(decoder.feed("data: {\"choi").unwrap().is_empty());
        let deltas = decoder
            .feed(concat!(
                "ces\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n",
                "\ndata: [DONE]\n\n",
            ))
            .unwrap();
        assert_eq!(deltas, vec![StreamDelta::Text("hi".to_owned())]);
        let response = decoder.finish().unwrap();
        assert_eq!(
            response.stop_reason,
            StopReason::Completed {
                content: content("hi")
            }
        );
    }

    #[test]
    fn streamed_tool_calls_are_accumulated() {
        let mut decoder = decoder();
        decoder
            .feed(concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",",
                "\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
                "\"function\":{\"arguments\":\"{\\\"path\\\":\\\"x\\\"}\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: [DONE]\n\n",
            ))
            .unwrap();
        let response = decoder.finish().unwrap();
        let StopReason::ToolUse { content, calls } = &response.stop_reason else {
            panic!("expected ToolUse");
        };
        assert!(content.is_none());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls.first().id.as_str(), "call_1");
        assert_eq!(calls.first().name.as_str(), "read_file");
        assert_eq!(calls.first().arguments.as_str(), r#"{"path":"x"}"#);
    }

    #[test]
    fn a_stream_without_a_finish_reason_is_malformed() {
        let mut decoder = decoder();
        decoder
            .feed("data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n")
            .unwrap();
        assert!(matches!(
            decoder.finish(),
            Err(LlmError::Protocol(ProtocolError::MissingFinishReason))
        ));
    }

    #[test]
    fn malformed_stream_json_is_a_decode_error() {
        let mut decoder = decoder();
        assert!(matches!(
            decoder.feed("data: not json\n\n"),
            Err(LlmError::Decode(_))
        ));
    }

    #[test]
    fn tool_result_maps_to_wire_role_tool() {
        let message = Message::ToolResult {
            call_id: ToolCallId::new("call-1"),
            output: ToolOutput::success("contents"),
        };
        let json = serde_json::to_value(WireMessage::from_core(&message)).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call-1");
        assert_eq!(json["content"], "contents");
    }

    #[test]
    fn assistant_text_omits_tool_calls() {
        let message = Message::Assistant(AssistantMessage::Text(content("hello")));
        let json = serde_json::to_value(WireMessage::from_core(&message)).unwrap();
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"], "hello");
        assert!(json.get("tool_calls").is_none());
    }

    #[test]
    fn assistant_tool_calls_omit_missing_content() {
        let message = Message::Assistant(AssistantMessage::ToolCalls {
            content: None,
            calls: NonEmpty::from_iter([ToolCall {
                id: ToolCallId::new("call-1"),
                name: ToolName::from_str("read_file").unwrap(),
                arguments: ToolArguments::new("{}"),
            }])
            .unwrap(),
        });
        let json = serde_json::to_value(WireMessage::from_core(&message)).unwrap();
        assert_eq!(json["role"], "assistant");
        assert!(json.get("content").is_none());
        assert_eq!(json["tool_calls"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn full_response_maps_back_to_core() {
        let body = r#"{
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"x\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 11, "completion_tokens": 7 },
            "some_future_field": true
        }"#;
        let parsed: ChatCompletionResponse = serde_json::from_str(body).unwrap();
        let response = parsed.into_core().unwrap();
        assert_eq!(
            response.usage,
            TokenUsage {
                input: 11,
                output: 7
            }
        );
        let StopReason::ToolUse { content, calls } = &response.stop_reason else {
            panic!("expected ToolUse");
        };
        assert!(content.is_none());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls.first().id.as_str(), "call_abc");
        assert_eq!(calls.first().arguments.as_str(), r#"{"path":"x"}"#);
    }

    #[test]
    fn commentary_alongside_tool_calls_is_kept() {
        let body = r#"{
            "choices": [{
                "message": {
                    "content": "let me check",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let parsed: ChatCompletionResponse = serde_json::from_str(body).unwrap();
        let response = parsed.into_core().unwrap();
        let StopReason::ToolUse { content, calls } = &response.stop_reason else {
            panic!("expected ToolUse");
        };
        assert_eq!(content.as_ref().map(Content::as_str), Some("let me check"));
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn stop_without_content_is_malformed() {
        let body =
            r#"{ "choices": [{ "message": { "content": null }, "finish_reason": "stop" }] }"#;
        let parsed: ChatCompletionResponse = serde_json::from_str(body).unwrap();
        assert!(matches!(
            parsed.into_core(),
            Err(LlmError::Protocol(ProtocolError::MissingContent))
        ));

        let body =
            r#"{ "choices": [{ "message": { "content": "   " }, "finish_reason": "stop" }] }"#;
        let parsed: ChatCompletionResponse = serde_json::from_str(body).unwrap();
        assert!(matches!(
            parsed.into_core(),
            Err(LlmError::Protocol(ProtocolError::MissingContent))
        ));
    }

    #[test]
    fn tool_calls_finish_without_calls_is_malformed() {
        let body =
            r#"{ "choices": [{ "message": { "content": null }, "finish_reason": "tool_calls" }] }"#;
        let parsed: ChatCompletionResponse = serde_json::from_str(body).unwrap();
        assert!(matches!(
            parsed.into_core(),
            Err(LlmError::Protocol(ProtocolError::EmptyToolCalls))
        ));
    }

    #[test]
    fn length_without_content_is_allowed() {
        let body =
            r#"{ "choices": [{ "message": { "content": null }, "finish_reason": "length" }] }"#;
        let parsed: ChatCompletionResponse = serde_json::from_str(body).unwrap();
        let response = parsed.into_core().unwrap();
        assert_eq!(
            response.stop_reason,
            StopReason::LengthExceeded { content: None }
        );
    }

    #[test]
    fn no_choices_is_malformed() {
        let parsed: ChatCompletionResponse = serde_json::from_str(r#"{"choices": []}"#).unwrap();
        assert!(matches!(
            parsed.into_core(),
            Err(LlmError::Protocol(ProtocolError::NoChoices))
        ));
    }

    #[test]
    fn unknown_finish_reason_is_rejected() {
        let body =
            r#"{ "choices": [{ "message": { "content": "x" }, "finish_reason": "wednesday" }] }"#;
        let parsed: ChatCompletionResponse = serde_json::from_str(body).unwrap();
        assert!(matches!(
            parsed.into_core(),
            Err(LlmError::Protocol(ProtocolError::UnknownFinishReason))
        ));
    }

    #[test]
    fn a_missing_finish_reason_is_typed() {
        let body = r#"{ "choices": [{ "message": { "content": "x" }, "finish_reason": null }] }"#;
        let parsed: ChatCompletionResponse = serde_json::from_str(body).unwrap();
        assert!(matches!(
            parsed.into_core(),
            Err(LlmError::Protocol(ProtocolError::MissingFinishReason))
        ));
    }

    #[test]
    fn content_filter_is_a_normal_outcome() {
        let body = r#"{
            "choices": [{
                "message": { "content": "partial" },
                "finish_reason": "content_filter"
            }]
        }"#;
        let parsed: ChatCompletionResponse = serde_json::from_str(body).unwrap();
        let response = parsed.into_core().unwrap();
        assert_eq!(
            response.stop_reason,
            StopReason::ContentFiltered {
                content: Some(Content::try_from("partial").unwrap())
            }
        );
    }

    #[test]
    fn error_body_extraction() {
        let detail = extract_error_body(r#"{"error": {"message": "bad key", "type": "auth"}}"#);
        assert_eq!(detail, "bad key");
        let detail = extract_error_body("plain text error");
        assert_eq!(detail, "plain text error");
        let detail = extract_error_body("   ");
        assert_eq!(detail, "empty error body");
    }
}
