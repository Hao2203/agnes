//! The OpenAI-compatible provider as an ECS message bridge.
//!
//! [`OpenAiPlugin`] connects the runtime's [`LlmCall`] / [`LlmCancel`] messages
//! to the HTTP capability installed by `agnes-plugin-http`: it encodes each
//! call with [`OpenAiCodec`], submits it as a [`HttpRequest`], and turns the
//! matching [`HttpResponse`] back into an [`LlmReply`]. Each hop is one
//! message and therefore costs one schedule tick; the runner drains
//! consecutive ticks, so the latency is not observable.
//!
//! Plugins apply immediately in registration order: add
//! `agnes_plugin_http::HttpPlugin` **before** this plugin. The bridge submits
//! through the installed [`HttpClient`] and panics without it — a missing
//! HTTP capability fails loudly instead of letting requests vanish. To use a
//! different provider, write a sibling plugin that speaks the same
//! `LlmCall` / `LlmReply` vocabulary and installs [`LlmProvider`].

use std::collections::HashMap;

use agnes_core::config::{ApiKey, BaseUrl};
use agnes_ecs::{App, MessageReader, MessageWriter, Plugin, Res, ResMut, Update};
use agnes_io::http::TransportError;
use agnes_io::llm::{LlmCall, LlmCallId, LlmCancel, LlmError, LlmProvider, LlmReply};
use agnes_llm_openai::OpenAiCodec;
use agnes_plugin_http::{HttpCancel, HttpClient, HttpRequest, HttpRequestId, HttpResponse};

/// Builds an OpenAI-compatible provider bridge.
///
/// The model name travels with every request (`RequestBuilder`), not with
/// the codec, so this plugin only needs the endpoint and the key.
pub struct OpenAiPlugin {
    base_url: BaseUrl,
    api_key: ApiKey,
}

impl OpenAiPlugin {
    /// Creates the plugin from endpoint configuration.
    pub fn new(base_url: BaseUrl, api_key: ApiKey) -> Self {
        Self { base_url, api_key }
    }
}

/// In-flight model calls: the runtime's id and the HTTP request performing it.
#[derive(Default)]
pub struct OpenAiBridge {
    pending: HashMap<LlmCallId, HttpRequestId>,
    reverse: HashMap<HttpRequestId, LlmCallId>,
}

impl Plugin for OpenAiPlugin {
    fn name(&self) -> &'static str {
        "agnes-openai"
    }

    /// # Panics
    ///
    /// Panics when the HTTP capability is missing. Add
    /// `agnes_plugin_http::HttpPlugin` before this plugin.
    fn build(&self, app: &mut App) {
        app.world()
            .resource::<HttpClient>()
            .expect("http capability is installed; add `HttpPlugin` first");
        let codec = OpenAiCodec::new(self.base_url.clone(), self.api_key.clone());

        app.insert_resource(codec)
            .insert_resource(OpenAiBridge::default())
            .insert_resource(LlmProvider)
            .add_systems(Update, dispatch_llm_calls)
            .add_systems(Update, dispatch_llm_cancels)
            .add_systems(Update, ingest_http_responses);
    }
}

/// Encodes calls and submits them as HTTP requests; encoding failures answer
/// immediately.
fn dispatch_llm_calls(
    codec: Res<OpenAiCodec>,
    mut client: ResMut<HttpClient>,
    mut bridge: ResMut<OpenAiBridge>,
    mut calls: MessageReader<LlmCall>,
    mut requests: MessageWriter<HttpRequest>,
    mut replies: MessageWriter<LlmReply>,
) {
    for call in calls.read() {
        match codec.build_request(&call.request) {
            Ok(request) => {
                let id = client.request_id();
                bridge.pending.insert(call.id, id);
                bridge.reverse.insert(id, call.id);
                requests.write(HttpRequest { id, request });
            }
            Err(error) => replies.write(LlmReply {
                id: call.id,
                result: Err(error),
            }),
        }
    }
}

/// Forwards cancellations to the HTTP capability (best effort).
fn dispatch_llm_cancels(
    bridge: ResMut<OpenAiBridge>,
    mut cancels: MessageReader<LlmCancel>,
    mut http_cancels: MessageWriter<HttpCancel>,
) {
    for cancel in cancels.read() {
        if let Some(id) = bridge.pending.get(&cancel.id) {
            http_cancels.write(HttpCancel { id: *id });
        }
    }
}

/// Decodes responses into replies and retires the mapping.
fn ingest_http_responses(
    mut bridge: ResMut<OpenAiBridge>,
    mut responses: MessageReader<HttpResponse>,
    mut replies: MessageWriter<LlmReply>,
) {
    for response in responses.read() {
        let Some(id) = bridge.reverse.remove(&response.id) else {
            continue;
        };
        bridge.pending.remove(&id);
        let result = match &response.result {
            Ok(http_response) => {
                OpenAiCodec::parse_response(http_response.status(), http_response.body())
            }
            Err(error) => Err(LlmError::Transport(to_transport_error(error))),
        };
        replies.write(LlmReply { id, result });
    }
}

/// Copies a transport failure out of the shared response message. `Timeout`
/// and `Cancelled` survive as variants; everything else keeps its message.
fn to_transport_error(error: &TransportError) -> TransportError {
    match error {
        TransportError::Timeout => TransportError::Timeout,
        TransportError::Cancelled => TransportError::Cancelled,
        other => TransportError::Other(Box::new(std::io::Error::other(other.to_string()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::config::{ModelName, SystemPrompt};
    use agnes_core::llm::{LlmRequest, Prompt, StopReason};
    use agnes_core::message::Message;
    use agnes_io::http::HttpTransport;
    use agnes_plugin_async::TokioPlugin;
    use async_trait::async_trait;
    use std::str::FromStr;
    use std::sync::Arc;

    struct OkTransport;

    #[async_trait]
    impl HttpTransport for OkTransport {
        async fn send(
            &self,
            _request: http::Request<String>,
        ) -> Result<http::Response<String>, TransportError> {
            Ok(http::Response::builder()
                .status(200)
                .body(
                    r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#
                        .to_owned(),
                )
                .expect("static response is valid"))
        }
    }

    #[derive(Default, Clone)]
    struct Captured {
        replies: Vec<Result<String, String>>,
    }

    fn capture_replies(mut replies: MessageReader<LlmReply>, mut captured: ResMut<Captured>) {
        for reply in replies.read() {
            captured.replies.push(match &reply.result {
                Ok(response) => match &response.stop_reason {
                    StopReason::Completed { content } => Ok(content.as_str().to_owned()),
                    _ => Ok("other".to_owned()),
                },
                Err(error) => Err(error.to_string()),
            });
        }
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugin(agnes_plugin_http::HttpPlugin::new(Arc::new(OkTransport)))
            .add_plugin(OpenAiPlugin::new(
                BaseUrl::from_str("https://api.example.com/v1").expect("static url is valid"),
                ApiKey::from_str("sk-test").expect("static key is valid"),
            ))
            .insert_resource(Captured::default())
            .add_systems(Update, capture_replies)
            .add_plugin(TokioPlugin);
        app
    }

    fn sample_call() -> LlmCall {
        LlmCall {
            id: LlmCallId::new(1),
            request: LlmRequest {
                model: ModelName::from_str("gpt-test").expect("static model is valid"),
                prompt: Prompt {
                    input: vec![Message::System {
                        content: SystemPrompt::new("sys"),
                    }],
                    tools: Vec::new(),
                },
            },
        }
    }

    #[tokio::test]
    async fn a_model_call_travels_to_a_reply() {
        let mut app = app();
        app.world_mut().send_message(sample_call());

        let mut replies = Vec::new();
        for _ in 0..256 {
            app.pump().expect("schedule runs");
            tokio::task::yield_now().await;
            replies = app
                .world()
                .resource::<Captured>()
                .expect("captured is installed")
                .replies
                .clone();
            if !replies.is_empty() {
                break;
            }
        }
        assert_eq!(replies, vec![Ok("ok".to_owned())]);
    }

    #[test]
    fn a_missing_http_capability_fails_the_build() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut app = App::new();
            app.add_plugin(OpenAiPlugin::new(
                BaseUrl::from_str("https://api.example.com/v1").expect("static url is valid"),
                ApiKey::from_str("sk-test").expect("static key is valid"),
            ));
        }));
        assert!(result.is_err());
    }
}
