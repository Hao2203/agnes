//! The OpenAI-compatible provider as an ECS message bridge.
//!
//! [`OpenAiPlugin`] connects the runtime's [`LlmCall`] / [`LlmCancel`] messages
//! to the HTTP capability installed by `agnes-plugin-http`: it encodes each
//! call with [`OpenAiCodec`], submits it as a [`HttpStreamRequest`], decodes
//! the streamed body with [`OpenAiStreamDecoder`] — forwarding text deltas and
//! usage as [`LlmStreamEvent`]s — and closes the call with an [`LlmReply`].
//! Each hop is one message and therefore costs one schedule tick; the runner
//! drains consecutive ticks, so the latency is not observable.
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
use agnes_io::http::{HttpStreamPart, TransportError};
use agnes_io::llm::{
    LlmCall, LlmCallId, LlmCancel, LlmError, LlmProvider, LlmReply, LlmStreamEvent, LlmStreamUpdate,
};
use agnes_llm_openai::{OpenAiCodec, OpenAiStreamDecoder, StreamDelta};
use agnes_plugin_http::{
    HttpCancel, HttpClient, HttpRequestId, HttpStreamEnd, HttpStreamEvent, HttpStreamRequest,
};

/// The most error-body text kept for an eventual `Api` error. Error bodies
/// are diagnostic; a hostile peer must not grow them without bound.
const MAX_ERROR_BODY_CHARS: usize = 64 * 1024;

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

/// In-flight model calls: the runtime's id, the HTTP stream performing it,
/// and the decoder accumulating that stream.
#[derive(Default)]
pub struct OpenAiBridge {
    pending: HashMap<LlmCallId, HttpRequestId>,
    streams: HashMap<HttpRequestId, StreamState>,
}

struct StreamState {
    call: LlmCallId,
    status: Option<http::StatusCode>,
    decoder: OpenAiStreamDecoder,
    error_body: String,
    failure: Option<LlmError>,
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
            .add_systems(Update, ingest_stream_events)
            .add_systems(Update, ingest_stream_ends);
    }
}

/// Encodes calls and submits them as streaming HTTP requests; encoding
/// failures answer immediately.
fn dispatch_llm_calls(
    codec: Res<OpenAiCodec>,
    mut client: ResMut<HttpClient>,
    mut bridge: ResMut<OpenAiBridge>,
    mut calls: MessageReader<LlmCall>,
    mut requests: MessageWriter<HttpStreamRequest>,
    mut replies: MessageWriter<LlmReply>,
) {
    for call in calls.read() {
        match codec.build_request(&call.request) {
            Ok(request) => {
                let id = client.request_id();
                bridge.pending.insert(call.id, id);
                bridge.streams.insert(
                    id,
                    StreamState {
                        call: call.id,
                        status: None,
                        decoder: OpenAiStreamDecoder::new(),
                        error_body: String::new(),
                        failure: None,
                    },
                );
                requests.write(HttpStreamRequest { id, request });
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

/// Consumes stream parts: success bodies feed the decoder and become
/// [`LlmStreamEvent`]s; error bodies are kept for the eventual `Api` error.
fn ingest_stream_events(
    mut bridge: ResMut<OpenAiBridge>,
    mut events: MessageReader<HttpStreamEvent>,
    mut updates: MessageWriter<LlmStreamEvent>,
) {
    for event in events.read() {
        let Some(stream) = bridge.streams.get_mut(&event.id) else {
            continue;
        };
        let call = stream.call;
        match &event.part {
            HttpStreamPart::Head { status, .. } => {
                stream.status = Some(*status);
                if status.is_success() {
                    updates.write(LlmStreamEvent {
                        id: call,
                        update: LlmStreamUpdate::Started,
                    });
                }
            }
            HttpStreamPart::Body(text) => {
                if stream.failure.is_some() {
                    continue;
                }
                if stream.status.is_some_and(|status| status.is_success()) {
                    match stream.decoder.feed(text) {
                        Ok(deltas) => {
                            for delta in deltas {
                                let update = match delta {
                                    StreamDelta::Text(text) => LlmStreamUpdate::Delta { text },
                                    StreamDelta::Usage(usage) => LlmStreamUpdate::Usage { usage },
                                };
                                updates.write(LlmStreamEvent { id: call, update });
                            }
                        }
                        Err(error) => stream.failure = Some(error),
                    }
                } else if stream.error_body.chars().count() < MAX_ERROR_BODY_CHARS {
                    let remaining = MAX_ERROR_BODY_CHARS - stream.error_body.chars().count();
                    stream.error_body.extend(text.chars().take(remaining));
                }
            }
        }
    }
}

/// Turns the end of a stream into the one reply the runtime consumes.
fn ingest_stream_ends(
    mut bridge: ResMut<OpenAiBridge>,
    mut ends: MessageReader<HttpStreamEnd>,
    mut replies: MessageWriter<LlmReply>,
) {
    for end in ends.read() {
        let Some(stream) = bridge.streams.remove(&end.id) else {
            continue;
        };
        bridge.pending.remove(&stream.call);
        let StreamState {
            call,
            status,
            decoder,
            error_body,
            failure,
        } = stream;
        let result = match &end.result {
            Err(error) => Err(LlmError::Transport(to_transport_error(error))),
            Ok(()) => match status {
                None => Err(LlmError::Transport(TransportError::Other(
                    "the stream ended before the response head".into(),
                ))),
                Some(status) if !status.is_success() => {
                    OpenAiCodec::parse_response(status, &error_body)
                }
                Some(_) => match failure {
                    Some(error) => Err(error),
                    None => decoder.finish(),
                },
            },
        };
        replies.write(LlmReply { id: call, result });
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

    const SSE_BODY: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"o\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"k\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );

    struct StreamingTransport;

    #[async_trait]
    impl HttpTransport for StreamingTransport {
        async fn send(
            &self,
            _request: http::Request<String>,
        ) -> Result<http::Response<String>, TransportError> {
            unreachable!("the bridge always streams")
        }

        async fn send_stream(
            &self,
            _request: http::Request<String>,
            sink: &mut (dyn FnMut(HttpStreamPart) + Send),
        ) -> Result<(), TransportError> {
            sink(HttpStreamPart::Head {
                status: http::StatusCode::OK,
                headers: http::HeaderMap::new(),
            });
            for line in SSE_BODY.split_inclusive("\n\n") {
                sink(HttpStreamPart::Body(line.to_owned()));
            }
            Ok(())
        }
    }

    struct ErrorTransport;

    #[async_trait]
    impl HttpTransport for ErrorTransport {
        async fn send(
            &self,
            _request: http::Request<String>,
        ) -> Result<http::Response<String>, TransportError> {
            unreachable!("the bridge always streams")
        }

        async fn send_stream(
            &self,
            _request: http::Request<String>,
            sink: &mut (dyn FnMut(HttpStreamPart) + Send),
        ) -> Result<(), TransportError> {
            sink(HttpStreamPart::Head {
                status: http::StatusCode::UNAUTHORIZED,
                headers: http::HeaderMap::new(),
            });
            sink(HttpStreamPart::Body(
                r#"{"error":{"message":"invalid api key"}}"#.to_owned(),
            ));
            Ok(())
        }
    }

    #[derive(Default, Clone)]
    struct Captured {
        replies: Vec<Result<String, String>>,
        deltas: Vec<String>,
        usage: Vec<(u64, u64)>,
        started: usize,
    }

    fn capture_replies(
        mut replies: MessageReader<LlmReply>,
        mut updates: MessageReader<LlmStreamEvent>,
        mut captured: ResMut<Captured>,
    ) {
        for update in updates.read() {
            match &update.update {
                LlmStreamUpdate::Started => captured.started += 1,
                LlmStreamUpdate::Delta { text } => captured.deltas.push(text.clone()),
                LlmStreamUpdate::Usage { usage } => {
                    captured.usage.push((usage.input, usage.output));
                }
            }
        }
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

    fn app_with(transport: Arc<dyn HttpTransport>) -> App {
        let mut app = App::new();
        app.add_plugin(agnes_plugin_http::HttpPlugin::new(transport))
            .add_plugin(OpenAiPlugin::new(
                BaseUrl::from_str("https://api.example.com/v1").expect("static url is valid"),
                ApiKey::from_str("sk-test").expect("static key is valid"),
            ))
            .insert_resource(Captured::default())
            .add_systems(Update, capture_replies)
            .add_plugin(TokioPlugin);
        app
    }

    async fn run_call(app: &mut App) -> Captured {
        app.world_mut().send_message(sample_call());
        for _ in 0..256 {
            app.pump().expect("schedule runs");
            tokio::task::yield_now().await;
            let captured = app
                .world()
                .resource::<Captured>()
                .expect("captured is installed")
                .clone();
            if !captured.replies.is_empty() {
                return captured;
            }
        }
        panic!("no reply after 256 pumps");
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
    async fn a_model_call_streams_deltas_and_ends_with_a_reply() {
        let mut app = app_with(Arc::new(StreamingTransport));
        let captured = run_call(&mut app).await;

        assert_eq!(captured.started, 1);
        assert_eq!(captured.deltas, vec!["o".to_owned(), "k".to_owned()]);
        assert_eq!(captured.usage, vec![(3, 2)]);
        assert_eq!(captured.replies, vec![Ok("ok".to_owned())]);
    }

    #[tokio::test]
    async fn an_error_status_surfaces_the_buffered_body() {
        let mut app = app_with(Arc::new(ErrorTransport));
        let captured = run_call(&mut app).await;

        assert!(captured.started == 0);
        assert!(captured.deltas.is_empty());
        assert!(
            matches!(&captured.replies[..], [Err(message)] if message.contains("invalid api key")),
            "the api error body is decoded"
        );
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
