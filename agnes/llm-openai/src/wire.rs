//! Wire types for the OpenAI chat-completions API and mapping to core types.

use agnes_core::llm::{LlmRequest, LlmResponse, StopReason};
use agnes_core::message::{
    AssistantMessage, Message, TokenUsage, ToolArguments, ToolCall, ToolCallId,
};
use agnes_core::tool::{ToolName, ToolSpec};
use agnes_ports::llm::LlmError;
use serde::Deserialize;
use serde_json::Value;
use std::str::FromStr;

/// `POST /chat/completions` request body.
#[derive(Debug, serde::Serialize)]
pub struct ChatCompletionRequest {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
}

impl ChatCompletionRequest {
    pub fn from_core(request: &LlmRequest) -> Self {
        Self {
            model: request.model.as_str().to_owned(),
            messages: request
                .messages
                .iter()
                .map(WireMessage::from_core)
                .collect(),
            tools: request.tools.iter().map(WireTool::from_core).collect(),
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
                content: content.clone(),
            },
            Message::Assistant(assistant) => Self::Assistant {
                content: assistant.content.clone(),
                tool_calls: assistant
                    .tool_calls
                    .iter()
                    .map(WireToolCall::from_core)
                    .collect(),
            },
            Message::ToolResult { call_id, output } => Self::ToolResult {
                tool_call_id: call_id.as_str().to_owned(),
                content: output.content.clone(),
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
                name: spec.name.as_str().to_owned(),
                description: spec.description.clone(),
                parameters: spec.parameters.get().clone(),
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
    finish_reason: String,
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
            .ok_or_else(|| LlmError::Malformed {
                detail: "response contains no choices".to_owned(),
            })?;
        let stop_reason = match choice.finish_reason.as_str() {
            "stop" => StopReason::Completed,
            "tool_calls" => StopReason::ToolUse,
            "length" => StopReason::LengthExceeded,
            other => {
                return Err(LlmError::Malformed {
                    detail: format!("unknown finish_reason `{other}`"),
                });
            }
        };
        let tool_calls = choice
            .message
            .tool_calls
            .into_iter()
            .map(|call| {
                Ok(ToolCall {
                    id: ToolCallId::new(call.id),
                    name: ToolName::from_str(&call.function.name).map_err(|e| {
                        LlmError::Malformed {
                            detail: e.to_string(),
                        }
                    })?,
                    arguments: ToolArguments::new(call.function.arguments),
                })
            })
            .collect::<Result<Vec<_>, LlmError>>()?;
        Ok(LlmResponse {
            assistant: AssistantMessage {
                content: choice.message.content.unwrap_or_default(),
                tool_calls,
            },
            usage: self
                .usage
                .map(|u| TokenUsage {
                    input: u.prompt_tokens,
                    output: u.completion_tokens,
                })
                .unwrap_or_default(),
            stop_reason,
        })
    }
}

/// Extracts a human-readable detail from an error body, tolerating arbitrary
/// shapes across OpenAI-compatible servers.
pub fn extract_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "empty error body".to_owned();
    }
    let parsed: Result<Value, _> = serde_json::from_str(trimmed);
    match parsed {
        Ok(Value::Object(map)) => {
            let error = map.get("error");
            if let Some(Value::Object(inner)) = error {
                if let Some(Value::String(message)) = inner.get("message") {
                    return message.clone();
                }
            }
            if let Some(Value::String(message)) = map.get("message") {
                return message.clone();
            }
            trimmed.to_owned()
        }
        _ => trimmed.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::config::{ModelName, SystemPrompt};
    use agnes_core::llm::LlmRequest;
    use agnes_core::message::{ToolOutput, Transcript};

    #[test]
    fn empty_request_serializes_without_tools() {
        let request = LlmRequest {
            model: ModelName::from_str("gpt-test").unwrap(),
            messages: Transcript::new(SystemPrompt::new("sys"), "hi", Vec::new()).into_messages(),
            tools: Vec::new(),
        };
        let json = serde_json::to_value(ChatCompletionRequest::from_core(&request)).unwrap();
        let object = json.as_object().unwrap();
        assert_eq!(object["model"], "gpt-test");
        assert!(object.get("tools").is_none());
        assert_eq!(object["messages"][0]["role"], "system");
        assert_eq!(object["messages"][1]["role"], "user");
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
    fn assistant_without_content_omits_empty_fields() {
        let message = Message::Assistant(AssistantMessage {
            content: String::new(),
            tool_calls: vec![],
        });
        let json = serde_json::to_value(WireMessage::from_core(&message)).unwrap();
        assert_eq!(json["role"], "assistant");
        assert!(json.get("content").is_none());
        assert!(json.get("tool_calls").is_none());
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
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(
            response.usage,
            TokenUsage {
                input: 11,
                output: 7
            }
        );
        assert_eq!(response.assistant.content, "");
        assert_eq!(response.assistant.tool_calls.len(), 1);
        assert_eq!(response.assistant.tool_calls[0].id.as_str(), "call_abc");
        assert_eq!(
            response.assistant.tool_calls[0].arguments.as_str(),
            r#"{"path":"x"}"#
        );
    }

    #[test]
    fn no_choices_is_malformed() {
        let parsed: ChatCompletionResponse = serde_json::from_str(r#"{"choices": []}"#).unwrap();
        assert!(matches!(
            parsed.into_core(),
            Err(LlmError::Malformed { .. })
        ));
    }

    #[test]
    fn unknown_finish_reason_is_malformed() {
        let body =
            r#"{ "choices": [{ "message": { "content": "x" }, "finish_reason": "wednesday" }] }"#;
        let parsed: ChatCompletionResponse = serde_json::from_str(body).unwrap();
        assert!(matches!(
            parsed.into_core(),
            Err(LlmError::Malformed { .. })
        ));
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
