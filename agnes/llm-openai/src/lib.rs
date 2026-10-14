//! OpenAI-format adapter for the async [`agnes_ports::llm::Llm`] port.
//!
//! Generic over [`HttpTransport`]: pair it with `agnes-transport-reqwest`
//! (tokio) today or any other async transport tomorrow.
pub mod wire;

use agnes_core::config::{ApiKey, BaseUrl};
use agnes_core::llm::{LlmRequest, LlmResponse};
use agnes_ports::http::{HttpTransport, TransportError};
use agnes_ports::llm::{Llm, LlmError};
use async_trait::async_trait;

/// Client for any OpenAI-compatible chat-completions API.
pub struct OpenAiClient<T> {
    transport: T,
    base_url: BaseUrl,
    api_key: ApiKey,
}

impl<T: HttpTransport> OpenAiClient<T> {
    pub fn new(transport: T, base_url: BaseUrl, api_key: ApiKey) -> Self {
        Self {
            transport,
            base_url,
            api_key,
        }
    }

    fn build_request(&self, request: &LlmRequest) -> Result<http::Request<String>, LlmError> {
        let body = serde_json::to_string(&wire::ChatCompletionRequest::from_core(request))
            .expect("request serialization cannot fail");
        http::Request::builder()
            .method(http::Method::POST)
            .uri(self.base_url.join("/chat/completions"))
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {}", self.api_key.expose()),
            )
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(body)
            .map_err(|e| LlmError::Transport(TransportError::Request(anyhow::Error::new(e))))
    }

    fn parse_response(
        &self,
        status: http::StatusCode,
        body: &str,
    ) -> Result<LlmResponse, LlmError> {
        if !status.is_success() {
            return Err(LlmError::Api {
                status,
                detail: wire::extract_error_body(body),
            });
        }
        let parsed: wire::ChatCompletionResponse =
            serde_json::from_str(body).map_err(|e| LlmError::Malformed {
                detail: format!("invalid JSON: {e}"),
            })?;
        parsed.into_core()
    }
}

#[async_trait]
impl<T: HttpTransport> Llm for OpenAiClient<T> {
    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let http_request = self.build_request(request)?;
        let http_response = self.transport.send(http_request).await?;
        self.parse_response(http_response.status(), http_response.body())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::config::{ModelName, SystemPrompt, TurnBudget};
    use agnes_core::content::Content;
    use agnes_core::llm::Prompt;
    use agnes_core::message::Transcript;
    use agnes_core::turn::{Agent, AgentConfig, ModelTurn};
    use std::num::NonZeroU32;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};

    /// Records the last request and replies with a canned response.
    struct FakeTransport {
        status: Mutex<http::StatusCode>,
        body: Mutex<String>,
    }

    impl FakeTransport {
        fn new(status: u16, body: &str) -> Self {
            Self {
                status: Mutex::new(http::StatusCode::from_u16(status).unwrap()),
                body: Mutex::new(body.to_owned()),
            }
        }
    }

    fn response(status: http::StatusCode, body: &str) -> http::Response<String> {
        http::Response::builder()
            .status(status)
            .body(body.to_owned())
            .unwrap()
    }

    #[async_trait]
    impl HttpTransport for FakeTransport {
        async fn send(
            &self,
            _request: http::Request<String>,
        ) -> Result<http::Response<String>, TransportError> {
            Ok(response(
                *self.status.lock().unwrap(),
                &self.body.lock().unwrap(),
            ))
        }
    }

    fn client(transport: FakeTransport) -> OpenAiClient<FakeTransport> {
        OpenAiClient::new(
            transport,
            BaseUrl::from_str("https://api.example.com/v1").unwrap(),
            ApiKey::new("sk-test").unwrap(),
        )
    }

    fn sample_request() -> LlmRequest {
        LlmRequest {
            model: ModelName::from_str("gpt-test").unwrap(),
            prompt: Prompt {
                input: Transcript::new(
                    SystemPrompt::new("sys"),
                    Content::try_from("do the thing").unwrap(),
                    Vec::new(),
                )
                .into_messages(),
                tools: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn sends_bearer_and_json_headers() {
        let captured: Arc<Mutex<Option<http::Request<String>>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&captured);

        struct Capture(Arc<Mutex<Option<http::Request<String>>>>);
        #[async_trait]
        impl HttpTransport for Capture {
            async fn send(
                &self,
                request: http::Request<String>,
            ) -> Result<http::Response<String>, TransportError> {
                *self.0.lock().unwrap() = Some(request);
                Ok(response(
                    http::StatusCode::OK,
                    r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#,
                ))
            }
        }

        let openai = OpenAiClient::new(
            Capture(Arc::clone(&sink)),
            BaseUrl::from_str("https://api.example.com/v1/").unwrap(),
            ApiKey::new("sk-test").unwrap(),
        );
        Llm::complete(&openai, &sample_request()).await.unwrap();

        let request = captured.lock().unwrap().take().unwrap();
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
    }

    #[tokio::test]
    async fn success_maps_to_llm_response() {
        let openai = client(FakeTransport::new(
            200,
            r#"{"choices":[{"message":{"content":"hello"},"finish_reason":"stop"}],
               "usage":{"prompt_tokens":3,"completion_tokens":2}}"#,
        ));
        let response = Llm::complete(&openai, &sample_request()).await.unwrap();
        let agnes_core::llm::StopReason::Completed { content } = &response.stop_reason else {
            panic!("expected Completed");
        };
        assert_eq!(content.as_str(), "hello");
        assert_eq!(response.usage.input, 3);
    }

    #[tokio::test]
    async fn api_error_surfaces_status_and_detail() {
        let openai = client(FakeTransport::new(
            401,
            r#"{"error":{"message":"invalid api key"}}"#,
        ));
        let err = Llm::complete(&openai, &sample_request()).await.unwrap_err();
        let LlmError::Api { status, .. } = &err else {
            panic!("expected Api error");
        };
        assert_eq!(*status, http::StatusCode::UNAUTHORIZED);
        assert!(err.to_string().contains("invalid api key"));
    }

    #[tokio::test]
    async fn garbage_json_is_malformed() {
        let openai = client(FakeTransport::new(200, "not json at all"));
        assert!(matches!(
            Llm::complete(&openai, &sample_request()).await.unwrap_err(),
            LlmError::Malformed { .. }
        ));
    }

    /// Drives the full state machine through the OpenAI client with a
    /// scripted transport that returns a tool call, then a final answer.
    #[tokio::test]
    async fn scripted_two_turn_conversation() {
        struct Queued {
            turns: Mutex<std::vec::IntoIter<(u16, String)>>,
        }
        #[async_trait]
        impl HttpTransport for Queued {
            async fn send(
                &self,
                _request: http::Request<String>,
            ) -> Result<http::Response<String>, TransportError> {
                let (status, body) = self
                    .turns
                    .lock()
                    .unwrap()
                    .next()
                    .expect("scripted transport ran dry");
                Ok(response(http::StatusCode::from_u16(status).unwrap(), &body))
            }
        }

        let turns: Vec<(u16, String)> = vec![
            (
                200,
                r#"{"choices":[{"message":{"content":null,"tool_calls":[
                    {"id":"call_1","type":"function",
                     "function":{"name":"flag","arguments":"{}"}}
                ]},"finish_reason":"tool_calls"}]}"#
                    .to_owned(),
            ),
            (
                200,
                r#"{"choices":[{"message":{"content":"flipped it"},"finish_reason":"stop"}],
                   "usage":{"prompt_tokens":9,"completion_tokens":4}}"#
                    .to_owned(),
            ),
        ];

        let openai = OpenAiClient::new(
            Queued {
                turns: Mutex::new(turns.into_iter()),
            },
            BaseUrl::from_str("https://api.example.com/v1").unwrap(),
            ApiKey::new("sk-test").unwrap(),
        );

        let config = AgentConfig {
            context: agnes_core::context::Context::new(SystemPrompt::new("sys")),
            goal: Content::try_from("flip").unwrap(),
            tools: Vec::new(),
            budget: TurnBudget::new(NonZeroU32::new(4).unwrap()),
        };
        let model = ModelName::from_str("gpt-test").unwrap();
        let (agent, prompt) = Agent::new(config).start();
        let request = LlmRequest {
            model: model.clone(),
            prompt,
        };
        let response = Llm::complete(&openai, &request).await.unwrap();
        let ModelTurn::NeedTools { agent, batch } = agent.model_responded(response).unwrap() else {
            panic!("expected NeedTools");
        };
        assert_eq!(batch.calls().len(), 1);
        let results = batch
            .complete(vec![agnes_core::message::ToolOutput::success("ok")])
            .unwrap();
        let agnes_core::turn::ToolsTurn::NeedModel { agent, prompt } =
            agent.tools_resolved(results, Vec::new()).unwrap()
        else {
            panic!("expected NeedModel");
        };
        let request = LlmRequest { model, prompt };
        let response = Llm::complete(&openai, &request).await.unwrap();
        let ModelTurn::Done(finished) = agent.model_responded(response).unwrap() else {
            panic!("expected Done");
        };
        assert!(matches!(
            finished.outcome(),
            agnes_core::turn::Outcome::Completed { .. }
        ));
    }
}
