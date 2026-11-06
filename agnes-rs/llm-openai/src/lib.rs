//! OpenAI-format codec: request encoding and response decoding for the
//! `chat/completions` wire format.
//!
//! Pure data in, pure data out — no I/O, no transport. The ECS bridge in
//! `agnes-plugin-openai` pairs it with the HTTP capability: `LlmCall` →
//! [`OpenAiCodec::build_request`] → `HttpStreamRequest` → [`OpenAiStreamDecoder`]
//! → `LlmReply`. Requests always ask for a stream; the decoder emits text
//! deltas as they arrive and validates the accumulated response at the end.
mod wire;

pub use wire::{OpenAiStreamDecoder, StreamDelta};

use agnes_core::config::{ApiKey, BaseUrl, ModelName};
use agnes_core::llm::{LlmRequest, LlmResponse};
use agnes_io::http::TransportError;
use agnes_io::llm::{ApiBody, LlmError};

/// Encodes the runtime's requests and decodes the provider's responses.
pub struct OpenAiCodec {
    base_url: BaseUrl,
    api_key: ApiKey,
}

impl OpenAiCodec {
    pub fn new(base_url: BaseUrl, api_key: ApiKey) -> Self {
        Self { base_url, api_key }
    }

    /// Encodes one model request as an HTTP request.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Config`] when the endpoint cannot be joined and
    /// [`LlmError::Transport`] when the `http` crate rejects the request.
    ///
    /// # Panics
    ///
    /// Panics when the request body cannot be serialized (it is built from
    /// owned domain values, so this cannot happen in practice).
    pub fn build_request(&self, request: &LlmRequest) -> Result<http::Request<String>, LlmError> {
        let body = serde_json::to_string(&wire::ChatCompletionRequest::from_core(request))
            .expect("request serialization cannot fail");
        let endpoint = self.base_url.join("chat/completions")?;
        http::Request::builder()
            .method(http::Method::POST)
            .uri(endpoint.as_str())
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {}", self.api_key.expose()),
            )
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(body)
            .map_err(|e| LlmError::Transport(TransportError::Request(e)))
    }

    /// Decodes the provider's HTTP response.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Api`] for non-success statuses, [`LlmError::Decode`]
    /// for malformed JSON, and [`LlmError::Protocol`] for a completion that
    /// parses but is not a legal response.
    pub fn parse_response(status: http::StatusCode, body: &str) -> Result<LlmResponse, LlmError> {
        if !status.is_success() {
            let detail = wire::extract_error_body(body);
            return Err(LlmError::Api {
                status,
                body: ApiBody::new(body, detail),
            });
        }
        let parsed: wire::ChatCompletionResponse =
            serde_json::from_str(body).map_err(|err| LlmError::Decode(err.into()))?;
        parsed.into_core()
    }

    /// Encodes a `GET /models` request. The list is small, so it uses the
    /// buffered HTTP path instead of the streaming one.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Config`] when the endpoint cannot be joined and
    /// [`LlmError::Transport`] when the `http` crate rejects the request.
    pub fn build_models_request(&self) -> Result<http::Request<String>, LlmError> {
        let endpoint = self.base_url.join("models")?;
        http::Request::builder()
            .method(http::Method::GET)
            .uri(endpoint.as_str())
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {}", self.api_key.expose()),
            )
            .header(http::header::ACCEPT, "application/json")
            .body(String::new())
            .map_err(|e| LlmError::Transport(TransportError::Request(e)))
    }

    /// Decodes a `GET /models` response into sorted, deduplicated names.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Api`] for non-success statuses and
    /// [`LlmError::Decode`] for a body that is not a model list.
    pub fn parse_models_response(
        status: http::StatusCode,
        body: &str,
    ) -> Result<Vec<ModelName>, LlmError> {
        if !status.is_success() {
            let detail = wire::extract_error_body(body);
            return Err(LlmError::Api {
                status,
                body: ApiBody::new(body, detail),
            });
        }
        wire::parse_model_list(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::config::{ModelName, ReasoningEffort, SystemPrompt, Thinking};
    use agnes_core::content::Content;
    use agnes_core::llm::{Prompt, StopReason};
    use agnes_core::message::Message;
    use std::num::NonZeroU32;
    use std::str::FromStr;

    fn codec() -> OpenAiCodec {
        OpenAiCodec::new(
            BaseUrl::from_str("https://api.example.com/v1").unwrap(),
            ApiKey::try_from("sk-test").unwrap(),
        )
    }

    fn sample_request() -> LlmRequest {
        LlmRequest {
            model: ModelName::from_str("gpt-test").unwrap(),
            thinking: Thinking::Off,
            prompt: Prompt {
                input: vec![
                    Message::System {
                        content: SystemPrompt::new("sys"),
                    },
                    Message::User {
                        content: Content::try_from("do the thing").unwrap(),
                    },
                ],
                tools: Vec::new(),
            },
        }
    }

    fn decode(status: u16, body: &str) -> Result<LlmResponse, LlmError> {
        OpenAiCodec::parse_response(http::StatusCode::from_u16(status).unwrap(), body)
    }

    #[test]
    fn sends_bearer_and_json_headers() {
        let request = codec().build_request(&sample_request()).unwrap();
        assert_eq!(
            request.uri().to_string(),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            request.headers().get(http::header::AUTHORIZATION).unwrap(),
            "Bearer sk-test"
        );
        assert_eq!(
            request.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body: serde_json::Value = serde_json::from_str(request.body()).unwrap();
        assert_eq!(body["model"], "gpt-test");
    }

    #[test]
    fn reasoning_effort_is_omitted_when_thinking_is_off() {
        let request = codec().build_request(&sample_request()).unwrap();
        let body: serde_json::Value = serde_json::from_str(request.body()).unwrap();
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn reasoning_effort_follows_the_thinking_setting() {
        for (effort, wire) in [
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
        ] {
            let mut request = sample_request();
            request.thinking = Thinking::On(effort);
            let encoded = codec().build_request(&request).unwrap();
            let body: serde_json::Value = serde_json::from_str(encoded.body()).unwrap();
            assert_eq!(body["reasoning_effort"], wire);
        }
    }

    #[test]
    fn models_request_is_a_bearer_get() {
        let request = codec().build_models_request().unwrap();
        assert_eq!(request.method(), http::Method::GET);
        assert_eq!(
            request.uri().to_string(),
            "https://api.example.com/v1/models"
        );
        assert_eq!(
            request.headers().get(http::header::AUTHORIZATION).unwrap(),
            "Bearer sk-test"
        );
        assert_eq!(
            request.headers().get(http::header::ACCEPT).unwrap(),
            "application/json"
        );
        assert!(request.body().is_empty());
    }

    #[test]
    fn model_list_parses_wrapped_and_bare_bodies() {
        let models = OpenAiCodec::parse_models_response(
            http::StatusCode::OK,
            r#"{"object":"list","data":[{"id":"b"},{"id":"a"},{"id":"a"},{"id":" "}]}"#,
        )
        .unwrap();
        let names: Vec<&str> = models.iter().map(ModelName::as_str).collect();
        assert_eq!(names, ["a", "b"]);

        let models =
            OpenAiCodec::parse_models_response(http::StatusCode::OK, r#"[{"id":"z"},{"id":"m"}]"#)
                .unwrap();
        let names: Vec<&str> = models.iter().map(ModelName::as_str).collect();
        assert_eq!(names, ["m", "z"]);
    }

    #[test]
    fn model_list_surfaces_api_errors() {
        let err = OpenAiCodec::parse_models_response(
            http::StatusCode::NOT_FOUND,
            r#"{"error":{"message":"no models endpoint"}}"#,
        )
        .unwrap_err();
        let LlmError::Api { status, body } = &err else {
            panic!("expected Api error");
        };
        assert_eq!(*status, http::StatusCode::NOT_FOUND);
        assert!(body.detail().contains("no models endpoint"));
    }

    #[test]
    fn model_list_garbage_is_a_decode_error() {
        assert!(matches!(
            OpenAiCodec::parse_models_response(http::StatusCode::OK, "not json"),
            Err(LlmError::Decode(_))
        ));
        assert!(matches!(
            OpenAiCodec::parse_models_response(http::StatusCode::OK, r#"{"models":[]}"#),
            Err(LlmError::Decode(_))
        ));
    }

    #[test]
    fn success_maps_to_llm_response() {
        let response = decode(
            200,
            r#"{"choices":[{"message":{"content":"hello"},"finish_reason":"stop"}],
               "usage":{"prompt_tokens":3,"completion_tokens":2}}"#,
        )
        .unwrap();
        let StopReason::Completed { content } = &response.stop_reason else {
            panic!("expected Completed");
        };
        assert_eq!(content.as_str(), "hello");
        assert_eq!(response.usage.input, 3);
    }

    #[test]
    fn api_error_surfaces_status_and_detail() {
        let err = decode(401, r#"{"error":{"message":"invalid api key"}}"#).unwrap_err();
        let LlmError::Api { status, body } = &err else {
            panic!("expected Api error");
        };
        assert_eq!(*status, http::StatusCode::UNAUTHORIZED);
        assert_eq!(body.raw(), r#"{"error":{"message":"invalid api key"}}"#);
        assert!(body.detail().contains("invalid api key"));
        assert!(err.to_string().contains("invalid api key"));
    }

    #[test]
    fn garbage_json_is_malformed() {
        assert!(matches!(
            decode(200, "not json at all").unwrap_err(),
            LlmError::Decode(_)
        ));
    }

    /// Drives the full state machine through the codec with scripted
    /// responses that return a tool call, then a final answer.
    #[test]
    fn scripted_two_turn_conversation() {
        let turn = agnes_core::turn::Turn::new(
            agnes_core::header::Header::new(
                SystemPrompt::new("sys"),
                agnes_core::header::Environment::default(),
            ),
            agnes_core::config::StepBudget::new(NonZeroU32::new(4).unwrap()),
        );
        let model = ModelName::from_str("gpt-test").unwrap();
        let agent =
            agnes_core::agent::Agent::with_goal(turn, Content::try_from("flip").unwrap()).start();

        let request = LlmRequest {
            model: model.clone(),
            thinking: Thinking::Off,
            prompt: agent.prompt(),
        };
        codec()
            .build_request(&request)
            .expect("the tool-round request encodes");

        let agent = agent.call();
        let response = decode(
            200,
            r#"{"choices":[{"message":{"content":null,"tool_calls":[
                {"id":"call_1","type":"function",
                 "function":{"name":"flag","arguments":"{}"}}
            ]},"finish_reason":"tool_calls"}]}"#,
        )
        .unwrap();
        let agnes_core::agent::Replied::Tools(agent) = agent.respond(response) else {
            panic!("expected Tools");
        };
        let calls: Vec<_> = agent.calls().cloned().collect();
        assert!(
            matches!(&calls[..], [tool] if tool.id.as_str() == "call_1" && tool.name.as_str() == "flag"),
            "第一个响应应该派发 flag 调用"
        );

        let call = calls.into_iter().next().unwrap();
        let agnes_core::agent::Advanced::Next(agent) =
            agent.finish(call.id, agnes_core::message::ToolOutput::success("ok"))
        else {
            panic!("expected Next");
        };
        let request = LlmRequest {
            model: model.clone(),
            thinking: Thinking::Off,
            prompt: agent.prompt(),
        };
        codec()
            .build_request(&request)
            .expect("the final request encodes");

        let agent = agent.call();
        let response = decode(
            200,
            r#"{"choices":[{"message":{"content":"flipped it"},"finish_reason":"stop"}],
               "usage":{"prompt_tokens":9,"completion_tokens":4}}"#,
        )
        .unwrap();
        let agnes_core::agent::Replied::Closed(agent) = agent.respond(response) else {
            panic!("expected Closed");
        };
        assert_eq!(agent.outcome(), agnes_core::turn::Outcome::Answered);
        assert!(matches!(
            agent.turn().messages().last(),
            Some(Message::Assistant(agnes_core::message::AssistantMessage::Text(content)))
                if content.as_str() == "flipped it"
        ));
    }
}
