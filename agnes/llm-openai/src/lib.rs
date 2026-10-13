//! OpenAI-format adapter for the async [`agnes_ports::llm::Llm`] port.
//!
//! Generic over [`HttpTransport`]: pair it with `agnes-transport-reqwest`
//! (tokio) today or any other async transport tomorrow.
pub mod wire;

use agnes_core::config::{ApiKey, BaseUrl};
use agnes_core::llm::{LlmRequest, LlmResponse};
use agnes_ports::http::{HttpMethod, HttpRequest, HttpTransport};
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

    fn build_request(&self, request: &LlmRequest) -> HttpRequest {
        HttpRequest {
            method: HttpMethod::Post,
            url: self.base_url.join("/chat/completions"),
            headers: vec![
                (
                    "Authorization".to_owned(),
                    format!("Bearer {}", self.api_key.expose()),
                ),
                ("Content-Type".to_owned(), "application/json".to_owned()),
            ],
            body: serde_json::to_string(&wire::ChatCompletionRequest::from_core(request))
                .expect("request serialization cannot fail"),
        }
    }

    fn parse_response(&self, status: u16, body: &str) -> Result<LlmResponse, LlmError> {
        if !(200..300).contains(&status) {
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
        let http_request = self.build_request(request);
        let http_response = self.transport.send(http_request).await?;
        self.parse_response(http_response.status, &http_response.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::config::{ModelName, SystemPrompt, TurnBudget};
    use agnes_core::machine::{Agent, AgentConfig, ModelTurn};
    use agnes_core::message::Transcript;
    use agnes_ports::http::{HttpResponse, TransportError};
    use std::num::NonZeroU32;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};

    /// Records the last request and replies with a canned response.
    struct FakeTransport {
        status: Mutex<u16>,
        body: Mutex<String>,
    }

    impl FakeTransport {
        fn new(status: u16, body: &str) -> Self {
            Self {
                status: Mutex::new(status),
                body: Mutex::new(body.to_owned()),
            }
        }
    }

    #[async_trait]
    impl HttpTransport for FakeTransport {
        async fn send(&self, _request: HttpRequest) -> Result<HttpResponse, TransportError> {
            Ok(HttpResponse {
                status: *self.status.lock().unwrap(),
                body: self.body.lock().unwrap().clone(),
            })
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
            messages: Transcript::new(SystemPrompt::new("sys"), "do the thing", Vec::new())
                .into_messages(),
            tools: Vec::new(),
        }
    }

    #[tokio::test]
    async fn sends_bearer_and_json_headers() {
        let captured: Arc<Mutex<Option<HttpRequest>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&captured);

        struct Capture(Arc<Mutex<Option<HttpRequest>>>);
        #[async_trait]
        impl HttpTransport for Capture {
            async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
                *self.0.lock().unwrap() = Some(request);
                Ok(HttpResponse {
                    status: 200,
                    body: r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#
                        .to_owned(),
                })
            }
        }

        let openai = OpenAiClient::new(
            Capture(Arc::clone(&sink)),
            BaseUrl::from_str("https://api.example.com/v1/").unwrap(),
            ApiKey::new("sk-test").unwrap(),
        );
        Llm::complete(&openai, &sample_request()).await.unwrap();

        let request = captured.lock().unwrap().take().unwrap();
        assert_eq!(request.url, "https://api.example.com/v1/chat/completions");
        assert!(
            request
                .headers
                .iter()
                .any(|(k, v)| k == "Authorization" && v == "Bearer sk-test")
        );
        assert!(
            request
                .headers
                .iter()
                .any(|(k, v)| k == "Content-Type" && v == "application/json")
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
        assert_eq!(response.assistant.content, "hello");
        assert_eq!(response.usage.input, 3);
    }

    #[tokio::test]
    async fn api_error_surfaces_status_and_detail() {
        let openai = client(FakeTransport::new(
            401,
            r#"{"error":{"message":"invalid api key"}}"#,
        ));
        let err = Llm::complete(&openai, &sample_request()).await.unwrap_err();
        assert!(matches!(err, LlmError::Api { status: 401, .. }));
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
            async fn send(&self, _request: HttpRequest) -> Result<HttpResponse, TransportError> {
                let (status, body) = self
                    .turns
                    .lock()
                    .unwrap()
                    .next()
                    .expect("scripted transport ran dry");
                Ok(HttpResponse { status, body })
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
            goal: "flip".to_owned(),
            model: ModelName::from_str("gpt-test").unwrap(),
            tools: Vec::new(),
            budget: TurnBudget::new(NonZeroU32::new(4).unwrap()),
        };
        let (agent, request) = Agent::new(config).start();
        let response = Llm::complete(&openai, &request).await.unwrap();
        let ModelTurn::NeedTools { agent, batch } = agent.model_responded(response).unwrap() else {
            panic!("expected NeedTools");
        };
        assert_eq!(batch.calls().len(), 1);
        let results = batch
            .complete(vec![agnes_core::message::ToolOutput::success("ok")])
            .unwrap();
        let agnes_core::machine::ToolsTurn::NeedModel { agent, request } =
            agent.tools_resolved(results).unwrap()
        else {
            panic!("expected NeedModel");
        };
        let response = Llm::complete(&openai, &request).await.unwrap();
        let ModelTurn::Done(finished) = agent.model_responded(response).unwrap() else {
            panic!("expected Done");
        };
        assert!(matches!(
            finished.outcome(),
            agnes_core::machine::Outcome::Completed { .. }
        ));
    }
}
